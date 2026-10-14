//! PDFs are indexed page by page. A page with a text layer (read with pdf_oxide) is one text piece; pages without one
//! (scans, slides, figures) are rendered with Windows.Data.Pdf, embedded as images and OCR'd.

use anyhow::{Result, anyhow};
use pdf_oxide::document::PdfDocument as Pdf;
use windows::Data::Pdf::{PdfDocument as WinPdf, PdfPageRenderOptions};
use windows::Storage::Streams::{IRandomAccessStream, InMemoryRandomAccessStream};
use windows::core::Interface;

use super::image::{self, OcrMode};
use super::text_chunker;
use super::{ExtractContext, ExtractEnd, ExtractedItem, Sink, chunk_kind};
use crate::cancel::Cancel;
use crate::paths::Log;
use crate::runtime::EmbedInput;

/// A page with fewer words than this (a scan, a slide, a figure) is embedded as a picture; its few words are kept for
/// keyword search.
const MIN_WORDS_PER_PAGE: usize = 30;

/// Pages past this are never read (Settings can lower it): only machine-made giants get this far.
pub const MAX_PAGES: usize = 5000;

pub fn open(path: &str) -> Result<Pdf> {
    // Without logging noise: pdf_oxide reports recoveries through `log`, which nothing here listens to.
    Pdf::open(path).map_err(|e| anyhow!("{e}"))
}

pub fn page_count(path: &str) -> Option<usize> {
    let pdf = open(path).ok()?;
    pdf.page_count().ok()
}

/// A page's text in reading order ("" when it can't be read).
pub fn page_text(pdf: &Pdf, index: usize) -> String {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pdf.extract_text(index))) {
        Ok(Ok(t)) => t,
        Ok(Err(_)) | Err(_) => String::new(),
    }
}

