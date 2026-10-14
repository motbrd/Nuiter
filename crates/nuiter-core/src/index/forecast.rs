//! What indexing takes: estimates from metadata alone (counts, PDF page counts, media durations from the file's header
//! via the Windows property system), with costs learnt on this PC (`CostLog`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::cancel::{Cancel, Cancelled};
use crate::extract::{code_chunker, image, pdf, text_chunker};
use crate::file_kinds::{FileCategory, pathx};
use crate::paths::{Log, Paths};
use crate::uni::ends_with_ignore_case;

/// Seconds of indexing per unit. The defaults are three times what Nuiter's development PC (28 threads, RTX 4090,
/// Vulkan) measured on its own libraries in October 2026, rounded: a typical laptop is slower, and an estimate that
/// comes in early is better than one that keeps slipping. `CostLog` moves each one towards what indexing measures here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnitCosts {
    /// A photo at 140 tokens, decode and OCR probe included.
    pub per_photo: f64,
    /// 280 tokens and full OCR.
    pub per_screenshot: f64,
    /// Text pages are cheap, scans cost a picture.
    pub per_pdf_page: f64,
    pub per_text_chunk: f64,
    pub per_audio_second: f64,
    /// Frames at scene changes plus the soundtrack.
    pub per_video_second: f64,
    pub per_code_piece: f64,
    /// The kinds this PC has measured enough of (a bit per `KINDS` entry). The rest are the defaults: a guess here,
    /// since they come from other hardware.
    pub measured: u8,
}

/// The kinds of units `CostLog` measures, in the order of `UnitCosts::measured`'s bits.
const KINDS: [&str; 7] = ["photo", "screenshot", "pdf-page", "text-chunk", "audio-second", "video-second", "code-piece"];

impl Default for UnitCosts {
    fn default() -> Self {
        Self {
            per_photo: 0.08,
            per_screenshot: 0.09,
            per_pdf_page: 0.05,
            per_text_chunk: 0.08,
            per_audio_second: 0.002,
            per_video_second: 0.05,
            per_code_piece: 0.08,
            measured: 0,
        }
    }
}

impl UnitCosts {
    pub fn for_kind(&self, kind: &str) -> f64 {
        match kind {
            "photo" => self.per_photo,
            "screenshot" => self.per_screenshot,
            "pdf-page" => self.per_pdf_page,
            "text-chunk" => self.per_text_chunk,
            "audio-second" => self.per_audio_second,
            "video-second" => self.per_video_second,
            "code-piece" => self.per_code_piece,
            _ => 0.05,
        }
    }

    /// Whether this PC has measured `kind` (otherwise its cost is the default, a guess).
    pub fn is_measured(&self, kind: &str) -> bool {
        KINDS.iter().position(|k| *k == kind).is_some_and(|i| self.measured & (1 << i) != 0)
    }
}

/// How much there is to index and how long it takes: the first step of every file, and the rest of long PDFs and
/// documents after it.
#[derive(Debug, Clone, Default)]
pub struct Workload {
    pub files: usize,
    /// Files already in the index as they are now (nothing to do for them).
    pub indexed: usize,
    pub by_kind: BTreeMap<FileCategory, usize>,
    pub pdf_pages: usize,
    pub media_seconds: f64,
    pub seconds: f64,
    /// The rest of long PDFs and documents: read once everything else has had its first step.
    pub rest_seconds: f64,
    /// The part of `seconds` + `rest_seconds` from kinds this PC hasn't measured yet: default costs, a guess.
    pub guessed_seconds: f64,
    /// For code search: about how big its part of the index gets on disk.
    pub bytes: f64,
}

/// A long file's first step takes about this long; it is searchable after it.
pub const FIRST_STEP_SECONDS: f64 = 8.0;
/// Each later step of a long file takes about this long (they come after every file's first step).
pub const LATER_STEP_SECONDS: f64 = 45.0;

// Code: keyword search keeps the text (~2.5 bytes of index per character) and every piece has a vector row (~1.6 KB).
const CODE_BYTES_PER_CHAR: f64 = 2.5;
const CODE_BYTES_PER_PIECE: f64 = 1600.0;
/// Characters a document chunk advances by: its size less the sentences carried into the next one.
const CHARS_PER_CHUNK: i64 = (text_chunker::CHUNK_CHARS - text_chunker::CARRY_CHARS) as i64;

