//! Image decoding (any codec Windows has: HEIC, WebP, RAW, AVIF…) with Windows.Graphics.Imaging, sizing for the vision
//! encoder exactly as the model's reference does, and OCR with Windows.Media.Ocr.

use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
    ExifOrientationMode, SoftwareBitmap,
};
use windows::Media::Ocr::{OcrEngine, OcrLine};
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Storage::FileAccessMode;
use windows::Storage::Streams::{IRandomAccessStream, InMemoryRandomAccessStream};
use windows::System::UserProfile::GlobalizationPreferences;
use windows::core::{HSTRING, Interface};

use super::bicubic;
use crate::file_kinds::pathx;
use crate::paths::Log;
use crate::uni::{contains_ignore_case, lower_str};

/// Vision soft-token budgets (EmbeddingGemma 2 accepts 70/140/280/560/1120). Fewer tokens embed faster; photos don't
/// need more than 140, while screenshots and pages keep 280 for their small detail.
pub const PHOTO_TOKENS: u32 = 140;
pub const SCREENSHOT_TOKENS: u32 = 280;
pub const PAGE_TOKENS: u32 = 280;
pub const FRAME_TOKENS: u32 = 70;
pub const MIN_SIDE: u32 = 48;
/// A picture whose longer side is shorter than this is an icon, an avatar or a UI scrap.
pub const MIN_LONG_SIDE: u32 = 256;
const OCR_MAX_SIDE: u32 = 2600;
const OCR_PROBE_MIN_WORDS: usize = 3;

/// The decoder first brings a picture down to this long side (photos 1,024 px, screenshots and pages 1,536), then the
/// bicubic resize takes it to the model's size: a 24 MP JPEG decodes in ~50 ms instead of ~240.
fn decode_side(budget: u32) -> u32 {
    if budget >= SCREENSHOT_TOKENS { 1536 } else { 1024 }
}

const RAW_EXTS: &[&str] = &[".dng", ".cr2", ".cr3", ".nef", ".arw", ".orf", ".rw2", ".raf"];
const GRAPHIC_EXTS: &[&str] = &[".png", ".bmp", ".gif", ".tif", ".tiff", ".webp", ".jxr"];

/// None: no OCR. Probe: a quick low-resolution check first (photos rarely contain text). Full: always.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrMode {
    None,
    Probe,
    Full,
}

/// `image` is a PNG at exactly the size the vision encoder uses for the budget; `pixels` the same picture as BGRA.
pub struct Decoded {
    pub image: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub ocr_text: Option<String>,
    pub pixels: Vec<u8>,
    pub pixels_width: u32,
    pub pixels_height: u32,
}

/// Screenshots, scans and graphics get full OCR; camera photos only when a quick probe finds text.
pub fn mode_for(path: &str, ocr_enabled: bool) -> OcrMode {
    if !ocr_enabled {
        return OcrMode::None;
    }
    let ext = lower_str(pathx::extension(path));
    if GRAPHIC_EXTS.contains(&ext.as_str()) {
        return OcrMode::Full;
    }
    let name = pathx::file_name(path);
    let hints = ["screenshot", "screen shot", "скриншот", "снимок экрана", "scan", "скан", "receipt", "чек", "document", "документ"];
    if hints.iter().any(|h| contains_ignore_case(name, h)) {
        return OcrMode::Full;
    }
    OcrMode::Probe
}

/// The token budget for an image file: screenshots and graphics keep more detail than photos.
pub fn budget_for(path: &str) -> u32 {
    if mode_for(path, true) == OcrMode::Full { SCREENSHOT_TOKENS } else { PHOTO_TOKENS }
}

pub fn is_raw(path: &str) -> bool {
    RAW_EXTS.contains(&lower_str(pathx::extension(path)).as_str())
}

/// A soft token is 3×3 patches of 16 px: one cell of 48×48 px of the encoder's input.
const CELL: u32 = 48;