/// The document's title from its Info dictionary.
pub fn title(pdf: &Pdf) -> Option<String> {
    let info = pdf.trailer().as_dict()?.get("Info")?;
    let info = pdf.resolve_object(info).ok()?;
    let value = info.as_dict()?.get("Title")?.clone();
    let value = pdf.resolve_object(&value).ok()?;
    let t = decode_text_string(value.as_string()?);
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// A PDF text string: UTF-16BE with its BOM, UTF-8 with its BOM, or PDFDocEncoding (Latin-1 for what titles hold).
fn decode_text_string(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    b.iter().map(|&c| c as char).collect()
}

/// Tokens with at least two letters: words, not page numbers or stray glyphs.
fn word_count(text: &str) -> usize {
    text.split_whitespace().filter(|t| t.chars().filter(|&c| crate::uni::is_letter(c)).count() >= 2).count()
}

pub fn extract(ctx: &ExtractContext, cancel: &Cancel, sink: Sink<'_>) -> Result<ExtractEnd> {
    let pdf = open(&ctx.path)?;
    let pages = pdf.page_count().map_err(|e| anyhow!("{e}"))?.min(ctx.settings.max_pdf_pages.max(0) as usize);
    let title = title(&pdf).filter(|t| t.chars().count() > 2);

    let mut rendered: Option<WinPdf> = None;
    // Windows couldn't open the PDF for rendering (a password, a broken file): once is enough to know.
    let mut unrenderable = false;
    let end = pages.min(ctx.start_unit.saturating_add(ctx.unit_budget));
    for i in ctx.start_unit..end {
        cancel.check()?;
        let text = page_text(&pdf, i);
        let loc = format!("p. {}", i + 1);
        if word_count(&text) >= MIN_WORDS_PER_PAGE {
            // A page is one piece: its vector reads the first 4,000 characters (the embedder's cap), keyword search
            // all of it. Calibration and ranking count pages, not pieces of them.
            let page = text_chunker::normalize(&text);
            let keyword_only = !text_chunker::is_prose(&page);
            sink(ExtractedItem::text(chunk_kind::TEXT, page, Some(loc), Some((i + 1) as f64)).keyword_only(keyword_only))?;
            continue;
        }

        // A scanned page: rendered and treated as a picture.
        let img = if unrenderable {
            Ok(None)
        } else {
            (|| -> Result<Option<image::Decoded>> {
                if rendered.is_none() {
                    match WinPdf::LoadFromStreamAsync(&image::open_stream(&ctx.path)?).and_then(|op| op.join()) {
                        Ok(doc) => rendered = Some(doc),
                        Err(e) => {
                            unrenderable = true;
                            Log::warn(format!("PDF pages can't be rendered ({e}); their words are kept: {}", ctx.path));
                            return Ok(None);
                        }
                    }
                }
                let doc = rendered.as_ref().unwrap();
                let stream = render_page(doc, i as u32, 1700)?;
                let mode = if ctx.settings.ocr_images { OcrMode::Full } else { OcrMode::None };
                image::from_stream(&stream, mode, image::PAGE_TOKENS)
            })()
        };
        let img = match img {
            Ok(img) => img,
            Err(e) => {
                Log::warn(format!("PDF render p.{} of {}: {e}", i + 1, ctx.path));
                None
            }
        };
        let mut ocr_text = None;
        if let Some(img) = img {
            sink(ExtractedItem::media(chunk_kind::IMAGE, EmbedInput::Image(img.image), Some(loc.clone()), Some((i + 1) as f64)))?;
            ocr_text = img.ocr_text;
        }
        // The page's own few words count for keyword search even when its picture couldn't be made.
        let ocr = format!("{}\n{text}", ocr_text.unwrap_or_default());
        if ocr.trim().chars().count() >= 20 {
            for chunk in text_chunker::split(&ocr) {
                sink(ExtractedItem::text(chunk_kind::OCR, chunk, Some(loc.clone()), Some((i + 1) as f64)).keyword_only(true))?;
            }
        }
    }
    Ok(ExtractEnd { next_unit: (end < pages).then_some(end), title })
}

/// Page `index` (from 0) rendered with its longer side `pixels` long, as a PNG stream.
pub fn render_page(doc: &WinPdf, index: u32, pixels: u32) -> Result<IRandomAccessStream> {
    let page = doc.GetPage(index)?;
    let size = page.Size()?;
    let scale = pixels as f64 / (size.Width.max(size.Height) as f64).max(1.0);
    let options = PdfPageRenderOptions::new()?;
    options.SetDestinationWidth(((size.Width as f64 * scale) as u32).max(1))?;
    options.SetDestinationHeight(((size.Height as f64 * scale) as u32).max(1))?;
    let out = InMemoryRandomAccessStream::new()?;
    page.RenderWithOptionsToStreamAsync(&out, &options)?.join()?;
    page.Close()?;
    let stream: IRandomAccessStream = out.cast()?;
    stream.Seek(0)?;
    Ok(stream)
}

/// Opens a PDF for rendering (Windows.Data.Pdf).
pub fn open_rendered(path: &str) -> Result<WinPdf> {
    Ok(WinPdf::LoadFromStreamAsync(&image::open_stream(path)?)?.join()?)
}

/// The words of page `index` (from 0) with their boxes, each as (text, x, y, w, h) in shares of the page's width and
/// height, y from the top.
pub fn page_words(pdf: &Pdf, index: usize) -> Vec<(String, f64, f64, f64, f64)> {
    let Ok((x0, y0, x1, y1)) = pdf.get_page_media_box(index) else { return vec![] };
    let (w, h) = ((x1 - x0) as f64, (y1 - y0) as f64);
    if w <= 0.0 || h <= 0.0 {
        return vec![];
    }
    let words = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pdf.extract_words(index))) {
        Ok(Ok(words)) => words,
        _ => return vec![],
    };
    words
        .into_iter()
        .map(|word| {
            let b = word.bbox;
            // Boxes are in PDF space, y up: the top of the box is y + height.
            let top = (b.y + b.height - y0) as f64;
            (word.text, (b.x - x0) as f64 / w, 1.0 - top / h, b.width as f64 / w, b.height as f64 / h)
        })
        .collect()
}
