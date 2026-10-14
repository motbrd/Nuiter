//! Keeps the index in sync with the configured folders: initial scan, file-system watching, a prioritised work queue
//! (documents → images → PDFs → audio → video → the rest of long files) and batched embedding.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::forecast::{self, CostLog, UnitCosts, Workload};
use super::power;
use super::store::{IndexStore, status};
use super::vectors::VectorIndex;
use super::watchdog::ReadingWatchdog;
use super::watcher::{WatchEvent, Watcher};
use crate::cancel::{self, Cancel};
use crate::extract::host::ExtractionHost;
use crate::extract::protocol::CallBody;
use crate::extract::{self, ExtractContext, ExtractedItem, chunk_kind, image, media};
use crate::file_kinds::{self, FileCategory, pathx};
use crate::paths::Log;
use crate::runtime::{EmbedInput, EmbedPriority, Embedder, EmbeddingError, installer, prompts, reference};
use crate::settings::Settings;
use crate::uni::{eq_ignore_case, is_lower, is_upper, lower_str, starts_with_ignore_case};
use crate::vector_math;
use crate::win;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPhase {
    Idle,
    Scanning,
    Indexing,
    Paused,
    WaitingForRuntime,
}

#[derive(Debug, Clone)]
pub struct IndexStatus {
    pub phase: IndexPhase,
    pub queued: usize,
    pub current: Option<String>,
    pub files: i64,
    pub chunks: i64,
    pub errors: i64,
    pub message: Option<String>,
}

const TEXT_BATCH: usize = 16;
const MEDIA_BATCH: usize = 4;

/// Bump when the pixels sent to the vision encoder change (sizing, budgets, encoding).
pub const IMAGE_PIPELINE_VERSION: &str = "pillow-v2";
/// Bump when which video frames are taken changes (interval, cap, same-shot test).
pub const VIDEO_FRAMES_VERSION: &str = "2s-looks";
/// Bump when which sound gets vectors changes (codecs, minimum length, silence level).
pub const SOUND_VERSION: &str = "ogg-active-0.5s";
/// Bump when how documents and PDF pages are cut into pieces, or which get vectors, changes.
pub const TEXT_CHUNKS_VERSION: &str = "sentences-1600";
/// Bump when how code is cut into stretches changes.
pub const CODE_CHUNKS_VERSION: &str = "outline-1500";
/// Bump when how some document formats are read changes (`READER_FORMATS`): files of those formats are read again.
pub const DOCUMENT_READERS_VERSION: &str = "eml-mime, rtf-uc, ifilter-embeddings";
const READER_FORMATS: &[&str] = &[".eml", ".rtf", ".doc", ".msg"];

/// Text the model gets of one chunk: at most this many characters (~1,500–2,000 tokens even for dense code).
pub const MAX_INPUT_CHARS: usize = 4000;

/// Priorities that still run while the backlog waits for power: they take seconds (no audio, video, PDF tails).
const MAX_LIVE_PRIORITY_ON_POWER_WAIT: i32 = 2;

const WAITING_FOR_RUNTIME: &str = "Waiting for the EmbeddingGemma 2 runtime download";

/// After the model fails to start, reading waits this long before trying again, twice as long each time it fails
/// again, up to `MODEL_RETRY_MAX`.
const MODEL_RETRY_FIRST: Duration = Duration::from_secs(30);
const MODEL_RETRY_MAX: Duration = Duration::from_secs(600);

/// A file found by a walk.
#[derive(Debug, Clone)]
struct Found {
    path: String,
    size: i64,
    mtime: i64,
    attributes: u32,
}

#[derive(Default)]
struct Queues {
    /// (priority, -mtime, seq) → path.
    backlog: BinaryHeap<Reverse<(i32, i64, u64, String)>>,
    /// Files the watcher just reported: indexed first, and even while the backlog waits for power.
    live: BinaryHeap<Reverse<(i32, u64, String)>>,
    /// Queued files → whether they're in the live lane. A file the watcher reports while it waits in the backlog moves to
    /// the live lane; its backlog entry is skipped when it comes up.
    queued: HashMap<String, bool>,
    /// Files a worker is reading now. A file is never read by two workers at once.
    busy: HashSet<String>,
    /// Files asked for again while being read: (path, priority, mtime, live), queued once the read is done.
    again: HashMap<String, (String, i32, i64, bool)>,
    seq: u64,
}

#[derive(Default)]
struct ScanState {
    scanning: bool,
    requested: bool,
    /// A scan asked for meanwhile wants failed files retried.
    requested_retry: bool,
}

type StatusFn = Box<dyn Fn(IndexStatus) + Send + Sync>;
type StoppedFn = Box<dyn Fn(String) + Send + Sync>;

struct Inner {
    store: Arc<IndexStore>,
    vectors: Arc<VectorIndex>,
    embedder: Arc<dyn Embedder>,
    reader: Option<Arc<ExtractionHost>>,
    settings: RwLock<Settings>,

    queue: Mutex<Queues>,
    signal: Condvar,
    pending: Mutex<HashMap<String, (String, Instant)>>,
    watchers: Mutex<Vec<Watcher>>,
    current: Mutex<HashMap<u64, String>>,
    current_seq: AtomicI64,
    cancel: Cancel,
    paused: AtomicBool,
    scan: Mutex<ScanState>,
    scanning: AtomicBool,
    boost_roots: Mutex<Vec<String>>,
    last_error: Mutex<Option<String>>,
    costs: CostLog,
    costs_saved: Mutex<Instant>,
    on_status: Mutex<Option<StatusFn>>,
    on_stopped: Mutex<Option<StoppedFn>>,
    last_raise: Mutex<Instant>,
    watchdog: Mutex<Option<ReadingWatchdog>>,
    suspects: Mutex<HashSet<String>>,
    /// Folder → whether it is in a code project (filled by walks, looked up for single changes).
    project_dirs: Mutex<HashMap<String, bool>>,
    datasets: RwLock<HashSet<String>>,
    unavailable: RwLock<Vec<String>>,
    last_availability_check: Mutex<Instant>,
    power_probe: RwLock<Option<PowerProbe>>,
    work_since_compaction: AtomicUsize,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// The index's counts, kept for `STATS_FOR`: the status line and Settings ask for them on every paint.
    stats: Mutex<Option<(Instant, IndexPhase, Counts)>>,
    /// The model couldn't be started: no file is read before this moment, and why.
    model_retry: Mutex<Option<(Instant, String)>>,
    /// How long the next such wait is (it doubles while the model keeps failing).
    model_backoff: Mutex<Duration>,
}

/// Why the backlog waits, as tests make it up.
type PowerProbe = Box<dyn Fn() -> Option<String> + Send + Sync>;
/// (files indexed, vectors, files that failed, files read in part).
type Counts = (i64, i64, i64, i64);

const STATS_FOR: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub struct Indexer(Arc<Inner>);

fn key(p: &str) -> String {
    lower_str(p)
}

impl Indexer {
    pub fn new(store: Arc<IndexStore>, vectors: Arc<VectorIndex>, embedder: Arc<dyn Embedder>, settings: Settings, reader: Option<Arc<ExtractionHost>>) -> Indexer {
        Indexer(Arc::new(Inner {
            store,
            vectors,
            embedder,
            reader,
            settings: RwLock::new(settings),
            queue: Mutex::new(Queues::default()),
            signal: Condvar::new(),
            pending: Mutex::new(HashMap::new()),
            watchers: Mutex::new(vec![]),
            current: Mutex::new(HashMap::new()),
            current_seq: AtomicI64::new(0),
            cancel: Cancel::new(),
            paused: AtomicBool::new(false),
            scan: Mutex::new(ScanState::default()),
            scanning: AtomicBool::new(false),
            boost_roots: Mutex::new(vec![]),
            last_error: Mutex::new(None),
            costs: CostLog::load(),
            costs_saved: Mutex::new(Instant::now()),
            on_status: Mutex::new(None),
            on_stopped: Mutex::new(None),
            last_raise: Mutex::new(Instant::now() - Duration::from_secs(10)),
            watchdog: Mutex::new(None),
            suspects: Mutex::new(HashSet::new()),
            project_dirs: Mutex::new(HashMap::new()),
            datasets: RwLock::new(HashSet::new()),
            unavailable: RwLock::new(vec![]),
            last_availability_check: Mutex::new(Instant::now()),
            power_probe: RwLock::new(None),
            work_since_compaction: AtomicUsize::new(0),
            threads: Mutex::new(vec![]),
            stats: Mutex::new(None),
            model_retry: Mutex::new(None),
            model_backoff: Mutex::new(MODEL_RETRY_FIRST),
        }))
    }

    pub fn on_status(&self, f: impl Fn(IndexStatus) + Send + Sync + 'static) {
        *self.0.on_status.lock().unwrap() = Some(Box::new(f));
    }

    /// A file made reading run away (`ReadingWatchdog`): it's marked as unreadable, and the process has to end, since
    /// nothing else stops the code that's stuck in it. The app starts itself again; the CLI exits.
    pub fn on_stopped(&self, f: impl Fn(String) + Send + Sync + 'static) {
        *self.0.on_stopped.lock().unwrap() = Some(Box::new(f));
    }

    /// Tests stand in for the power state with this.
    pub fn set_power_probe(&self, f: impl Fn() -> Option<String> + Send + Sync + 'static) {
        *self.0.power_probe.write().unwrap() = Some(Box::new(f));
    }

    pub fn settings(&self) -> Settings {
        self.0.settings.read().unwrap().clone()
    }

