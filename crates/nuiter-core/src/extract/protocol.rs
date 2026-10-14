//! Reading files in a helper process: decoders (WIC, Windows OCR, Media Foundation, IFilter, PDF) run on whatever files
//! people have, and some hang, crash or grow without end on a few of them. In a helper that takes one file, not the app:
//! the app marks the file as unreadable and goes on. The helper also holds what reading needs (OCR models, decoders,
//! buffers) only while there's reading to do, and exits when the app stops sending files.
//!
//! Over the helper's stdin/stdout: frames of [u32 length][u8 type][payload]. The app sends `REQUEST` frames; for each,
//! the helper answers with `ITEM` frames and then one `END` (or `FAILED`). An item's payload is [u32 header length]
//! [JSON header][media bytes]. `CALL` asks for a small job (a page count, a preview), answered by `RESULT`.

use std::io::{Read, Write};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use super::{ExtractedItem, chunk_kind};
use crate::file_kinds::FileCategory;
use crate::runtime::EmbedInput;

pub const REQUEST: u8 = 1;
pub const ITEM: u8 = 2;
pub const END: u8 = 3;
pub const FAILED: u8 = 4;
pub const CALL: u8 = 5;
pub const RESULT: u8 = 6;

/// A frame bigger than this is a broken stream, not data.
const MAX_FRAME: u32 = 512 << 20;

#[derive(Debug, Serialize, Deserialize)]
pub struct RequestBody {
    pub path: String,
    pub category: FileCategory,
    pub start_unit: usize,
    pub unit_budget: usize,
    pub settings: crate::settings::Settings,
}

#[derive(Debug, Serialize, Deserialize)]
struct ItemHeader {
    kind: String,
    text: Option<String>,
    media: Option<String>,
    locator: Option<String>,
    position: Option<f64>,
    keyword_only: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndBody {
    pub next_unit: Option<usize>,
    pub title: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FailedBody {
    pub kind: String,
    pub message: String,
}

/// A small job besides reading a file for the index, for what the app shows: "pdf-pages" (count), "pdf-page" (a page
/// with the query's words marked), "video-frame" (the frame at a moment), "pdf-landmark" (the word to search for when
/// opening a PDF at a page).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallBody {
    pub op: String,
    pub path: String,
    #[serde(default)]
    pub page: usize,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub pixels: u32,
}

impl CallBody {
    pub fn new(op: &str, path: &str) -> Self {
        Self { op: op.into(), path: path.into(), page: 0, seconds: 0.0, query: None, pixels: 0 }
    }
}

/// A call's answer; `image` travels after the JSON, as an item's media does.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResultBody {
    pub pages: Option<usize>,
    pub word: Option<String>,
    pub marks: Option<Vec<[f64; 4]>>,
    #[serde(skip)]
    pub image: Option<Vec<u8>>,
}

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = [0u8; 5];
    head[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    head[4] = kind;
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

pub fn write_json<T: Serialize>(w: &mut impl Write, kind: u8, body: &T) -> std::io::Result<()> {
    write_frame(w, kind, &serde_json::to_vec(body).map_err(std::io::Error::other)?)
}

/// The next frame, or None at the end of the stream.
pub fn read_frame(r: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    if !read_exact_or_eof(r, &mut head)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    if len > MAX_FRAME {
        return Err(std::io::Error::other(format!("bad frame length {len}")));
    }
    let mut payload = vec![0u8; len as usize];
    if !read_exact_or_eof(r, &mut payload)? {
        return Ok(None);
    }
    Ok(Some((head[4], payload)))
}

fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut read = 0;
    while read < buf.len() {
        match r.read(&mut buf[read..]) {
            Ok(0) => return Ok(false),
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn with_header(header: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + header.len() + bytes.len());
    payload.extend_from_slice(&(header.len() as u32).to_le_bytes());
    payload.extend_from_slice(header);
    payload.extend_from_slice(bytes);
    payload
}

fn split_header(payload: &[u8]) -> Result<(&[u8], &[u8])> {
    if payload.len() < 4 {
        return Err(anyhow!("short payload"));
    }
    let n = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    if 4 + n > payload.len() {
        return Err(anyhow!("bad header length"));
    }
    Ok((&payload[4..4 + n], &payload[4 + n..]))
}

pub fn encode_item(item: &ExtractedItem) -> Vec<u8> {
    let (media, bytes): (Option<&str>, &[u8]) = match &item.media {
        Some(EmbedInput::Image(b)) => (Some("image"), b),
        Some(EmbedInput::Audio(b)) => (Some("audio"), b),
        _ => (None, &[]),
    };
    let header = serde_json::to_vec(&ItemHeader {
        kind: item.kind.into(),
        text: item.text.clone(),
        media: media.map(String::from),
        locator: item.locator.clone(),
        position: item.position,
        keyword_only: item.keyword_only,
    })
    .unwrap_or_default();
    with_header(&header, bytes)
}

pub fn decode_item(payload: &[u8]) -> Result<ExtractedItem> {
    let (header, bytes) = split_header(payload)?;
    let h: ItemHeader = serde_json::from_slice(header)?;
    let media = match h.media.as_deref() {
        Some("image") => Some(EmbedInput::Image(bytes.to_vec())),
        Some("audio") => Some(EmbedInput::Audio(bytes.to_vec())),
        _ => None,
    };
    Ok(ExtractedItem {
        kind: chunk_kind::intern(&h.kind),
        text: h.text,
        media,
        locator: h.locator,
        position: h.position,
        keyword_only: h.keyword_only,
    })
}

pub fn encode_result(r: &ResultBody) -> Vec<u8> {
    let header = serde_json::to_vec(r).unwrap_or_default();
    with_header(&header, r.image.as_deref().unwrap_or(&[]))
}

pub fn decode_result(payload: &[u8]) -> Result<ResultBody> {
    let (header, bytes) = split_header(payload)?;
    let mut r: ResultBody = serde_json::from_slice(header)?;
    if !bytes.is_empty() {
        r.image = Some(bytes.to_vec());
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_and_results_survive_the_protocol() {
        let item = ExtractedItem::media(chunk_kind::FRAME, EmbedInput::Image(vec![0x89, 0x50, 1, 2, 3]), Some("0:06".into()), Some(6.0));
        let back = decode_item(&encode_item(&item)).unwrap();
        assert_eq!(back, item);
        let text = ExtractedItem::text(chunk_kind::TEXT, "привет".into(), Some("p. 2".into()), Some(2.0)).keyword_only(true);
        assert_eq!(decode_item(&encode_item(&text)).unwrap(), text);

        let r = ResultBody { pages: Some(3), marks: Some(vec![[0.1, 0.2, 0.3, 0.4]]), image: Some(vec![9, 8, 7]), ..Default::default() };
        let back = decode_result(&encode_result(&r)).unwrap();
        assert_eq!(back.pages, Some(3));
        assert_eq!(back.image, Some(vec![9, 8, 7]));
        assert_eq!(back.marks.unwrap()[0][2], 0.3);

        let mut buf = vec![];
        write_frame(&mut buf, ITEM, b"abc").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), Some((ITEM, b"abc".to_vec())));
        assert_eq!(read_frame(&mut cursor).unwrap(), None);
    }
}
