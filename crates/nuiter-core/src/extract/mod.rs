//! Reading files: what goes into the index from each kind of file (`extract`), in this process or in reader helpers
//! (`host`, `service`, `protocol`).

pub mod bicubic;
pub mod code_chunker;
pub mod document_text;
pub mod email;
pub mod host;
pub mod ifilter;
pub mod image;
pub mod media;
pub mod mf;
pub mod ogg_audio;
pub mod pdf;
pub mod preview;
pub mod protocol;
pub mod resample;
pub mod service;
pub mod text_chunker;

use anyhow::Result;

use crate::cancel::Cancel;
use crate::file_kinds::{FileCategory, pathx};
use crate::paths::Log;
use crate::runtime::EmbedInput;
use crate::settings::Settings;
use crate::uni::lower_str;

/// What a stored chunk represents. Persisted as text in the database.
pub mod chunk_kind {
    /// Document / PDF text.
    pub const TEXT: &str = "text";
    /// Text recognised in an image, scanned page or video frame.
    pub const OCR: &str = "ocr";
    /// An image or a rendered PDF page.
    pub const IMAGE: &str = "image";
    /// A video frame.
    pub const FRAME: &str = "frame";
    /// An audio window (also video soundtracks).
    pub const AUDIO: &str = "audio";
    /// A code region.
    pub const CODE: &str = "code";
    /// File name only (keyword search).
    pub const NAME: &str = "name";

    /// The constant for a kind read back from the database or a helper.
    pub fn intern(kind: &str) -> &'static str {
        match kind {
            OCR => OCR,
            IMAGE => IMAGE,
            FRAME => FRAME,
            AUDIO => AUDIO,
            CODE => CODE,
            NAME => NAME,
            _ => TEXT,
        }
    }
}

/// One unit to embed: text (text kinds) or media (image/audio kinds).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedItem {
    pub kind: &'static str,
    pub text: Option<String>,
    pub media: Option<EmbedInput>,
    pub locator: Option<String>,
    pub position: Option<f64>,
    /// Text that is kept for keyword search but not embedded (mostly-numeric data, OCR).
    pub keyword_only: bool,
}

impl ExtractedItem {
    pub fn text(kind: &'static str, text: String, locator: Option<String>, position: Option<f64>) -> Self {
        Self { kind, text: Some(text), media: None, locator, position, keyword_only: false }
    }

    pub fn media(kind: &'static str, media: EmbedInput, locator: Option<String>, position: Option<f64>) -> Self {
        Self { kind, text: None, media: Some(media), locator, position, keyword_only: false }
    }

    /// The file-name row of keyword search.
    pub fn name() -> Self {
        Self { kind: chunk_kind::NAME, text: None, media: None, locator: None, position: None, keyword_only: false }
    }

    pub fn keyword_only(mut self, k: bool) -> Self {
        self.keyword_only = k;
        self
    }
}

/// What to read of a file.
#[derive(Debug, Clone)]
pub struct ExtractContext {
    pub path: String,
    pub settings: Settings,
    /// First unit (0-based) to extract: a PDF page, or a document's chunk. Long files are read a step at a time.
    pub start_unit: usize,
    /// Stop after this many pages or chunks; the extractor then says where to go on (`ExtractEnd::next_unit`).
    pub unit_budget: usize,
}

impl ExtractContext {
    pub fn new(path: &str, settings: Settings) -> Self {
        Self { path: path.to_string(), settings, start_unit: 0, unit_budget: usize::MAX }
    }
}

/// Where a step of a long file stopped, and the title the file gives itself (a PDF's).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtractEnd {
    pub next_unit: Option<usize>,
    pub title: Option<String>,
}

/// Where items go as they're read. An error from it (the model failed, a cancel) stops the reading.
pub type Sink<'a> = &'a mut dyn FnMut(ExtractedItem) -> Result<()>;

/// A bigger code file (or notebook's cells) was written by a program, not a person: not read.
const MAX_CODE_CHARS: usize = 2_000_000;