    pub fn paused(&self) -> bool {
        self.0.paused.load(Ordering::SeqCst)
    }

    pub fn set_paused(&self, p: bool) {
        self.0.paused.store(p, Ordering::SeqCst);
        self.0.signal.notify_all();
        self.raise_status(true);
    }

    // ---------- lifecycle ----------

    /// Loads stored vectors (checking the model id) and starts scanning, watching and indexing.
    pub fn start(&self) {
        let s = &self.0.store;
        if let Some(id) = s.get_meta("model_id")
            && id != installer::MODEL_ID
        {
            Log::warn(format!("Index was built with {id}; re-embedding everything with {}", installer::MODEL_ID));
            for f in s.all_files() {
                s.delete_file(&f.path);
            }
            s.set_meta("ref_vec", None);
        }
        s.set_meta("model_id", Some(installer::MODEL_ID));
        let version_check = |meta: &str, version: &str, mark: &dyn Fn() -> usize, what: &str| {
            if s.get_meta(meta).as_deref() != Some(version) {
                let n = mark();
                if n > 0 {
                    Log::info(format!("{what} changed ({} → {version}); reading {n} files again", s.get_meta(meta).unwrap_or_else(|| "v0".into())));
                }
                s.set_meta(meta, Some(version));
            }
        };
        version_check("image_pipeline", IMAGE_PIPELINE_VERSION, &|| s.mark_image_files_for_reindex(), "Image preparation");
        version_check("video_frames", VIDEO_FRAMES_VERSION, &|| s.mark_category_for_reindex(FileCategory::Video), "Video sampling");
        version_check(
            "text_chunks",
            TEXT_CHUNKS_VERSION,
            &|| s.mark_category_for_reindex(FileCategory::Document) + s.mark_category_for_reindex(FileCategory::Pdf),
            "Text chunking",
        );
        version_check("code_chunks", CODE_CHUNKS_VERSION, &|| s.mark_category_for_reindex(FileCategory::Code), "Code chunking");
        version_check("document_readers", DOCUMENT_READERS_VERSION, &|| s.mark_extensions_for_reindex(READER_FORMATS), "Reading e-mail, RTF and Word 97 files");
        version_check("sound", SOUND_VERSION, &|| s.mark_soundless_for_reindex(), "Sound reading");
        // Text read from pictures (OCR) is for exact-word search only; the picture's own vector carries its meaning.
        if s.get_meta("ocr_vectors").as_deref() != Some("none") {
            let n = s.drop_vectors(chunk_kind::OCR);
            if n > 0 {
                Log::info(format!("Removed {n} OCR text vectors (OCR text is now keyword-only)"));
            }
            s.set_meta("ocr_vectors", Some("none"));
        }

        // The vectors are read when a search needs them (`SearchEngine::search`, the search window's warm-up), not at
        // every start: a tray app that is rarely searched shouldn't hold 1.5 KB per vector from the moment it starts.
        self.0.vectors.release();
        Log::info(format!("{} vectors in the index", s.vector_count()));

        // Reading a file waits on its helper and then on the model far more than it computes, so half the logical
        // processors, 2 to 12, keep both busy. NUITER_WORKERS overrides.
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let parallel = std::env::var("NUITER_WORKERS").ok().and_then(|v| v.parse().ok()).filter(|&n: &usize| n > 0).unwrap_or((cpus / 2).clamp(2, 12));
        let mut threads = self.0.threads.lock().unwrap();
        for i in 0..parallel {
            let me = self.clone();
            if let Ok(t) = std::thread::Builder::new().name(format!("Indexer{i}")).stack_size(4 << 20).spawn(move || me.work_loop()) {
                threads.push(t);
            }
        }
        self.start_watchdog();
        let me = self.clone();
        if let Ok(t) = std::thread::Builder::new().name("IndexerTicker".into()).stack_size(512 << 10).spawn(move || {
            while me.0.cancel.sleep(1000) {
                me.flush_pending();
            }
        }) {
            threads.push(t);
        }
        drop(threads);
        self.start_watchers();
        self.rescan_in_background(false);
    }

    pub fn update_settings(&self, s: Settings) {
        let old = self.settings();
        let roots_changed = s.folders != old.folders
            || s.code_folders != old.code_folders
            || s.excluded_paths != old.excluded_paths
            || s.excluded_extensions != old.excluded_extensions
            || s.index_images != old.index_images
            || s.index_audio != old.index_audio
            || s.index_video != old.index_video;
        // Folders the user just added are scanned first and jump the queue, so they show up in results right away.
        let before: Vec<&String> = old.folders.iter().chain(&old.code_folders).collect();
        let added: Vec<String> = s
            .folders
            .iter()
            .chain(&s.code_folders)
            .filter(|f| !before.iter().any(|b| eq_ignore_case(b, f)))
            .filter(|f| std::path::Path::new(f).is_dir())
            .map(|f| pathx::normalize_dir(f))
            .collect();
        self.0.boost_roots.lock().unwrap().extend(added);
        *self.0.settings.write().unwrap() = s;
        if !roots_changed {
            return;
        }
        self.stop_watchers();
        self.start_watchers();
        self.rescan_in_background(false);
    }

    /// Returns when scanning and the work queue are both finished (used by the CLI and tests).
    pub fn wait_for_idle(&self, cancel: &Cancel) -> Result<(), cancel::Cancelled> {
        if !cancel.sleep(300) {
            return Err(cancel::Cancelled);
        }
        while !matches!(self.status().phase, IndexPhase::Idle | IndexPhase::Paused) {
            if !cancel.sleep(300) {
                return Err(cancel::Cancelled);
            }
        }
        Ok(())
    }

    /// Asks the threads to stop without waiting for them. Before ending the reader helpers and the model at closing: a
    /// worker waiting on one of them then knows it's being stopped (its file stays queued for the next start, it isn't
    /// marked as unreadable).
    pub fn cancel(&self) {
        self.0.cancel.cancel();
        self.0.signal.notify_all();
    }

    /// Stops the threads and watchers (a clean stop: nothing is left half-read).
    pub fn stop(&self) {
        self.cancel();
        self.stop_watchers();
        self.0.costs.save();
        if let Some(w) = self.0.watchdog.lock().unwrap().take() {
            w.stop();
        }
        let threads: Vec<JoinHandle<()>> = self.0.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }

    // ---------- files that make reading run away ----------

    fn start_watchdog(&self) {
        let vectors = self.0.vectors.clone();
        let w = ReadingWatchdog::new(self.0.store.file_path.with_extension("db.reading"), move || vectors.bytes() as u64);
        // Files the last run was reading when it ended without closing. Once can be anything (a power cut, a kill);
        // the second time in a row it's the file.
        for path in w.left_over() {
            let k = format!("suspect:{}", lower_str(&path));
            if self.0.store.get_meta(&k).as_deref() == Some("1") {
                self.mark_unreadable(&path, "Nuiter closed twice while reading it");
                self.0.store.set_meta(&k, None);
            } else {
                Log::warn(format!("Nuiter closed while reading {path}; it's read again once"));
                self.0.store.set_meta(&k, Some("1"));
                self.0.suspects.lock().unwrap().insert(key(&path));
            }
        }
        let me = Arc::downgrade(&self.0);
        w.on_ran_away(move |path, why| {
            let Some(inner) = me.upgrade() else { return };
            let ix = Indexer(inner);
            if let Some(p) = &path {
                ix.mark_unreadable(p, &format!("It {why}"));
            }
            let name = path.as_deref().map(pathx::file_name).unwrap_or("Indexing").to_string();
            let msg = format!("{name} {why}; restarting");
            *ix.0.last_error.lock().unwrap() = Some(msg.clone());
            ix.raise_status(true);
            if let Some(f) = ix.0.on_stopped.lock().unwrap().as_ref() {
                f(msg);
            }
        });
        w.start();
        *self.0.watchdog.lock().unwrap() = Some(w);
    }

    /// The file isn't read again until it changes; what was stored of it stays searchable.
    fn mark_unreadable(&self, path: &str, why: &str) {
        Log::warn(format!("Leaving out {path}: {why}"));
        if let Some(rec) = self.0.store.get_file(path) {
            self.0.store.finish_file(rec.id, status::ERROR, None, Some(why), None);
        }
    }

    /// A suspect read fine after all.
    fn cleared(&self, path: &str) {
        if !self.0.suspects.lock().unwrap().remove(&key(path)) {
            return;
        }
        self.0.store.set_meta(&format!("suspect:{}", lower_str(path)), None);
    }

    /// Verifies that the running model still produces the vectors stored in the index (guards llama.cpp upgrades).
    pub fn verify_model(&self, cancel: &Cancel) -> Result<()> {
        let probe = prompts::document(None, reference::TEXT);
        let v = self.0.embedder.embed(&[EmbedInput::Text(probe)], EmbedPriority::Query, cancel)?.remove(0);
        let store = &self.0.store;
        let Some(stored) = store.get_meta("ref_vec") else {
            store.set_meta("ref_vec", Some(&vector_math::base64_encode(&vector_math::to_blob(&v))));
            return Ok(());
        };
        let r = vector_math::base64_decode(&stored).map(|b| vector_math::from_blob(&b)).unwrap_or_default();
        let cos = vector_math::dot(&v, &r);
        if cos < 0.995 {
            Log::warn(format!("Reference vector drifted (cos={cos:.4}); re-embedding the index"));
            for f in store.all_files() {
                self.0.vectors.remove(store.delete_file(&f.path));
            }
            store.set_meta("ref_vec", Some(&vector_math::base64_encode(&vector_math::to_blob(&v))));
            self.rescan_in_background(false);
        }
        Ok(())
    }