/// The size transformers' Gemma image processor (the model's reference) scales a picture to: as many 48-px cells as
/// `budget` soft tokens, the aspect ratio kept, each side a whole number of cells (rounded down). A side that comes to
/// no cell (a very thin picture) gets one, and the other side its aspect ratio in whole cells, at most the budget.
pub fn encoder_size(width: u32, height: u32, budget: u32) -> Option<(u32, u32)> {
    let cells_per_pixel = (budget as f64 / (width as f64 * height as f64)).sqrt();
    let mut cols = (width as f64 * cells_per_pixel).floor() as u32;
    let mut rows = (height as f64 * cells_per_pixel).floor() as u32;
    if cols == 0 && rows == 0 {
        return None;
    }
    if rows == 0 {
        rows = 1;
        cols = (width / height).min(budget);
    } else if cols == 0 {
        cols = 1;
        rows = (height / width).min(budget);
    }
    Some((cols * CELL, rows * CELL))
}

/// A file as a WinRT stream (read-only, shared), without the StorageFile broker.
pub fn open_stream(path: &str) -> Result<IRandomAccessStream> {
    // SAFETY: plain call with a valid string; the stream is returned as an owned interface.
    unsafe {
        windows::Win32::System::WinRT::CreateRandomAccessStreamOnFile(&HSTRING::from(path), FileAccessMode::Read.0 as u32)
            .with_context(|| format!("cannot open {path}"))
    }
}

/// Bytes as a WinRT stream.
pub fn memory_stream(bytes: &[u8]) -> Result<IRandomAccessStream> {
    let stream = InMemoryRandomAccessStream::new()?;
    let buffer = CryptographicBuffer::CreateFromByteArray(bytes)?;
    stream.WriteAsync(&buffer)?.join()?;
    stream.Seek(0)?;
    Ok(stream.cast()?)
}

/// A stream's whole content.
pub fn read_stream(stream: &IRandomAccessStream) -> Result<Vec<u8>> {
    use windows::Storage::Streams::{Buffer, InputStreamOptions};
    let size = stream.Size()? as u32;
    stream.Seek(0)?;
    let buffer = Buffer::Create(size)?;
    let got = stream.ReadAsync(&buffer, size, InputStreamOptions::None)?.join()?;
    let mut bytes = windows::core::Array::<u8>::with_len(got.Length()? as usize);
    CryptographicBuffer::CopyToByteArray(&got, &mut bytes)?;
    Ok(bytes.to_vec())
}

pub fn from_file(path: &str, ocr: OcrMode, budget: u32) -> Result<Option<Decoded>> {
    let stream = open_stream(path)?;
    let mut decoder = BitmapDecoder::CreateAsync(&stream)?.join().map_err(|e| {
        // WINCODEC_ERR_COMPONENTNOTFOUND: no decoder recognises the bytes. For a common type that's a damaged file (or one
        // named .jpg that isn't a JPEG), not a missing codec.
        if e.code().0 as u32 == 0x8898_2F50 {
            anyhow!("not a picture Windows can read: damaged, or not really a {} file", pathx::extension(path))
        } else {
            anyhow!(e).context("the picture can't be decoded")
        }
    })?;
    // RAW files carry a full-size JPEG preview: decoding it is >10x faster than developing the RAW.
    if is_raw(path)
        && let Ok(preview) = decoder.GetPreviewAsync().and_then(|p| p.join())
        && let Ok(pd) = BitmapDecoder::CreateAsync(&preview).and_then(|d| d.join())
        && pd.PixelWidth()?.max(pd.PixelHeight()?) >= 1200
    {
        decoder = pd;
    }
    from_decoder(&decoder, ocr, budget)
}

pub fn from_stream(stream: &IRandomAccessStream, ocr: OcrMode, budget: u32) -> Result<Option<Decoded>> {
    let decoder = BitmapDecoder::CreateAsync(stream)?.join()?;
    from_decoder(&decoder, ocr, budget)
}

pub fn from_bytes(bytes: &[u8], ocr: OcrMode, budget: u32) -> Result<Option<Decoded>> {
    from_stream(&memory_stream(bytes)?, ocr, budget)
}

/// BitmapTransform scales the raw (unrotated) frame; EXIF rotation is applied afterwards. Returns the transform and the
/// final oriented size of the pixel buffer.
fn plan(d: &BitmapDecoder, max_side: u32) -> Result<(BitmapTransform, u32, u32)> {
    let (pw, ph) = (d.PixelWidth()?, d.PixelHeight()?);
    let scale = (max_side as f64 / pw.max(ph) as f64).min(1.0);
    let rw = ((pw as f64 * scale).round() as u32).max(1);
    let rh = ((ph as f64 * scale).round() as u32).max(1);
    let rotated = d.OrientedPixelWidth()? != pw;
    let t = BitmapTransform::new()?;
    t.SetScaledWidth(rw)?;
    t.SetScaledHeight(rh)?;
    t.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    Ok(if rotated { (t, rh, rw) } else { (t, rw, rh) })
}