/// How many pages (or chunks) a step of a long file reads: as many as take `FIRST_STEP_SECONDS` or
/// `LATER_STEP_SECONDS` at `seconds_per_unit`, at least 10 and at most 2,000.
pub fn step_units(seconds_per_unit: f64, first: bool) -> usize {
    let s = if first { FIRST_STEP_SECONDS } else { LATER_STEP_SECONDS };
    (s / seconds_per_unit.max(1e-4)).clamp(10.0, 2000.0) as usize
}

/// A PDF's page count, when something can read it (the reader helper).
pub type PageCounter<'a> = &'a dyn Fn(&str) -> Option<usize>;

/// `pdf_pages` counts a PDF's pages (None: unreadable): the reader helper's when there is one, so a broken PDF never
/// takes the app down while Settings estimates a folder.
pub fn estimate<'a>(
    files: impl IntoIterator<Item = (&'a str, i64, FileCategory, bool)>,
    costs: &UnitCosts,
    cancel: &Cancel,
    pdf_pages: Option<PageCounter<'_>>,
) -> Result<Workload, Cancelled> {
    let mut w = Workload::default();
    let (mut now, mut later) = (0.0, 0.0);
    // A long file: its first step now, the rest later.
    let stepped = |units: usize, per_unit: f64, now: &mut f64, later: &mut f64| {
        let first = units.min(step_units(per_unit, true));
        *now += first as f64 * per_unit;
        *later += (units - first) as f64 * per_unit;
    };
    for (path, size, category, indexed) in files {
        cancel.check()?;
        w.files += 1;
        *w.by_kind.entry(category).or_insert(0) += 1;
        if indexed {
            w.indexed += 1;
            continue;
        }
        let before = now + later;
        match category {
            FileCategory::Image => {
                now += if image::mode_for(path, true) == image::OcrMode::Full { costs.per_screenshot } else { costs.per_photo };
            }
            FileCategory::Pdf => {
                // Unreadable header: about 100 KB a page.
                let counted = match pdf_pages {
                    Some(f) => f(path),
                    None => pdf::page_count(path),
                };
                let total = counted.unwrap_or_else(|| ((size / 100_000) as usize).max(1)).min(pdf::MAX_PAGES);
                w.pdf_pages += total;
                stepped(total, costs.per_pdf_page, &mut now, &mut later);
            }
            FileCategory::Document => {
                let chunks = (size / CHARS_PER_CHUNK).clamp(1, text_chunker::MAX_CHUNKS_PER_DOCUMENT as i64) as usize;
                stepped(chunks, costs.per_text_chunk, &mut now, &mut later);
            }
            FileCategory::Audio => {
                let a = duration(path).unwrap_or(size as f64 / 16_000.0); // ~128 kbit/s
                w.media_seconds += a;
                now += a * costs.per_audio_second;
            }
            FileCategory::Video => {
                let v = duration(path).unwrap_or(size as f64 / 1_000_000.0); // ~8 Mbit/s
                w.media_seconds += v;
                now += v * costs.per_video_second;
            }
            FileCategory::Code => {
                // A notebook is mostly outputs.
                let chars = if ends_with_ignore_case(path, ".ipynb") { size / 8 } else { size };
                let piece = code_chunker::PIECE_CHARS as i64;
                let pieces = ((chars + piece - 1) / piece).max(1);
                now += pieces as f64 * costs.per_code_piece;
                w.bytes += chars as f64 * CODE_BYTES_PER_CHAR + pieces as f64 * CODE_BYTES_PER_PIECE;
            }
            FileCategory::None => {}
        }
        if unit_kind(category, path).is_some_and(|k| !costs.is_measured(k)) {
            w.guessed_seconds += now + later - before;
        }
    }
    w.seconds = now;
    w.rest_seconds = later;
    Ok(w)
}

/// A recording's length from its header (System.Media.Duration), without decoding it.
pub fn duration(path: &str) -> Option<f64> {
    use windows::Win32::Storage::EnhancedStorage::PKEY_Media_Duration;
    use windows::Win32::UI::Shell::PropertiesSystem::{GPS_DEFAULT, IPropertyStore, SHGetPropertyStoreFromParsingName};
    use windows::core::HSTRING;
    crate::win::init_com_mta();
    // SAFETY: COM calls on owned interfaces; the PROPVARIANT owns and frees its value.
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreFromParsingName(&HSTRING::from(path), None, GPS_DEFAULT).ok()?;
        let value = store.GetValue(&PKEY_Media_Duration).ok()?;
        let d = u64::try_from(&value).ok()?;
        (d > 0).then(|| d as f64 / 10_000_000.0)
    }
}