/// Reads a file in this process.
pub fn extract(ctx: &ExtractContext, category: FileCategory, cancel: &Cancel, sink: Sink<'_>) -> Result<ExtractEnd> {
    match category {
        FileCategory::Document => {
            // Chunks are cut the same way every time, so a later step goes on where the last one stopped. Each carries
            // its number (from 1), which says how far the file has been read.
            let sections = document_text::extract(&ctx.path)?;
            let mut number = 0usize;
            let end = ctx.start_unit.saturating_add(ctx.unit_budget);
            for (section, text) in sections {
                for chunk in text_chunker::split(&text) {
                    if number >= text_chunker::MAX_CHUNKS_PER_DOCUMENT {
                        return Ok(ExtractEnd::default());
                    }
                    if number >= end {
                        return Ok(ExtractEnd { next_unit: Some(number), title: None });
                    }
                    number += 1;
                    if number <= ctx.start_unit {
                        continue;
                    }
                    cancel.check()?;
                    let keyword_only = !text_chunker::is_prose(&chunk);
                    sink(ExtractedItem::text(chunk_kind::TEXT, chunk, section.clone(), Some(number as f64)).keyword_only(keyword_only))?;
                }
            }
            Ok(ExtractEnd::default())
        }
        FileCategory::Code => {
            if lower_str(pathx::extension(&ctx.path)) == ".ipynb" {
                // A notebook's cells (code and notes, never outputs). Editors can't be told a cell, so it opens at its top.
                let cells = code_chunker::read_notebook(&document_text::read_text(&ctx.path)?);
                let all: String = cells.iter().map(|c| c.source.as_str()).collect::<Vec<_>>().join("\n");
                if all.chars().count() > MAX_CODE_CHARS || code_chunker::why_left_out(&all, true).is_some() {
                    return Ok(ExtractEnd::default());
                }
                for p in code_chunker::split_notebook(&cells) {
                    let loc = if p.first == p.last { format!("cell {}", p.first) } else { format!("cells {}–{}", p.first, p.last) };
                    let keyword_only = !text_chunker::is_prose(&p.text);
                    sink(ExtractedItem::text(chunk_kind::CODE, p.text, Some(loc), None).keyword_only(keyword_only))?;
                }
                return Ok(ExtractEnd::default());
            }
            let text = document_text::read_text(&ctx.path)?;
            if text.chars().count() > MAX_CODE_CHARS || text.contains('\0') {
                return Ok(ExtractEnd::default());
            }
            if let Some(why) = code_chunker::why_left_out(&text, is_source(&ctx.path)) {
                Log::info(format!("Code left out ({why}): {}", ctx.path));
                return Ok(ExtractEnd::default());
            }
            for p in code_chunker::split(&text) {
                cancel.check()?;
                let loc = if p.first == p.last { format!("L{}", p.first) } else { format!("L{}–{}", p.first, p.last) };
                let keyword_only = !text_chunker::is_prose(&p.text);
                sink(ExtractedItem::text(chunk_kind::CODE, p.text, Some(loc), Some(p.first as f64)).keyword_only(keyword_only))?;
            }
            Ok(ExtractEnd::default())
        }
        FileCategory::Pdf => pdf::extract(ctx, cancel, sink),
        FileCategory::Image => {
            let ocr = image::mode_for(&ctx.path, ctx.settings.ocr_images);
            let Some(img) = image::from_file(&ctx.path, ocr, image::budget_for(&ctx.path))? else {
                return Ok(ExtractEnd::default());
            };
            sink(ExtractedItem::media(chunk_kind::IMAGE, EmbedInput::Image(img.image), None, None))?;
            if let Some(text) = img.ocr_text
                && text.trim().chars().count() >= 12
            {
                for chunk in text_chunker::split(&text) {
                    sink(ExtractedItem::text(chunk_kind::OCR, chunk, None, None).keyword_only(true))?;
                }
            }
            Ok(ExtractEnd::default())
        }
        FileCategory::Audio => {
            media::audio(&ctx.path, cancel, sink)?;
            Ok(ExtractEnd::default())
        }
        FileCategory::Video => {
            let mut items = 0usize;
            let mut counted = |item: ExtractedItem| {
                items += 1;
                sink(item)
            };
            let unreadable = media::video_frames(&ctx.path, ctx.settings.ocr_images, cancel, &mut counted)?;
            media::audio(&ctx.path, cancel, &mut counted)?;
            // Nothing seen and nothing heard: the file couldn't be read (it's still found by its name).
            match unreadable {
                Some(why) if items == 0 => Err(anyhow::anyhow!(why)),
                _ => Ok(ExtractEnd::default()),
            }
        }
        FileCategory::None => Ok(ExtractEnd::default()),
    }
}

/// Code, as opposed to a repo's docs and settings (Markdown, reStructuredText, TOML, YAML).
fn is_source(path: &str) -> bool {
    !matches!(lower_str(pathx::extension(path)).as_str(), ".md" | ".rst" | ".txt" | ".toml" | ".yaml" | ".yml")
}
