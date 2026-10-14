//! The model runtime: llama.cpp's llama-server as a helper process (`worker`), its download (`installer`), new
//! releases of Nuiter (`updates`) and the vectors a GPU is checked against (`reference`).

pub mod cpu;
pub mod installer;
pub mod job;
pub mod llama_http;
pub mod reference;
pub mod updates;
pub mod worker;

use crate::cancel::Cancel;

/// What the model embeds: text, or a picture (PNG/JPEG already sized for the vision encoder), or a recording (WAV).
#[derive(Debug, Clone, PartialEq)]
pub enum EmbedInput {
    Text(String),
    Image(Vec<u8>),
    Audio(Vec<u8>),
}

impl EmbedInput {
    pub fn image_mime(bytes: &[u8]) -> &'static str {
        if bytes.len() > 3 && bytes[0] == 0x89 && bytes[1] == 0x50 { "image/png" } else { "image/jpeg" }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedPriority {
    Query,
    Indexing,
}

/// The model refused an input, failed on it, or isn't there to ask.
#[derive(Debug, Clone)]
pub struct EmbeddingError {
    pub message: String,
    /// The model isn't running and couldn't be started (or isn't installed, or the app is closing): it says nothing
    /// about the input, which is tried again later rather than marked unreadable.
    pub unavailable: bool,
}

impl EmbeddingError {
    /// The model ran and failed on (or refused) what it was given.
    pub fn refused(message: impl Into<String>) -> Self {
        Self { message: message.into(), unavailable: false }
    }

    /// The model couldn't be asked at all.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self { message: message.into(), unavailable: true }
    }

    /// Whether `e` is an `EmbeddingError` saying the model couldn't be asked.
    pub fn is_unavailable(e: &anyhow::Error) -> bool {
        e.downcast_ref::<EmbeddingError>().is_some_and(|x| x.unavailable)
    }
}

impl std::fmt::Display for EmbeddingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EmbeddingError {}

pub trait Embedder: Send + Sync {
    fn dimensions(&self) -> usize {
        768
    }

    /// One unit vector per input, in order. Errors are `EmbeddingError` (or `Cancelled`).
    fn embed(&self, inputs: &[EmbedInput], priority: EmbedPriority, cancel: &Cancel) -> anyhow::Result<Vec<Vec<f32>>>;

    /// When the model last became ready (`Instant`): work that waited for a start doesn't say what indexing costs.
    fn ready_since(&self) -> Option<std::time::Instant> {
        None
    }
}

/// EmbeddingGemma 2 task prompts (text only; images, audio and video frames get no prefix).
pub mod prompts {
    pub fn query(q: &str) -> String {
        format!("task: search result | query: {q}")
    }

    pub fn code_query(q: &str) -> String {
        format!("task: code retrieval | query: {q}")
    }

    pub fn document(title: Option<&str>, text: &str) -> String {
        let t = match title {
            Some(t) if !t.trim().is_empty() => t,
            _ => "none",
        };
        format!("title: {t} | text: {text}")
    }
}