/// A duration as an estimate deserves it: seconds under two minutes, minutes under two hours, else hours to a tenth
/// ("< 1 s", "45 s", "12 min", "3.5 h").
pub fn duration_text(seconds: f64) -> String {
    if seconds < 1.0 {
        "< 1 s".into()
    } else if seconds < 120.0 {
        format!("{} s", seconds.round())
    } else if seconds < 7200.0 {
        format!("{} min", (seconds / 60.0).round())
    } else {
        format!("{} h", one_decimal(seconds / 3600.0))
    }
}

/// A size in decimal units, whole below a gigabyte ("640 KB", "40 MB", "1.2 GB").
pub fn size_text(bytes: f64) -> String {
    if bytes < 1e6 {
        format!("{} KB", (bytes / 1e3).max(1.0).round())
    } else if bytes < 1e9 {
        format!("{} MB", (bytes / 1e6).round())
    } else {
        format!("{} GB", one_decimal(bytes / 1e9))
    }
}

/// "2.5", "3" (one decimal at most, as .NET's "0.#").
fn one_decimal(x: f64) -> String {
    let r = (x * 10.0).round() / 10.0;
    if r.fract() == 0.0 { format!("{}", r as i64) } else { format!("{r:.1}") }
}

/// What indexing costs on this PC, learnt from the indexing itself, in costs.json beside the settings. Per kind of unit
/// it keeps decaying sums of seconds and units: every unit added shrinks what came before by a little, so a kind's last
/// ~half-life units carry half the weight. A kind's cost is those sums blended with the default as if the default had
/// been measured on `prior_units` units: the first files move it a little, a few dozen most of the way.
#[derive(Debug, Default)]
pub struct CostLog {
    inner: Mutex<CostState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CostState {
    #[serde(rename = "kinds")]
    kinds: HashMap<String, Sums>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Sums {
    seconds: f64,
    units: f64,
}

fn half_life(kind: &str) -> Option<f64> {
    Some(match kind {
        "photo" => 300.0,
        "screenshot" => 200.0,
        "pdf-page" => 400.0,
        "text-chunk" => 400.0,
        "audio-second" => 7200.0,
        "video-second" => 7200.0,
        "code-piece" => 600.0,
        _ => return None,
    })
}

fn prior_units(kind: &str) -> f64 {
    match kind {
        "photo" | "screenshot" => 10.0,
        "pdf-page" | "text-chunk" => 15.0,
        "audio-second" | "video-second" => 300.0,
        "code-piece" => 30.0,
        _ => 10.0,
    }
}

impl CostLog {
    fn file() -> std::path::PathBuf {
        Paths::home().join("costs.json")
    }

    pub fn load() -> CostLog {
        let state = std::fs::read(Self::file()).ok().and_then(|b| serde_json::from_slice::<CostState>(&b).ok()).unwrap_or_default();
        CostLog { inner: Mutex::new(state) }
    }

    /// A file that took `seconds` of the indexer's time for `units` of `kind`.
    pub fn add(&self, kind: &str, units: f64, seconds: f64) {
        let Some(half) = half_life(kind) else { return };
        if units <= 0.0 || seconds <= 0.0 {
            return;
        }
        let mut s = self.inner.lock().unwrap();
        let sums = s.kinds.entry(kind.to_string()).or_default();
        let keep = 0.5f64.powf(units / half);
        sums.seconds = sums.seconds * keep + seconds;
        sums.units = sums.units * keep + units;
        s.dirty = true;
    }

    /// Writes costs.json when something was measured since the last save.
    pub fn save(&self) {
        let json = {
            let mut s = self.inner.lock().unwrap();
            if !s.dirty {
                return;
            }
            s.dirty = false;
            serde_json::to_string_pretty(&*s).unwrap_or_default()
        };
        if let Err(e) = std::fs::write(Self::file(), json) {
            Log::warn(format!("Cannot save measured costs: {e}"));
        }
    }

    /// The costs to estimate with: each default moved towards what was measured.
    pub fn unit_costs(&self) -> UnitCosts {
        let d = UnitCosts::default();
        let s = self.inner.lock().unwrap();
        let blend = |kind: &str| -> f64 {
            let prior = prior_units(kind);
            match s.kinds.get(kind) {
                Some(m) => (m.seconds + prior * d.for_kind(kind)) / (m.units + prior),
                None => d.for_kind(kind),
            }
        };
        // Measured: enough units that the default's share in the blend is a sixth at most.
        let measured = KINDS
            .iter()
            .enumerate()
            .filter(|(_, k)| s.kinds.get(**k).is_some_and(|m| m.units >= 5.0 * prior_units(k)))
            .fold(0u8, |bits, (i, _)| bits | 1 << i);
        UnitCosts {
            measured,
            per_photo: blend("photo"),
            per_screenshot: blend("screenshot"),
            per_pdf_page: blend("pdf-page"),
            per_text_chunk: blend("text-chunk"),
            per_audio_second: blend("audio-second"),
            per_video_second: blend("video-second"),
            per_code_piece: blend("code-piece"),
        }
    }
}

/// The label of a kind of unit for a file (what `CostLog` measures).
pub fn unit_kind(category: FileCategory, path: &str) -> Option<&'static str> {
    Some(match category {
        FileCategory::Image => {
            if image::mode_for(path, true) == image::OcrMode::Full {
                "screenshot"
            } else {
                "photo"
            }
        }
        FileCategory::Pdf => "pdf-page",
        FileCategory::Document => "text-chunk",
        FileCategory::Code => "code-piece",
        FileCategory::Audio => "audio-second",
        FileCategory::Video => "video-second",
        FileCategory::None => return None,
    })
}

/// A path's parent's name (for logs and estimates).
pub fn folder_of(path: &str) -> &str {
    pathx::parent(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rough_durations_have_the_precision_an_estimate_deserves() {
        assert_eq!(duration_text(0.2), "< 1 s");
        assert_eq!(duration_text(8.4), "8 s");
        assert_eq!(duration_text(100.0), "100 s");
        assert_eq!(duration_text(125.0), "2 min");
        assert_eq!(duration_text(5400.0), "90 min");
        assert_eq!(duration_text(9000.0), "2.5 h");
        assert_eq!(size_text(640_000.0), "640 KB");
        assert_eq!(size_text(40_200_000.0), "40 MB");
        assert_eq!(size_text(1_200_000_000.0), "1.2 GB");
    }

    #[test]
    fn measured_costs_move_the_defaults_towards_what_indexing_measured() {
        let m = CostLog::default();
        let d = UnitCosts::default().per_photo;
        assert_eq!(m.unit_costs().per_photo, d);
        m.add("photo", 10.0, 0.1);
        assert!((m.unit_costs().per_photo - (0.1 + 10.0 * d) / 20.0).abs() < 1e-6);
        for _ in 0..100 {
            m.add("photo", 10.0, 0.1);
        }
        assert!((0.01..=0.012).contains(&m.unit_costs().per_photo));
        for _ in 0..100 {
            m.add("photo", 10.0, 1.0);
        }
        assert!((0.09..=0.1).contains(&m.unit_costs().per_photo));
        m.add("nonsense", 10.0, 1.0);
        assert_eq!(m.unit_costs().per_screenshot, UnitCosts::default().per_screenshot);
    }

    #[test]
    fn a_time_is_a_guess_until_this_pc_has_measured_the_kind() {
        let m = CostLog::default();
        assert!(!m.unit_costs().is_measured("photo"));
        m.add("photo", 10.0, 0.3);
        assert!(!m.unit_costs().is_measured("photo"), "10 photos: the default still weighs half");
        for _ in 0..5 {
            m.add("photo", 10.0, 0.3);
        }
        let c = m.unit_costs();
        assert!(c.is_measured("photo") && !c.is_measured("video-second"));
        let files = [(r"C:\p\a.jpg", 1_000_000, FileCategory::Image, false), (r"C:\p\b.mp4", 8_000_000, FileCategory::Video, false)];
        let w = estimate(files, &c, &Cancel::never(), Some(&|_: &str| None)).unwrap();
        // The video (no duration in a file that isn't there: 8 s from its size) is guessed, the photo isn't.
        assert!((w.guessed_seconds - 8.0 * c.per_video_second).abs() < 1e-9);
        assert!(w.seconds > w.guessed_seconds);
    }

    #[test]
    fn long_files_are_read_in_steps_of_about_the_same_time() {
        assert_eq!(step_units(0.05, true), 160);
        assert_eq!(step_units(0.05, false), 900);
        assert_eq!(step_units(5.0, true), 10);
        assert_eq!(step_units(0.0001, false), 2000);
    }
}
