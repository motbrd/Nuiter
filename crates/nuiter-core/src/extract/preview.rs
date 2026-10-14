//! The preview pane's big pictures: the PDF page that matched with the query's words marked on it, the way a viewer
//! marks what you searched for, and the video frame at the moment that matched.

use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapEncoder, BitmapInterpolationMode, BitmapPixelFormat};
use windows::Storage::Streams::{IRandomAccessStream, InMemoryRandomAccessStream};
use windows::core::Interface;

use super::{image, mf::VideoFrames, pdf};
use crate::paths::Log;
use crate::search::{engine, query_text};

/// Page `page` (from 1) as a PNG, its longer side `pixels`, and where the query's words are on it (x, y, w, h as shares
/// of its width and height).
pub fn pdf_page(path: &str, page: usize, query: &str, pixels: u32) -> Option<(Vec<u8>, Vec<[f64; 4]>)> {
    let mut marks = vec![];
    let terms: Vec<_> = query_text::terms(&engine::parse(query, None).text, false).into_iter().filter(|t| t.stem.chars().count() >= 2).collect();
    if !terms.is_empty()
        && page >= 1
        && let Ok(doc) = pdf::open(path)
        && page <= doc.page_count().unwrap_or(0)
    {
        for (text, x, y, w, h) in pdf::page_words(&doc, page - 1) {
            let t = query_text::normalize(text.trim_matches(['.', ',', ';', ':', '!', '?', '(', ')', '[', ']', '«', '»', '"', '\'']));
            if !terms.iter().any(|term| if term.prefix { t.starts_with(&term.stem) } else { t == term.stem }) {
                continue;
            }
            marks.push([x, y, w, h]);
            if marks.len() >= 200 {
                break;
            }
        }
    }
    let rendered = (|| -> anyhow::Result<Vec<u8>> {
        let doc = pdf::open_rendered(path)?;
        if page < 1 || page as u32 > doc.PageCount()? {
            anyhow::bail!("no page {page}");
        }
        let stream = pdf::render_page(&doc, (page - 1) as u32, pixels)?;
        image::read_stream(&stream)
    })();
    match rendered {
        Ok(png) => Some((png, marks)),
        Err(e) => {
            Log::warn(format!("Preview of p.{page} of {path}: {e}"));
            None
        }
    }
}

/// The video frame at `seconds`, `width` pixels wide at most, as a JPEG.
pub fn video_frame(path: &str, seconds: f64, width: u32) -> Option<Vec<u8>> {
    let r = (|| -> anyhow::Result<Option<Vec<u8>>> {
        let video = VideoFrames::open(path)?;
        let at = seconds.clamp(0.0, (video.duration - 0.1).max(0.0));
        let Some((pixels, w, h)) = video.frame_at(at)? else { return Ok(None) };
        drop(video);
        let out = InMemoryRandomAccessStream::new()?;
        let stream: IRandomAccessStream = out.cast()?;
        let encoder = BitmapEncoder::CreateAsync(BitmapEncoder::JpegEncoderId()?, &stream)?.join()?;
        encoder.SetPixelData(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Ignore, w, h, 96.0, 96.0, &pixels)?;
        let scale = (width as f64 / w as f64).min(1.0);
        let t = encoder.BitmapTransform()?;
        t.SetScaledWidth(((w as f64 * scale) as u32).max(1))?;
        t.SetScaledHeight(((h as f64 * scale) as u32).max(1))?;
        t.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
        encoder.FlushAsync()?.join()?;
        Ok(Some(image::read_stream(&stream)?))
    })();
    r.unwrap_or_else(|e| {
        Log::warn(format!("Preview frame {seconds:.0}s of {path}: {e}"));
        None
    })
}