    // ---------- status ----------

    pub fn status(&self) -> IndexStatus {
        // Files being read count from the moment they leave the queue (`busy`, set under the same lock), so there's no
        // instant between the queue and `current` in which the indexer looks idle (`wait_for_idle` ended early on it).
        let (queued, busy) = {
            let q = self.0.queue.lock().unwrap();
            (q.queued.len() + q.again.len(), q.busy.len())
        };
        let (current, in_flight) = {
            let c = self.0.current.lock().unwrap();
            (c.values().next().cloned(), c.len())
        };
        let last_error = self.0.last_error.lock().unwrap().clone();
        let phase = if last_error.is_some() && !installer::is_installed() {
            IndexPhase::WaitingForRuntime
        } else if self.paused() || (self.wait_reason().is_some() && current.is_none()) {
            IndexPhase::Paused
        } else if self.0.scanning.load(Ordering::SeqCst) {
            IndexPhase::Scanning
        } else if queued > 0 || busy > 0 || current.is_some() {
            IndexPhase::Indexing
        } else {
            IndexPhase::Idle
        };
        let (files, chunks, errors, _) = self.stats(phase);
        IndexStatus { phase, queued: queued + in_flight, current, files, chunks, errors, message: last_error }
    }

    /// (files indexed, vectors, files that failed, files read in part), at most `STATS_FOR` old, and counted again
    /// whenever the phase changes: "Up to date" must show the last file's numbers, not those of a moment before it.
    fn stats(&self, phase: IndexPhase) -> Counts {
        let mut cache = self.0.stats.lock().unwrap();
        if let Some((at, p, v)) = *cache
            && p == phase
            && at.elapsed() < STATS_FOR
        {
            return v;
        }
        let v = self.0.store.stats();
        *cache = Some((Instant::now(), phase, v));
        v
    }

    fn raise_status(&self, force: bool) {
        {
            let mut last = self.0.last_raise.lock().unwrap();
            if !force && last.elapsed() < Duration::from_millis(400) {
                return;
            }
            *last = Instant::now();
        }
        let has = self.0.on_status.lock().unwrap().is_some();
        if !has {
            return;
        }
        let st = self.status();
        if let Some(f) = self.0.on_status.lock().unwrap().as_ref() {
            f(st);
        }
    }

    /// Why the backlog waits right now (Energy Saver, battery, hours), or None.
    pub fn power_wait(&self) -> Option<String> {
        if let Some(p) = self.0.power_probe.read().unwrap().as_ref() {
            return p();
        }
        power::wait_reason(&self.0.settings.read().unwrap())
    }

    /// Why indexing waits right now: the power (`power_wait`), or a model that failed to start and is tried again soon.
    pub fn wait_reason(&self) -> Option<String> {
        self.power_wait().or_else(|| self.model_wait())
    }

    /// The model failed to start and reading waits: why, and for how long still.
    fn model_wait(&self) -> Option<String> {
        let left = self.model_wait_left()?;
        let why = self.0.model_retry.lock().unwrap().as_ref().map(|r| r.1.clone()).unwrap_or_default();
        Some(format!("the search model didn't start ({why}); trying again in {} s", left.as_secs().max(1)))
    }

    fn model_wait_left(&self) -> Option<Duration> {
        let r = self.0.model_retry.lock().unwrap();
        let (at, _) = r.as_ref()?;
        at.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
    }

    /// The model couldn't be started: reading waits, longer each time it fails again. Workers that fail together (the
    /// files in flight) count once.
    fn model_failed(&self, why: &str) {
        let wait = {
            let mut retry = self.0.model_retry.lock().unwrap();
            if retry.as_ref().is_some_and(|r| r.0 > Instant::now()) {
                return;
            }
            let mut backoff = self.0.model_backoff.lock().unwrap();
            let wait = *backoff;
            *backoff = (*backoff * 2).min(MODEL_RETRY_MAX);
            *retry = Some((Instant::now() + wait, why.to_string()));
            wait
        };
        Log::warn(format!("The model didn't start ({why}); indexing tries again in {} s", wait.as_secs()));
        self.raise_status(true);
    }

    /// The model answered: the next failure waits the shortest time again.
    fn model_ok(&self) {
        if self.0.model_retry.lock().unwrap().take().is_some() {
            *self.0.model_backoff.lock().unwrap() = MODEL_RETRY_FIRST;
            self.raise_status(true);
        }
    }

    // ---------- scanning ----------

    pub fn rescan_in_background(&self, retry_errors: bool) {
        let me = self.clone();
        let _ = std::thread::Builder::new().name("IndexerScan".into()).stack_size(2 << 20).spawn(move || me.rescan(retry_errors));
    }

