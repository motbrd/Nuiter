//! Search: keyword terms (`query_text`), names in code (`code_names`), the hybrid ranking (`engine`) and the word a PDF
//! viewer searches for when it opens at a page (`pdf_jump`).

pub mod code_names;
pub mod engine;
pub mod pdf_jump;
pub mod query_text;

pub use engine::{MatchKind, ParsedQuery, SearchEngine, SearchQuery, SearchResult};