fn from_decoder(decoder: &BitmapDecoder, ocr: OcrMode, budget: u32) -> Result<Option<Decoded>> {
    let (w, h) = (decoder.OrientedPixelWidth()?, decoder.OrientedPixelHeight()?);
    if w < MIN_SIDE || h < MIN_SIDE || w.max(h) < MIN_LONG_SIDE {
        return Ok(None);
    }
    let Some(mut target) = encoder_size(w, h, budget) else { return Ok(None) };

    // Decoded at a thumbnail size, then shrunk the way the reference does. One resize serves both the encoder input
    // and the OCR probe.
    let (transform, dw, dh) = plan(decoder, decode_side(budget))?;
    let data = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Straight,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )?
        .join()?
        .DetachPixelData()?;
    if dw != w {
        target = encoder_size(dw, dh, budget).unwrap_or(target);
    }
    let (tw, th) = target;
    let mut bgra = data.to_vec();
    drop(data);
    if bgra.len() < (dw * dh * 4) as usize {
        return Err(anyhow!("the decoder gave {} bytes for {dw}x{dh}", bgra.len()));
    }
    let pixels = shrink(&mut bgra, dw as usize, dh as usize, tw as usize, th as usize);
    drop(bgra);
    let png = encode_png(&pixels, tw, th)?;

    let mut text = None;
    if ocr != OcrMode::None && ocr_available() {
        let engines = rent();
        let result = (|| -> Result<Option<String>> {
            let mut full = ocr == OcrMode::Full;
            if !full {
                let buffer = CryptographicBuffer::CreateFromByteArray(&pixels)?;
                let small = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
                    &buffer,
                    BitmapPixelFormat::Bgra8,
                    tw as i32,
                    th as i32,
                    BitmapAlphaMode::Premultiplied,
                )?;
                let probe = engines[0].RecognizeAsync(&small)?.join()?;
                let words: usize = probe.Lines()?.into_iter().map(|l: OcrLine| l.Words().map(|w| w.Size().unwrap_or(0)).unwrap_or(0) as usize).sum();
                full = words >= OCR_PROBE_MIN_WORDS;
                small.Close()?;
            }
            if !full {
                return Ok(None);
            }
            let max_ocr = OCR_MAX_SIDE.min(OcrEngine::MaxImageDimension().unwrap_or(OCR_MAX_SIDE));
            let (t2, _, _) = plan(decoder, max_ocr)?;
            let bmp = decoder
                .GetSoftwareBitmapTransformedAsync(
                    BitmapPixelFormat::Bgra8,
                    BitmapAlphaMode::Premultiplied,
                    &t2,
                    ExifOrientationMode::RespectExifOrientation,
                    ColorManagementMode::DoNotColorManage,
                )?
                .join()?;
            let text = recognize_with(&bmp, &engines);
            bmp.Close()?;
            text
        })();
        give_back(engines);
        match result {
            Ok(t) => text = t,
            Err(e) => Log::warn(format!("OCR failed: {e}")),
        }
    }
    Ok(Some(Decoded { image: png, width: w, height: h, ocr_text: text, pixels, pixels_width: tw, pixels_height: th }))
}

/// Flattens transparency onto white (transparent PNGs must not turn black), then resizes as the reference.
fn shrink(bgra: &mut [u8], width: usize, height: usize, to_width: usize, to_height: usize) -> Vec<u8> {
    let bgra = &mut bgra[..width * height * 4];
    for p in bgra.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a == 255 {
            continue;
        }
        for c in &mut p[..3] {
            *c = ((*c as u32 * a + 255 * (255 - a)) / 255) as u8;
        }
        p[3] = 255;
    }
    bicubic::bgra(bgra, width, height, to_width, to_height)
}

/// Lossless, so the encoder sees exactly the resized pixels (no JPEG artefacts in between). RGB: alpha is all opaque.
pub fn encode_png(bgra: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for p in bgra.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    encode_png_rgb(&rgb, w, h)
}

pub fn encode_png_rgb(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() / 2);
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header()?;
        writer.write_image_data(rgb)?;
    }
    Ok(out)
}

// ---------- OCR ----------