    /// Full scan. Files that failed before are retried when `retry_errors` is set.
    pub fn rescan(&self, retry_errors: bool) {
        {
            let mut s = self.0.scan.lock().unwrap();
            // A scan is already running: run another one right after it instead of dropping the request, otherwise
            // folders added meanwhile would only be picked up after a restart.
            if s.scanning {
                s.requested = true;
                s.requested_retry |= retry_errors;
                return;
            }
            s.scanning = true;
            self.0.scanning.store(true, Ordering::SeqCst);
        }
        let mut retry = retry_errors;
        loop {
            self.raise_status(true);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.scan_once(retry)));
            if result.is_err() {
                Log::error("Scan failed");
            }
            let mut s = self.0.scan.lock().unwrap();
            if !s.requested || self.0.cancel.is_cancelled() {
                s.scanning = false;
                s.requested = false;
                s.requested_retry = false;
                self.0.scanning.store(false, Ordering::SeqCst);
                break;
            }
            retry = std::mem::take(&mut s.requested_retry);
            s.requested = false;
        }
        self.raise_status(true);
    }

    fn scan_once(&self, retry_errors: bool) {
        let sw = Instant::now();
        let store = &self.0.store;
        let settings = self.settings();
        let known: HashMap<String, super::store::FileRecord> = store.all_files().into_iter().map(|f| (key(&f.path), f)).collect();
        let unavailable = self.unavailable_roots(&settings);
        for r in &unavailable {
            Log::info(format!("Folder not reachable (drive unplugged or offline?), keeping its files: {r}"));
        }
        *self.0.unavailable.write().unwrap() = unavailable;

        let boost: Vec<String> = self.0.boost_roots.lock().unwrap().clone();
        self.0.project_dirs.lock().unwrap().clear();
        let found = self.enumerate_all(&settings, &boost);
        // Folders full of machine-named media are datasets: skipped (their files leave the index), and logged.
        let roots: HashSet<String> = settings.folders.iter().map(|f| key(&pathx::normalize_dir(f))).collect();
        let datasets = file_kinds::training_data_folders(
            found.iter().filter(|f| !f.3).map(|f| (pathx::parent(&f.0.path), pathx::stem(&f.0.path), f.1)),
            &roots,
            500,
        );
        for (folder, n) in &datasets {
            Log::info(format!("Skipping a dataset folder ({n} machine-named media files): {folder}"));
        }
        *self.0.datasets.write().unwrap() = datasets.iter().map(|d| key(&d.0)).collect();

        let mut seen: HashSet<String> = HashSet::new();
        let mut fresh: Vec<(Found, i32)> = vec![];
        for (fi, cat, is_code) in found.iter().map(|f| (&f.0, f.1, f.3)) {
            if !is_code && self.is_in_dataset(&fi.path, cat) {
                continue;
            }
            let bonus = if boost.iter().any(|r| pathx::is_strictly_under(&fi.path, r)) { -10 } else { 0 };
            let k = key(&fi.path);
            if let Some(rec) = known.get(&k)
                && rec.size == fi.size
                && rec.mtime == fi.mtime
                && rec.is_code == is_code
            {
                seen.insert(k);
                if rec.status == status::PARTIAL {
                    self.enqueue(&fi.path, 9, i64::MAX, false);
                } else if rec.status == status::INDEXING {
                    self.enqueue(&fi.path, priority(cat, &fi.path) + bonus, fi.mtime, false);
                } else if rec.status == status::ERROR && retry_errors {
                    self.enqueue(&fi.path, 8, i64::MAX, false);
                }
                continue;
            }
            seen.insert(k);
            fresh.push((fi.clone(), priority(cat, &fi.path) + bonus));
        }
        {
            let mut b = self.0.boost_roots.lock().unwrap();
            b.retain(|r| !boost.contains(r));
        }

        // Files that are gone from where they were. Files on folders that can't be reached right now are kept.
        let gone: Vec<&super::store::FileRecord> = known.values().filter(|f| !seen.contains(&key(&f.path)) && !self.is_unavailable(&f.path)).collect();
        // A new file with the name, size and date of a gone one is that file, moved: re-point it, don't re-embed.
        let mut gone_by_identity: HashMap<(String, i64, i64), VecDeque<&super::store::FileRecord>> = HashMap::new();
        for f in &gone {
            gone_by_identity.entry((lower_str(pathx::file_name(&f.path)), f.size, f.mtime)).or_default().push_back(f);
        }
        let mut moved = 0;
        let mut repathed: HashSet<i64> = HashSet::new();
        for (fi, prio) in &fresh {
            let id_key = (lower_str(pathx::file_name(&fi.path)), fi.size, fi.mtime);
            if !known.contains_key(&key(&fi.path))
                && let Some(q) = gone_by_identity.get_mut(&id_key)
                && let Some(old) = q.pop_front()
                && store.repath_file(old.id, &fi.path, &name_row(&fi.path)).is_ok() {
                    repathed.insert(old.id);
                    moved += 1;
                    if old.status == status::INDEXING {
                        self.enqueue(&fi.path, *prio, fi.mtime, false);
                    } else if old.status == status::PARTIAL {
                        self.enqueue(&fi.path, 9, i64::MAX, false);
                    }
                    continue;
                }
            self.enqueue(&fi.path, *prio, fi.mtime, false);
        }
        let mut removed = 0;
        let left: Vec<&&super::store::FileRecord> = gone.iter().filter(|f| !repathed.contains(&f.id)).collect();
        if settings.damaged {
            // settings.json couldn't be read: the folders these files came from are unknown, not removed.
            if !left.is_empty() {
                Log::warn(format!("Settings are defaults standing in for an unreadable file; keeping {} indexed files", left.len()));
            }
        } else {
            for f in left {
                self.0.vectors.remove(store.delete_file(&f.path));
                removed += 1;
            }
        }
        Log::info(format!("Scan finished: {} files in {} ms ({moved} moved, {removed} removed)", seen.len(), sw.elapsed().as_millis()));
    }

    /// (file, category, is in code search).
    fn enumerate_all(&self, settings: &Settings, first: &[String]) -> Vec<(Found, FileCategory, (), bool)> {
        let code_roots: Vec<String> =
            settings.code_folders.iter().filter(|f| std::path::Path::new(f).is_dir()).map(|f| pathx::normalize_dir(f)).collect();
        let mut roots: Vec<String> = vec![];
        for f in settings.folders.iter().filter(|f| std::path::Path::new(f).is_dir()).map(|f| pathx::normalize_dir(f)) {
            if !roots.iter().any(|r| eq_ignore_case(r, &f)) {
                roots.push(f);
            }
        }
        // Newly added folders are walked first.
        roots.sort_by_key(|r| if first.iter().any(|b| eq_ignore_case(b, r)) { 0 } else { 1 });
        let mut out = vec![];
        for root in &roots {
            for fi in self.walk(root, &code_roots, true, settings) {
                let cat = classify(&fi.path, false, settings);
                if cat != FileCategory::None {
                    out.push((fi, cat, (), false));
                }
            }
        }
        let mut seen_code: Vec<String> = vec![];
        for root in code_roots {
            if seen_code.iter().any(|r| eq_ignore_case(r, &root)) {
                continue;
            }
            for fi in self.code_files(&root, settings) {
                out.push((fi, FileCategory::Code, (), true));
            }
            seen_code.push(root);
        }
        out.into_iter().collect()
    }

    /// Files under `root`. In a `regular` walk (not code search), a code project's files are left out but for its
    /// screenshots: its assets, fixtures and datasets aren't your pictures.
    fn walk(&self, root: &str, skip_roots: &[String], regular: bool, settings: &Settings) -> Vec<Found> {
        let mut out = vec![];
        let mut stack: Vec<(String, bool)> = vec![];
        // A folder below a chosen one may sit in a project that starts higher up; above the chosen folder nothing counts.
        let parent = pathx::parent(root);
        let inherited = regular && !parent.is_empty() && is_under_configured_root(root, settings) && self.is_in_project(parent, settings);
        stack.push((root.to_string(), inherited));
        while let Some((dir, inherited)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            let mut entries: Vec<(String, std::fs::Metadata)> = vec![];
            for e in rd.flatten() {
                let Ok(m) = e.metadata() else { continue };
                let attrs = std::os::windows::fs::MetadataExt::file_attributes(&m);
                if attrs & 0x4 != 0 {
                    continue; // System
                }
                entries.push((e.file_name().to_string_lossy().into_owned(), m));
            }
            let in_project = regular && (inherited || file_kinds::is_code_project(entries.iter().map(|e| e.0.as_str())));
            if regular {
                self.0.project_dirs.lock().unwrap().insert(key(&dir), in_project);
            }
            for (name, m) in entries {
                let full = pathx::join(&dir, &name);
                let st = win::stat_of(&m);
                if st.is_dir {
                    if st.attributes & (0x400 /* reparse point */ | 0x2 /* hidden */) != 0 {
                        continue;
                    }
                    if file_kinds::should_skip_directory(&name, !regular) || is_excluded(&full, settings) {
                        continue;
                    }
                    if skip_roots.iter().any(|r| eq_ignore_case(r, &full)) {
                        continue; // indexed as code
                    }
                    stack.push((full, in_project));
                } else {
                    if st.attributes & 0x2 != 0 || file_kinds::is_cloud_placeholder(st.attributes) {
                        continue;
                    }
                    if name.starts_with("~$") || name.starts_with('.') {
                        continue; // Office lock files, dotfiles
                    }
                    if is_excluded(&full, settings) {
                        continue;
                    }
                    if in_project && !file_kinds::is_screenshot_name(&name) {
                        continue;
                    }
                    out.push(Found { path: full, size: st.size, mtime: st.mtime, attributes: st.attributes });
                }
            }
        }
        out
    }

    /// Whether `dir` is a code project or inside one, looking up no further than the chosen folder.
    fn is_in_project(&self, dir: &str, settings: &Settings) -> bool {
        // "F:\" stays whole: "F:" would mean the current folder on drive F.
        let dir = pathx::trim_end_sep(dir);
        if let Some(known) = self.0.project_dirs.lock().unwrap().get(&key(&dir)) {
            return *known;
        }
        let names: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        let mut answer = file_kinds::is_code_project(names.iter().map(String::as_str));
        if !answer && is_under_configured_root(&dir, settings) {
            let parent = pathx::parent(&dir);
            if !parent.is_empty() {
                answer = self.is_in_project(parent, settings);
            }
        }
        self.0.project_dirs.lock().unwrap().insert(key(&dir), answer);
        answer
    }

    fn is_in_dataset(&self, path: &str, category: FileCategory) -> bool {
        category.is_media() && self.0.datasets.read().unwrap().contains(&key(pathx::parent(path)))
    }

    /// Code files of a repository. Uses `git ls-files` (exact .gitignore semantics) when available.
    fn code_files(&self, root: &str, settings: &Settings) -> Vec<Found> {
        let mut listed: Option<Vec<String>> = None;
        if std::path::Path::new(root).join(".git").exists() {
            match git_ls_files(root) {
                Ok(files) => listed = Some(files),
                Err(e) => Log::warn(format!("git ls-files failed in {root}: {e}")),
            }
        }
        let source: Vec<Found> = match listed {
            Some(files) => files
                .into_iter()
                .filter_map(|p| win::stat(&p).filter(|s| !s.is_dir).map(|s| Found { path: p, size: s.size, mtime: s.mtime, attributes: s.attributes }))
                .collect(),
            None => self.walk(root, &[], false, settings),
        };
        source
            .into_iter()
            .filter(|f| f.size <= 1_500_000 && !file_kinds::is_cloud_placeholder(f.attributes))
            .filter(|f| file_kinds::is_code(&f.path) && !is_excluded(&f.path, settings))
            .filter(|f| {
                // Vendored / generated folders.
                let rel = relative(root, &f.path);
                let parts: Vec<&str> = rel.split('\\').collect();
                !parts[..parts.len().saturating_sub(1)].iter().any(|d| file_kinds::should_skip_directory(d, true))
            })
            .collect()
    }

    /// Configured folders that can't be reached: their drive isn't there (unplugged USB disk, card reader) or a network
    /// share is offline. A folder whose drive is present but which no longer exists was deleted, and isn't listed.
    fn unavailable_roots(&self, s: &Settings) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for r in s.folders.iter().chain(&s.code_folders).map(|f| pathx::normalize_dir(f)) {
            if out.iter().any(|o| eq_ignore_case(o, &r)) {
                continue;
            }
            let unavailable = if r.starts_with(r"\\") {
                !std::path::Path::new(&r).is_dir()
            } else if r.len() >= 2 && r.as_bytes()[1] == b':' {
                !std::path::Path::new(&format!("{}\\", &r[..2])).is_dir()
            } else {
                false
            };
            if unavailable {
                out.push(r);
            }
        }
        out
    }

    fn is_unavailable(&self, path: &str) -> bool {
        self.0.unavailable.read().unwrap().iter().any(|r| pathx::is_strictly_under(path, r))
    }

    /// Re-checks drive availability every 10 seconds: a drive coming back is rescanned and watched again.
    fn check_availability(&self) {
        {
            let mut last = self.0.last_availability_check.lock().unwrap();
            if last.elapsed() < Duration::from_secs(10) {
                return;
            }
            *last = Instant::now();
        }
        let settings = self.settings();
        let now = self.unavailable_roots(&settings);
        let same = {
            let cur = self.0.unavailable.read().unwrap();
            cur.len() == now.len() && cur.iter().zip(&now).all(|(a, b)| eq_ignore_case(a, b))
        };
        if same {
            return;
        }
        Log::info(format!("Folder availability changed (unreachable now: {})", if now.is_empty() { "none".into() } else { now.join(", ") }));
        *self.0.unavailable.write().unwrap() = now;
        self.stop_watchers();
        self.start_watchers();
        self.rescan_in_background(false);
    }

    fn is_code_path(&self, path: &str, settings: &Settings) -> bool {
        settings.code_folders.iter().any(|r| pathx::is_strictly_under(path, &pathx::normalize_dir(r)))
    }

    // ---------- watching ----------

    fn start_watchers(&self) {
        let settings = self.settings();
        let mut watchers = self.0.watchers.lock().unwrap();
        let mut roots: Vec<String> = vec![];
        for r in settings.folders.iter().chain(&settings.code_folders) {
            if std::path::Path::new(r).is_dir() && !roots.iter().any(|x| eq_ignore_case(x, r)) {
                roots.push(r.clone());
            }
        }
        for root in roots {
            let me = Arc::downgrade(&self.0);
            let r2 = root.clone();
            match Watcher::start(&pathx::normalize_dir(&root), move |e| {
                let Some(inner) = me.upgrade() else { return };
                let ix = Indexer(inner);
                match e {
                    WatchEvent::Touched(p) => ix.touch(&p),
                    WatchEvent::Renamed { from, to } => {
                        if !ix.try_rename(&from, &to) {
                            ix.touch(&from);
                            ix.touch(&to);
                        }
                    }
                    WatchEvent::Overflow => {
                        Log::warn(format!("Watcher overflow on {r2}; rescanning"));
                        ix.rescan_in_background(false);
                    }
                }
            }) {
                Ok(w) => watchers.push(w),
                Err(e) => Log::warn(format!("Cannot watch {root}: {e}")),
            }
        }
    }

    fn stop_watchers(&self) {
        let old: Vec<Watcher> = self.0.watchers.lock().unwrap().drain(..).collect();
        drop(old);
    }

    fn touch(&self, path: &str) {
        self.0.pending.lock().unwrap().insert(key(path), (path.to_string(), Instant::now()));
    }

    /// A rename the watcher reported: re-point the file (or every file under a folder) without re-embedding.
    fn try_rename(&self, old: &str, new: &str) -> bool {
        let store = &self.0.store;
        if std::path::Path::new(new).is_dir() {
            let n = store.repath_folder(old, new);
            if n > 0 {
                // A file's name row holds its folder's name too: the files right inside are found by the new one.
                let dir = pathx::trim_end_sep(new);
                for f in store.files_under(&dir).into_iter().filter(|f| eq_ignore_case(pathx::parent(&f.path), &dir)) {
                    let _ = store.repath_file(f.id, &f.path, &name_row(&f.path));
                }
                Log::info(format!("Folder renamed: {old} → {new} ({n} files kept)"));
            }
            return n > 0;
        }
        let Some(st) = win::stat(new) else { return false };
        let Some(rec) = store.get_file(old) else { return false };
        if rec.size != st.size || rec.mtime != st.mtime {
            return false;
        }
        if classify(new, rec.is_code, &self.settings()) == FileCategory::None {
            return false; // e.g. renamed to an excluded type
        }
        if store.repath_file(rec.id, new, &name_row(new)).is_err() {
            return false;
        }
        Log::info(format!("Renamed: {old} → {new} (kept its vectors)"));
        self.raise_status(false);
        true
    }

    /// A new file that is an indexed file moved from elsewhere (same name, size, date; old path gone).
    fn try_adopt_moved(&self, path: &str, size: i64, mtime: i64) -> bool {
        for old in self.0.store.find_by_identity(pathx::file_name(path), size, mtime) {
            if eq_ignore_case(&old.path, path) || std::path::Path::new(&old.path).exists() || self.is_unavailable(&old.path) {
                continue;
            }
            if self.0.store.repath_file(old.id, path, &name_row(path)).is_ok() {
                Log::info(format!("Moved: {} → {path} (kept its vectors)", old.path));
                self.raise_status(false);
                return true;
            }
        }
        false
    }

    /// Processes debounced file-system events: ~1.5 s for documents, ~60 s after the last edit for code.
    fn flush_pending(&self) {
        self.check_availability();
        {
            let mut saved = self.0.costs_saved.lock().unwrap();
            if saved.elapsed() > Duration::from_secs(30) {
                *saved = Instant::now();
                self.0.costs.save();
            }
        }
        let settings = self.settings();
        let mut due: Vec<(String, String, Instant, bool)> = {
            let pending = self.0.pending.lock().unwrap();
            pending
                .iter()
                .filter_map(|(k, (p, t))| {
                    let code = self.is_code_path(p, &settings);
                    (t.elapsed() >= Duration::from_millis(if code { 60_000 } else { 1_500 })).then(|| (k.clone(), p.clone(), *t, code))
                })
                .collect()
        };
        // Paths that exist first: a move's new path must be handled before its old path is treated as deleted.
        due.sort_by_cached_key(|d| if std::path::Path::new(&d.1).exists() { 0 } else { 1 });
        for (k, path, t, code) in due {
            {
                let mut pending = self.0.pending.lock().unwrap();
                match pending.get(&k) {
                    Some((_, t2)) if *t2 == t => {
                        pending.remove(&k);
                    }
                    _ => continue,
                }
            }
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle_change(&path, code, &settings))) {
                Log::warn(format!("Change handling failed for {path}: {e:?}"));
            }
        }
    }

    fn handle_change(&self, path: &str, code: bool, settings: &Settings) {
        let store = &self.0.store;
        let p = std::path::Path::new(path);
        if p.is_dir() {
            // A folder appeared or was renamed into place: index what is inside.
            if file_kinds::should_skip_directory(pathx::file_name(path), code) || is_excluded(path, settings) {
                return;
            }
            let files = if code { self.code_files(path, settings) } else { self.walk(path, &[], true, settings) };
            for fi in files {
                let cat = classify(&fi.path, code, settings);
                if cat == FileCategory::None {
                    continue;
                }
                // Only what's new or changed; a file moved in from elsewhere keeps its vectors.
                let known = store.get_file(&fi.path);
                if let Some(k) = &known
                    && k.size == fi.size
                    && k.mtime == fi.mtime
                    && k.status != status::INDEXING
                {
                    continue;
                }
                if known.is_none() && self.try_adopt_moved(&fi.path, fi.size, fi.mtime) {
                    continue;
                }
                self.enqueue(&fi.path, priority(cat, &fi.path), fi.mtime, false);
            }
            return;
        }
        let Some(st) = win::stat(path) else {
            if self.is_unavailable(path) || self.unavailable_roots(settings).iter().any(|r| pathx::is_strictly_under(path, r)) {
                return; // the drive went away; its files stay in the index
            }
            // Deleted: the path may be a file or a whole folder.
            let mut removed = store.delete_file(path);
            if removed.is_empty() {
                for f in store.files_under(path) {
                    removed.extend(store.delete_file(&f.path));
                }
            }
            self.0.vectors.remove(removed);
            self.raise_status(true);
            return;
        };
        let name = pathx::file_name(path);
        if st.attributes & 0x2 != 0 || file_kinds::is_cloud_placeholder(st.attributes) || is_excluded(path, settings) {
            return;
        }
        if name.starts_with("~$") || name.starts_with('.') {
            return;
        }
        // Folders below the chosen one that a scan doesn't walk into (the chosen folder itself may be anywhere).
        let roots = if code { &settings.code_folders } else { &settings.folders };
        let root = roots.iter().map(|r| pathx::normalize_dir(r)).filter(|r| pathx::is_strictly_under(path, r)).max_by_key(|r| r.len());
        let below = match &root {
            Some(r) => relative(r, path),
            None => path.to_string(),
        };
        let parts: Vec<&str> = below.split('\\').collect();
        if parts[..parts.len().saturating_sub(1)].iter().any(|s| file_kinds::should_skip_directory(s, code)) {
            return;
        }
        let category = if code {
            if file_kinds::is_code(path) { FileCategory::Code } else { FileCategory::None }
        } else {
            classify(path, false, settings)
        };
        if category == FileCategory::None {
            return;
        }
        if !code && (self.is_in_dataset(path, category) || (!file_kinds::is_screenshot_name(name) && self.is_in_project(pathx::parent(path), settings))) {
            return;
        }
        let rec = store.get_file(path);
        if let Some(r) = &rec
            && r.size == st.size
            && r.mtime == st.mtime
            && r.status != status::INDEXING
        {
            return;
        }
        if rec.is_none() && self.try_adopt_moved(path, st.size, st.mtime) {
            return;
        }
        self.enqueue(path, priority(category, path), st.mtime, true);
    }

    // ---------- queue / workers ----------

    /// Within a priority, newer files go first: what you saved today matters more than a 2014 photo. `live` files
    /// (just created or changed) skip the backlog.
    fn enqueue(&self, path: &str, priority: i32, mtime: i64, live: bool) {
        if !Self::push(&mut self.0.queue.lock().unwrap(), path, priority, mtime, live) {
            return;
        }
        self.0.signal.notify_one();
        self.raise_status(false);
    }

    /// Queues `path` (see `enqueue`); false when nothing new was queued.
    fn push(q: &mut Queues, path: &str, priority: i32, mtime: i64, live: bool) -> bool {
        let k = key(path);
        if q.busy.contains(&k) {
            // Being read right now: once more after that read, never in parallel with it.
            let e = q.again.entry(k).or_insert_with(|| (path.to_string(), priority, mtime, live));
            e.1 = e.1.min(priority);
            e.3 |= live;
            return false;
        }
        match q.queued.get(&k) {
            Some(true) => return false,
            Some(false) if !live => return false,
            _ => {}
        }
        q.queued.insert(k, live);
        q.seq += 1;
        let seq = q.seq;
        if live {
            q.live.push(Reverse((priority, seq, path.to_string())));
        } else {
            q.backlog.push(Reverse((priority, mtime.saturating_neg(), seq, path.to_string())));
        }
        true
    }

    /// The next file to read (marked busy), skipping backlog entries of files that moved to the live lane.
    fn try_dequeue(&self, q: &mut Queues, power_wait: bool) -> Option<(String, i32)> {
        while let Some(Reverse((p, _, _))) = q.live.peek()
            && (!power_wait || *p <= MAX_LIVE_PRIORITY_ON_POWER_WAIT)
        {
            let Reverse((p, _, path)) = q.live.pop().unwrap();
            let k = key(&path);
            if q.queued.get(&k) == Some(&true) {
                q.queued.remove(&k);
                q.busy.insert(k);
                return Some((path, p));
            }
        }
        if power_wait {
            return None;
        }
        while let Some(Reverse((p, _, _, path))) = q.backlog.pop() {
            let k = key(&path);
            if q.queued.get(&k) == Some(&false) {
                q.queued.remove(&k);
                q.busy.insert(k);
                return Some((path, p));
            }
        }
        None
    }

    /// A worker is done with `path`: what was asked for it meanwhile is queued now.
    /// Under one lock: in between, the file would be neither queued nor busy, and the indexer would look idle.
    fn release_busy(&self, path: &str) {
        let k = key(path);
        let queued = {
            let mut q = self.0.queue.lock().unwrap();
            q.busy.remove(&k);
            match q.again.remove(&k) {
                Some((p, priority, mtime, live)) => Self::push(&mut q, &p, priority, mtime, live),
                None => false,
            }
        };
        if queued {
            self.0.signal.notify_one();
        }
    }

    fn work_loop(&self) {
        crate::win::init_com_mta();
        while !self.0.cancel.is_cancelled() {
            let installed = installer::is_installed();
            if self.paused() || !installed {
                if !installed {
                    *self.0.last_error.lock().unwrap() = Some(WAITING_FOR_RUNTIME.into());
                }
                let q = self.0.queue.lock().unwrap();
                let _ = self.0.signal.wait_timeout(q, Duration::from_secs(5));
                continue;
            }
            {
                let mut e = self.0.last_error.lock().unwrap();
                if e.as_deref() == Some(WAITING_FOR_RUNTIME) {
                    *e = None;
                }
            }
            // The model failed to start a moment ago: nothing can be embedded until it's tried again.
            if let Some(left) = self.model_wait_left() {
                let q = self.0.queue.lock().unwrap();
                let _ = self.0.signal.wait_timeout(q, left.min(Duration::from_secs(5)));
                continue;
            }
            let power_wait = self.power_wait().is_some();
            let next = {
                let mut q = self.0.queue.lock().unwrap();
                self.try_dequeue(&mut q, power_wait)
            };
            let Some((path, prio)) = next else {
                self.give_memory_back();
                self.raise_status(true);
                // While waiting for power, look again soon: the charger may come back any moment.
                let q = self.0.queue.lock().unwrap();
                if q.live.is_empty() && q.backlog.is_empty() || power_wait {
                    let _ = self.0.signal.wait_timeout(q, Duration::from_secs(if power_wait { 5 } else { 30 }));
                }
                continue;
            };
            let slot = self.0.current_seq.fetch_add(1, Ordering::SeqCst) as u64;
            self.0.current.lock().unwrap().insert(slot, path.clone());
            self.raise_status(false);
            // Reading in this process: the watchdog keeps an eye on it (a helper is watched by the host instead).
            let wd = self.0.watchdog.lock().unwrap().clone();
            if self.0.reader.is_none()
                && let Some(w) = &wd
            {
                w.begin(&path);
            }
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.index_file(&path, prio == 9)));
            self.0.current.lock().unwrap().remove(&slot);
            self.release_busy(&path);
            if let Some(w) = &wd {
                w.end(&path);
            }
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) if cancel::is_cancelled(&e) => return,
                Ok(Err(e)) => {
                    Log::error(format!("Indexer loop error: {e}"));
                    self.0.cancel.sleep(1000);
                }
                Err(_) => {
                    Log::error(format!("Indexing {path} panicked"));
                    if let Some(rec) = self.0.store.get_file(&path) {
                        self.0.store.finish_file(rec.id, status::ERROR, None, Some("the reader failed"), None);
                    }
                }
            }
        }
    }

    /// Once the queue is empty: hands back the memory indexing used (pictures, pages and audio pass through here as
    /// megabytes of buffers; SQLite's caches grew). Once per stretch of work, and only when no file is in flight.
    fn give_memory_back(&self) {
        if self.0.work_since_compaction.load(Ordering::SeqCst) == 0 || !self.0.current.lock().unwrap().is_empty() {
            return;
        }
        if self.0.work_since_compaction.swap(0, Ordering::SeqCst) == 0 {
            return;
        }
        let before = win::private_bytes();
        self.0.store.release_memory();
        crate::memory::relieve();
        Log::info(format!("Indexing idle: private {} → {} MB", before >> 20, win::private_bytes() >> 20));
    }

    fn index_file(&self, path: &str, second_pass: bool) -> Result<()> {
        self.0.work_since_compaction.fetch_add(1, Ordering::SeqCst);
        let settings = self.settings();
        let store = &self.0.store;
        let Some(st) = win::stat(path).filter(|s| !s.is_dir) else {
            if !self.is_unavailable(path) && !self.unavailable_roots(&settings).iter().any(|r| pathx::is_strictly_under(path, r)) {
                self.0.vectors.remove(store.delete_file(path));
            }
            return Ok(());
        };
        let is_code = self.is_code_path(path, &settings) && file_kinds::is_code(path);
        let category = if is_code { FileCategory::Code } else { classify(path, false, &settings) };
        if category == FileCategory::None {
            return Ok(());
        }

        let mut start_unit = 0usize;
        let existing = store.get_file(path);
        let file_id = match existing {
            Some(e) if second_pass && e.status == status::PARTIAL && e.next_page.is_some() && e.size == st.size && e.mtime == st.mtime => {
                start_unit = e.next_page.unwrap().max(0) as usize;
                store.resume_file(e.id)?;
                e.id
            }
            _ => {
                let (id, removed) = store.begin_file(path, category, is_code, st.size, st.mtime)?;
                self.0.vectors.remove(removed);
                // File name row for keyword search ("invoice_2024-03.pdf" → "invoice 2024 03 pdf"), plus the folder name.
                store.add_chunks(id, is_code, &[(&ExtractedItem::name(), None)], Some(&name_row(path)))?;
                id
            }
        };

        let costs = self.unit_costs();
        let mut ctx = ExtractContext::new(path, settings.clone());
        ctx.start_unit = start_unit;
        // Long files are read in steps of about the same time: every file gets its first step before any gets a second,
        // so all are searchable sooner; the rest of a book waits for everything else.
        ctx.unit_budget = match category {
            FileCategory::Pdf => forecast::step_units(costs.per_pdf_page, start_unit == 0),
            FileCategory::Document => forecast::step_units(costs.per_text_chunk, start_unit == 0),
            _ => usize::MAX,
        };
        let title = if is_code {
            settings
                .code_folders
                .iter()
                .map(|r| pathx::normalize_dir(r))
                .find(|r| starts_with_ignore_case(path, r))
                .map(|r| relative(&r, path))
                .unwrap_or_else(|| pathx::file_name(path).to_string())
        } else {
            pathx::stem(path).to_string()
        };
        let sw = Instant::now();
        let started_at = Instant::now();
        let mut count = 0usize;
        let mut units = Units::default();
        let wd = self.0.watchdog.lock().unwrap().clone();
        let cancel = self.0.cancel.clone();

        let result = (|| -> Result<extract::ExtractEnd> {
            let mut batch: Vec<ExtractedItem> = vec![];
            let mut title_now = title.clone();
            let mut sink = |item: ExtractedItem| -> Result<()> {
                units.add(&item);
                batch.push(item);
                if let Some(w) = &wd {
                    w.progress(path);
                }
                let media = batch.iter().filter(|b| b.media.is_some()).count();
                if batch.len() >= TEXT_BATCH || media >= MEDIA_BATCH {
                    count += self.flush(file_id, is_code, &title_now, &mut batch, &cancel)?;
                }
                while self.paused() && !cancel.is_cancelled() {
                    if let Some(w) = &wd {
                        w.progress(path);
                    }
                    cancel.sleep(500);
                }
                cancel.check()?;
                Ok(())
            };
            let end = match &self.0.reader {
                Some(reader) => reader.extract(&ctx, category, &cancel, &mut sink)?,
                None => extract::extract(&ctx, category, &cancel, &mut sink)?,
            };
            if let Some(t) = &end.title {
                title_now = t.clone();
            }
            count += self.flush(file_id, is_code, &title_now, &mut batch, &cancel)?;
            Ok(end)
        })();

        match result {
            // Closing: the helper or the model was ended under this file. It stays queued for the next start.
            Err(_) if cancel.is_cancelled() => return Err(cancel::Cancelled.into()),
            Ok(end) => {
                if let Some(next) = end.next_unit {
                    store.finish_file(file_id, status::PARTIAL, Some(next as i64), None, end.title.as_deref());
                    self.enqueue(path, 9, i64::MAX, false);
                } else {
                    store.finish_file(file_id, status::DONE, None, None, end.title.as_deref());
                }
                self.cleared(path);
                Log::info(format!("Indexed {path}: {count} items in {} ms", sw.elapsed().as_millis()));
                // The indexer's time this file took: files are read in parallel, so its share of the workers. Not when
                // the model was being loaded meanwhile: that's seconds that say nothing about this file.
                let model_waited = self.0.embedder.ready_since().is_some_and(|r| r > started_at);
                if !model_waited && let Some((kind, amount)) = units.amount(category, path) {
                    let in_flight = self.0.current.lock().unwrap().len().max(1);
                    self.0.costs.add(kind, amount, sw.elapsed().as_secs_f64() / in_flight as f64);
                }
            }
            Err(e) if cancel::is_cancelled(&e) => return Err(e),
            // The model couldn't be started (a crashed GPU driver, the runtime gone): the file isn't to blame. It's read
            // again from its start once the model is back (what this attempt stored is replaced then).
            Err(e) if EmbeddingError::is_unavailable(&e) => {
                Log::warn(format!("Model unavailable while indexing {path}: {e}"));
                store.finish_file(file_id, status::INDEXING, None, None, None);
                self.model_failed(&e.to_string());
                self.enqueue(path, priority(category, path), st.mtime, false);
            }
            Err(e) if e.downcast_ref::<EmbeddingError>().is_some() => {
                Log::warn(format!("Embedding failed for {path}: {e}"));
                store.finish_file(file_id, status::ERROR, None, Some(&e.to_string()), None);
                *self.0.last_error.lock().unwrap() = Some(e.to_string());
            }
            Err(e) => {
                Log::warn(format!("Failed to index {path}: {e}"));
                // Keep whatever was stored (at least the file name) so the file is still findable by name.
                store.finish_file(file_id, status::ERROR, None, Some(&e.to_string()), None);
            }
        }
        self.raise_status(false);
        Ok(())
    }

    fn flush(&self, file_id: i64, is_code: bool, title: &str, batch: &mut Vec<ExtractedItem>, cancel: &Cancel) -> Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        // What gets a vector: media, and text that isn't keyword-only. The model reads 2,048 tokens of an input at most,
        // and llama-server refuses a longer one outright. Pictures and sound move out of the items (only their vectors
        // are stored).
        let mut which: Vec<usize> = vec![];
        let mut inputs: Vec<EmbedInput> = vec![];
        for (i, b) in batch.iter_mut().enumerate() {
            if b.keyword_only {
                b.media = None;
                continue;
            }
            if let Some(m) = b.media.take() {
                which.push(i);
                inputs.push(m);
            } else if let Some(t) = &b.text
                && !t.trim().is_empty()
            {
                which.push(i);
                inputs.push(EmbedInput::Text(prompts::document(Some(title), cap(t))));
            }
        }
        let vectors = if inputs.is_empty() {
            vec![]
        } else {
            let v = self.embed_each(inputs, cancel)?;
            self.model_ok();
            v
        };
        let mut by_item: Vec<Option<Vec<f32>>> = vec![None; batch.len()];
        for (i, v) in which.into_iter().zip(vectors) {
            by_item[i] = v;
        }
        let rows: Vec<(&ExtractedItem, Option<&[f32]>)> = batch.iter().zip(&by_item).map(|(b, v)| (b, v.as_deref())).collect();
        let stored = self.0.store.add_chunks(file_id, is_code, &rows, None)?;
        self.0.vectors.add(stored);
        let n = batch.len();
        batch.clear();
        Ok(n)
    }

    /// Embeds a batch. When llama-server refuses it (one unreadable picture, one input it can't take), each input is
    /// tried on its own, so one bad page doesn't sink the rest of the file: what still fails stays keyword-only (None).
    /// When none of them works the model itself is the problem, and the error goes up.
    fn embed_each(&self, inputs: Vec<EmbedInput>, cancel: &Cancel) -> Result<Vec<Option<Vec<f32>>>> {
        // Only a refusal says something about the inputs; a model that couldn't be started fails them all alike.
        let refused = |e: &anyhow::Error| e.downcast_ref::<EmbeddingError>().is_some_and(|x| !x.unavailable);
        match self.0.embedder.embed(&inputs, EmbedPriority::Indexing, cancel) {
            Ok(v) => return Ok(v.into_iter().map(Some).collect()),
            Err(e) if inputs.len() > 1 && refused(&e) => {
                Log::warn(format!("Batch of {} refused ({e}); embedding one by one", inputs.len()));
            }
            Err(e) => return Err(e),
        }
        let mut result = vec![None; inputs.len()];
        let mut last: Option<anyhow::Error> = None;
        for (i, input) in inputs.iter().enumerate() {
            match self.0.embedder.embed(std::slice::from_ref(input), EmbedPriority::Indexing, cancel) {
                Ok(mut v) => result[i] = Some(v.remove(0)),
                Err(e) if refused(&e) => {
                    Log::warn(format!("Input {} of {} refused: {e}", i + 1, inputs.len()));
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(e) = last
            && result.iter().all(Option::is_none)
        {
            return Err(e);
        }
        Ok(result)
    }

    /// The costs estimates use: measured on this PC where there's enough, else the defaults.
    pub fn unit_costs(&self) -> UnitCosts {
        self.0.costs.unit_costs()
    }

    /// What indexing `root` involves (a chosen folder, or a repository for code search), by the same rules as the
    /// scan, and how long what isn't indexed yet will take. Reads metadata only.
    pub fn estimate_folder(&self, root: &str, code: bool, cancel: &Cancel) -> Result<Workload, cancel::Cancelled> {
        let root = pathx::normalize_dir(root);
        if !std::path::Path::new(&root).is_dir() {
            return Ok(Workload::default());
        }
        let settings = self.settings();
        let known: HashMap<String, super::store::FileRecord> = self.0.store.all_files().into_iter().map(|f| (key(&f.path), f)).collect();
        let mut files: Vec<(Found, FileCategory)> = if code {
            self.code_files(&root, &settings).into_iter().map(|f| (f, FileCategory::Code)).collect()
        } else {
            let code_roots: Vec<String> =
                settings.code_folders.iter().filter(|f| std::path::Path::new(f).is_dir()).map(|f| pathx::normalize_dir(f)).collect();
            let found: Vec<(Found, FileCategory)> = self
                .walk(&root, &code_roots, true, &settings)
                .into_iter()
                .map(|f| {
                    let c = classify(&f.path, false, &settings);
                    (f, c)
                })
                .filter(|f| f.1 != FileCategory::None)
                .collect();
            let roots: HashSet<String> = [key(&root)].into();
            let datasets = file_kinds::training_data_folders(found.iter().map(|f| (pathx::parent(&f.0.path), pathx::stem(&f.0.path), f.1)), &roots, 500);
            let dataset_keys: HashSet<String> = datasets.iter().map(|d| key(&d.0)).collect();
            found.into_iter().filter(|f| !(f.1.is_media() && dataset_keys.contains(&key(pathx::parent(&f.0.path))))).collect()
        };
        cancel.check()?;
        files.sort_by(|a, b| a.0.path.cmp(&b.0.path));
        let rows: Vec<(&str, i64, FileCategory, bool)> = files
            .iter()
            .map(|(f, c)| {
                let indexed = known.get(&key(&f.path)).is_some_and(|r| r.status == status::DONE && r.size == f.size && r.mtime == f.mtime);
                (f.path.as_str(), f.size, *c, indexed)
            })
            .collect();
        let reader = self.0.reader.clone();
        let count_pages = move |p: &str| -> Option<usize> {
            reader.as_ref()?.call(&CallBody::new("pdf-pages", p), Duration::from_secs(30)).and_then(|r| r.pages)
        };
        let pages: Option<forecast::PageCounter<'_>> = if self.0.reader.is_some() { Some(&count_pages) } else { None };
        forecast::estimate(rows, &costs_or(&self.unit_costs()), cancel, pages)
    }
}

fn costs_or(c: &UnitCosts) -> UnitCosts {
    *c
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// What a file amounted to, in the units estimates count.
#[derive(Default)]
struct Units {
    pages: HashSet<u64>,
    chunks: usize,
    end: f64,
}

impl Units {
    fn add(&mut self, item: &ExtractedItem) {
        if let Some(p) = item.position {
            self.pages.insert(p.to_bits()); // a PDF's items carry their page
            self.end = self.end.max(p);
        }
        if item.text.is_some() && !item.keyword_only && item.kind != chunk_kind::OCR {
            self.chunks += 1;
        }
    }

    fn amount(&self, category: FileCategory, path: &str) -> Option<(&'static str, f64)> {
        let kind = forecast::unit_kind(category, path)?;
        Some(match category {
            FileCategory::Image => (kind, 1.0),
            FileCategory::Pdf => (kind, self.pages.len() as f64),
            FileCategory::Document | FileCategory::Code => (kind, self.chunks as f64),
            FileCategory::Audio => (kind, self.end + media::WINDOW_SECONDS),
            FileCategory::Video => (kind, self.end + media::SAMPLE_SECONDS),
            FileCategory::None => return None,
        })
    }
}

/// Documents, code and screenshots first, then photos, PDFs, audio, video; the rest of long files last (9).
fn priority(c: FileCategory, path: &str) -> i32 {
    match c {
        FileCategory::Image if image::mode_for(path, true) == image::OcrMode::Full => 0, // screenshots
        FileCategory::Document | FileCategory::Code => 0,
        FileCategory::Image => 1,
        FileCategory::Pdf => 2,
        FileCategory::Audio => 3,
        FileCategory::Video => 4,
        FileCategory::None => 5,
    }
}

fn classify(path: &str, code: bool, s: &Settings) -> FileCategory {
    if code {
        return FileCategory::Code;
    }
    match file_kinds::classify(path) {
        FileCategory::Image if !s.index_images => FileCategory::None,
        FileCategory::Audio if !s.index_audio => FileCategory::None,
        FileCategory::Video if !s.index_video => FileCategory::None,
        // RAW + JPEG pairs from cameras are the same picture: index the JPEG only.
        FileCategory::Image if image::is_raw(path) && has_jpeg_twin(path) => FileCategory::None,
        c => c,
    }
}

fn has_jpeg_twin(raw: &str) -> bool {
    let stem = pathx::join(pathx::parent(raw), pathx::stem(raw));
    [".jpg", ".jpeg", ".heic"].iter().any(|e| std::path::Path::new(&format!("{stem}{e}")).exists())
}

fn is_excluded(path: &str, s: &Settings) -> bool {
    if file_kinds::is_secret_like(pathx::file_name(path)) {
        return true;
    }
    let ext = pathx::extension(path);
    if !ext.is_empty()
        && s.excluded_extensions
            .iter()
            .any(|e| eq_ignore_case(e, ext) || eq_ignore_case(&format!(".{}", e.trim_start_matches('.')), ext))
    {
        return true;
    }
    s.excluded_paths.iter().any(|p| !p.is_empty() && pathx::is_under(path, &pathx::normalize_dir(p)))
}

/// Whether `dir` is inside one of the chosen folders (not one of them itself).
fn is_under_configured_root(dir: &str, s: &Settings) -> bool {
    let dir = pathx::trim_end_sep(dir);
    s.folders.iter().any(|r| pathx::is_strictly_under(&dir, &pathx::trim_end_sep(&pathx::full_path(r))))
}

/// `path` relative to `root` (both full paths, `path` under `root`).
fn relative(root: &str, path: &str) -> String {
    let r = pathx::trim_end_sep(root);
    match crate::uni::strip_prefix_ignore_case(path, &r) {
        Some(rest) => rest.trim_start_matches('\\').to_string(),
        None => path.to_string(),
    }
}

fn cap(text: &str) -> &str {
    match text.char_indices().nth(MAX_INPUT_CHARS) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

fn find_git() -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(';')
        .map(str::trim)
        .filter(|d| pathx::is_fully_qualified(d))
        .map(|d| std::path::Path::new(d).join("git.exe"))
        .find(|p| p.is_file())
}

/// `git ls-files` of a repository: git from PATH only (never the current folder), with the repo's fsmonitor hook off
/// (a repo's own config could otherwise make it run any command it names).
fn git_ls_files(root: &str) -> Result<Vec<String>> {
    use std::os::windows::process::CommandExt;
    let git = find_git().ok_or_else(|| anyhow::anyhow!("git not found on PATH"))?;
    let out = std::process::Command::new(git)
        .args(["-C", root, "-c", "core.quotepath=off", "-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false", "ls-files", "--cached", "--others", "--exclude-standard"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        anyhow::bail!("exit code {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|rel| pathx::full_path(&pathx::join(root, &rel.replace('/', "\\"))))
        .collect())
}

/// Keyword text for a file's name row: the name split into words, plus its folder's name.
pub fn name_row(path: &str) -> String {
    format!("{} {}", name_tokens(pathx::file_name(path)), name_tokens(pathx::file_name(pathx::parent(path))))
}

/// "QuarterlyReport_2026.docx" → the name, then its words: "QuarterlyReport_2026.docx Quarterly Report 2026 docx".
pub fn name_tokens(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut sb = String::with_capacity(name.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        if matches!(c, '_' | '-' | '.' | '(' | ')' | '[' | ']' | '+' | ',') {
            sb.push(' ');
            continue;
        }
        // Split camelCase / PascalCase so "QuarterlyReport" also matches "report".
        if i > 0 && is_upper(c) && is_lower(chars[i - 1]) {
            sb.push(' ');
        }
        sb.push(c);
    }
    format!("{name} {sb}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::engine::tests::Fixture;

    fn indexer(f: &Fixture, settings: Settings) -> Indexer {
        Indexer::new(f.store.clone(), f.vectors.clone(), f.embedder.clone(), settings, None)
    }

    fn next(ix: &Indexer, power_wait: bool) -> Option<(String, i32)> {
        let mut q = ix.0.queue.lock().unwrap();
        ix.try_dequeue(&mut q, power_wait)
    }

    #[test]
    fn a_file_is_never_read_by_two_workers_at_once() {
        let f = Fixture::new("busy");
        let ix = indexer(&f, Settings::default());
        let path = r"C:\docs\notes.txt";
        ix.enqueue(path, 0, 1, false);
        let (p, _) = next(&ix, false).unwrap();
        // Changed while it's being read: no second worker gets it meanwhile, and a long file's next step waits too.
        ix.enqueue(path, 0, 2, true);
        ix.enqueue(path, 9, i64::MAX, false);
        assert!(next(&ix, false).is_none());
        assert_eq!(ix.status().queued, 1);
        // Once the read is done it's queued again, in the live lane, at the most urgent of what was asked.
        ix.release_busy(&p);
        assert_eq!(next(&ix, true), Some((path.to_string(), 0)));
        ix.release_busy(path);
        assert!(next(&ix, false).is_none());
    }

    #[test]
    fn a_change_the_watcher_reports_moves_a_waiting_file_ahead() {
        let f = Fixture::new("promote");
        let ix = indexer(&f, Settings::default());
        let (video, doc) = (r"C:\v\clip.mp4", r"C:\d\a.txt");
        ix.enqueue(video, 4, 1, false);
        ix.enqueue(doc, 0, 1, false);
        ix.enqueue(video, 2, 5, true);
        ix.enqueue(video, 2, 5, true);
        assert_eq!(ix.status().queued, 2);
        // While the backlog waits for power, the live lane still runs.
        assert_eq!(next(&ix, true).map(|n| n.0), Some(video.to_string()));
        assert!(next(&ix, true).is_none());
        assert_eq!(next(&ix, false).map(|n| n.0), Some(doc.to_string()));
        // The video's old backlog entry is skipped.
        assert!(next(&ix, false).is_none());
    }

    #[test]
    fn unreadable_settings_never_empty_the_index() {
        let f = Fixture::new("damaged");
        f.add_doc(r"C:\nowhere-nuiter-test\a.txt", "kept words", FileCategory::Document, chunk_kind::TEXT);
        let damaged = Settings { damaged: true, ..Settings::default() };
        indexer(&f, damaged).scan_once(false);
        assert!(f.store.get_file(r"C:\nowhere-nuiter-test\a.txt").is_some());
        // Settings the user saved without the folder: its files go.
        indexer(&f, Settings::default()).scan_once(false);
        assert!(f.store.get_file(r"C:\nowhere-nuiter-test\a.txt").is_none());
    }

    #[test]
    fn a_rescan_asked_for_during_a_scan_keeps_retrying_failed_files() {
        let f = Fixture::new("rescan");
        let ix = indexer(&f, Settings::default());
        f.add_doc(r"C:\nowhere-nuiter-test\b.txt", "words", FileCategory::Document, chunk_kind::TEXT);
        {
            let mut s = ix.0.scan.lock().unwrap();
            s.scanning = true;
        }
        ix.rescan(true);
        let s = ix.0.scan.lock().unwrap();
        assert!(s.requested && s.requested_retry);
    }

    #[test]
    fn a_renamed_folder_is_found_by_its_new_name() {
        let f = Fixture::new("rename");
        let ix = indexer(&f, Settings::default());
        let (old, new) = (f.dir.join("Pemba"), f.dir.join("Zanzibar"));
        std::fs::create_dir_all(&new).unwrap();
        let file = old.join("notes.txt").display().to_string();
        f.add_doc(&file, "kept words", FileCategory::Document, chunk_kind::TEXT);
        assert!(ix.try_rename(&old.display().to_string(), &new.display().to_string()));
        assert!(f.store.get_file(&new.join("notes.txt").display().to_string()).is_some());
        assert!(!f.store.keyword("\"zanzibar\"", false, 10).is_empty());
        assert!(f.store.keyword("\"pemba\"", false, 10).is_empty());
    }

    #[test]
    fn the_model_failing_to_start_holds_reading_back_and_doubles_the_wait() {
        let f = Fixture::new("modelwait");
        let ix = indexer(&f, Settings::default());
        assert!(ix.model_wait_left().is_none());
        ix.model_failed("no GPU");
        let first = ix.model_wait_left().unwrap();
        assert!(first <= MODEL_RETRY_FIRST && first > MODEL_RETRY_FIRST - Duration::from_secs(5));
        assert!(ix.model_wait().unwrap().contains("no GPU"));
        // The other files in flight fail at the same moment: they don't double the wait again.
        ix.model_failed("no GPU");
        assert_eq!(*ix.0.model_backoff.lock().unwrap(), MODEL_RETRY_FIRST * 2);
        ix.model_ok();
        assert!(ix.model_wait_left().is_none());
        assert_eq!(*ix.0.model_backoff.lock().unwrap(), MODEL_RETRY_FIRST);
    }

    #[test]
    fn relative_paths_survive_letters_whose_cases_differ_in_length() {
        // The Kelvin sign is 3 bytes, the "k" it lowercases to 1: the rest is taken by characters.
        assert_eq!(relative("C:\\\u{212A}it", r"C:\kit\a\b.txt"), r"a\b.txt");
        assert_eq!(relative(r"C:\kit", "C:\\\u{212A}it\\a.txt"), "a.txt");
        assert!(pathx::is_strictly_under("C:\\\u{212A}\\x", r"C:\k"));
        assert!(!pathx::is_strictly_under(r"C:\k", "C:\\\u{212A}"));
    }
}