// OcrEngine instances must not be used concurrently ("Operation aborted"), so each reader rents its own set.
static ENGINE_POOL: Mutex<Vec<Vec<OcrEngine>>> = Mutex::new(vec![]);
static OCR_AVAILABLE: OnceLock<bool> = OnceLock::new();

fn ocr_available() -> bool {
    *OCR_AVAILABLE.get_or_init(|| {
        let e = create_engines(true);
        let ok = !e.is_empty();
        ENGINE_POOL.lock().unwrap().push(e);
        ok
    })
}

fn rent() -> Vec<OcrEngine> {
    ENGINE_POOL.lock().unwrap().pop().unwrap_or_else(|| create_engines(false))
}

fn give_back(e: Vec<OcrEngine>) {
    ENGINE_POOL.lock().unwrap().push(e);
}

/// Recognises text with every OCR language the user has installed and keeps the most complete result.
pub fn recognize(bmp: &SoftwareBitmap) -> Result<Option<String>> {
    if !ocr_available() {
        return Ok(None);
    }
    let engines = rent();
    let r = recognize_with(bmp, &engines);
    give_back(engines);
    r
}

fn recognize_with(bmp: &SoftwareBitmap, engines: &[OcrEngine]) -> Result<Option<String>> {
    let mut best: Option<String> = None;
    let mut best_score: i64 = -1;
    for engine in engines {
        let r = engine.RecognizeAsync(bmp)?.join()?;
        let lines = r.Lines()?;
        let mut text = String::new();
        let mut score = 0i64;
        for (i, line) in lines.into_iter().enumerate() {
            if i > 0 {
                text.push('\n');
            }
            text.push_str(&line.Text()?.to_string_lossy());
            for w in line.Words()? {
                let t = w.Text()?.to_string_lossy();
                if t.chars().count() >= 3 && t.chars().all(crate::uni::is_letter) {
                    score += 1;
                }
            }
        }
        // Prefer the language that yields the most real words, not just the most characters.
        if score > best_score {
            best = Some(text);
            best_score = score;
        }
    }
    Ok(best.filter(|t| !t.trim().is_empty()))
}

fn create_engines(log: bool) -> Vec<OcrEngine> {
    let mut engines: Vec<OcrEngine> = vec![];
    let result = (|| -> Result<()> {
        let available: Vec<String> = OcrEngine::AvailableRecognizerLanguages()?
            .into_iter()
            .filter_map(|l| l.LanguageTag().ok().map(|t| t.to_string_lossy()))
            .collect();
        for tag in GlobalizationPreferences::Languages()? {
            let Ok(lang) = Language::CreateLanguage(&tag) else { continue };
            if !OcrEngine::IsLanguageSupported(&lang).unwrap_or(false) {
                continue;
            }
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                let t = e.RecognizerLanguage().and_then(|l| l.LanguageTag()).map(|t| t.to_string_lossy()).unwrap_or_default();
                let known = engines.iter().any(|x| {
                    x.RecognizerLanguage().and_then(|l| l.LanguageTag()).map(|y| y.to_string_lossy() == t).unwrap_or(false)
                });
                if !known {
                    engines.push(e);
                }
            }
            if engines.len() >= 3 {
                break;
            }
        }
        if engines.is_empty()
            && let Ok(d) = OcrEngine::TryCreateFromUserProfileLanguages()
        {
            engines.push(d);
        }
        if log {
            let tags: Vec<String> = engines
                .iter()
                .filter_map(|e| e.RecognizerLanguage().and_then(|l| l.LanguageTag()).ok().map(|t| t.to_string_lossy()))
                .collect();
            Log::info(format!("OCR languages: {} (installed: {})", tags.join(", "), available.join(", ")));
        }
        Ok(())
    })();
    if let Err(e) = result {
        Log::warn(format!("OCR unavailable: {e}"));
    }
    engines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_target_size_matches_reference() {
        // Values from transformers' Gemma image processor (sides are multiples of 48 px, area fills the budget).
        for (w, h, budget, tw, th) in [
            (4032, 3024, 140, 624, 480),
            (1920, 1080, 280, 1056, 576),
            (640, 360, 70, 528, 288),
            (100, 5000, 280, 96, 5664),
        ] {
            let t = encoder_size(w, h, budget).unwrap();
            assert_eq!(t, (tw, th));
            assert!(t.0 / 48 * (t.1 / 48) <= budget);
        }
    }
}
