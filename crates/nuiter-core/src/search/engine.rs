//! Hybrid search. Two kinds of evidence are scored per file and added up:
//! - meaning: the file's best chunk on the query's calibrated scale (`vectors::calibrate`), counted from zero; it alone
//!   makes a file a result from `MEANING_GATE` up;
//! - words: how much of the query the file holds, each word weighted by its rarity in the index (IDF), squared, so that
//!   most of the words count far more than a few. It alone makes a file a result from `WORDS_GATE` up. All the words
//!   side by side, as a phrase, earn `PHRASE_BONUS` more.
//!
//! A query of IDs and codes ("INV-2025-0345", "E-2207", "Garden-Guest") or a name looked up in code leans on words. A
//! file of the kind the query names ("…in a video") gets `KIND_BONUS`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use super::code_names;
use super::query_text::{self, KeywordTerm};
use crate::cancel::Cancel;
use crate::extract::chunk_kind;
use crate::file_kinds::FileCategory;
use crate::index::store::{ChunkInfo, IndexStore};
use crate::index::vectors::VectorIndex;
use crate::paths::Log;
use crate::runtime::{EmbedInput, EmbedPriority, Embedder, prompts};
use crate::uni::{lower_str, strip_prefix_ignore_case};

pub const MEANING_GATE: f64 = 2.0;
/// The share of the highest score n files allow, (n − 1) / √n, that a small index's gate asks for.
const SMALL_INDEX_GATE: f64 = 0.5;
pub const WORDS_GATE: f64 = 0.5;
const KEYWORD_SCALE: f64 = 3.0;
const CODE_KEYWORD_SCALE: f64 = 1.5;
const PHRASE_BONUS: f64 = 1.0;
const KIND_BONUS: f64 = 1.0;
const DEFINITION_BONUS: f64 = 1.0;
/// A word found only as the start of longer words ("zork" in "Zork3") counts this much of a whole one.
const PREFIX_CREDIT: f64 = 0.6;
/// Keyword search reads this many chunks, best first, and keeps the first `KEYWORD_FILES` files.
const KEYWORD_ROWS: usize = 1000;
const KEYWORD_FILES: usize = 100;
/// Results more than this far below the best are folded under it (`shown_count`).
pub const FOLD_BELOW_BEST: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum MatchKind {
    Semantic,
    Keyword,
    Both,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    pub category: FileCategory,
    pub score: f64,
    #[serde(rename = "match")]
    pub match_kind: MatchKind,
    pub kind: &'static str,
    pub locator: Option<String>,
    pub position: Option<f64>,
    pub snippet: Option<String>,
    /// Modification time, .NET ticks (UTC).
    pub modified: i64,
    pub size: i64,
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub text: String,
    pub category: Option<FileCategory>,
    pub semantic: bool,
    pub limit: usize,
    /// The last word is still being typed: it also matches as the start of longer words.
    pub prefix_last: bool,
}

impl SearchQuery {
    pub fn new(text: &str) -> Self {
        Self { text: text.to_string(), category: None, semantic: true, limit: 60, prefix_last: false }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedQuery {
    pub text: String,
    pub code: bool,
    pub category: Option<FileCategory>,
}

pub fn parse(raw: &str, mut category: Option<FileCategory>) -> ParsedQuery {
    let mut text = raw.trim();
    let mut code = false;
    if let Some(rest) = strip_prefix_ignore_case(text, "code:") {
        code = true;
        text = rest.trim();
    }
    let mut words = vec![];
    for w in text.split(' ').filter(|w| !w.is_empty()) {
        if let Some(kind) = strip_prefix_ignore_case(w, "type:") {
            category = match lower_str(kind).as_str() {
                "doc" | "docs" | "document" | "documents" => Some(FileCategory::Document),
                "pdf" => Some(FileCategory::Pdf),
                "image" | "images" | "photo" | "photos" | "img" => Some(FileCategory::Image),
                "audio" | "music" | "sound" => Some(FileCategory::Audio),
                "video" | "videos" | "movie" => Some(FileCategory::Video),
                "code" => Some(FileCategory::Code),
                _ => category,
            };
            continue;
        }
        words.push(w);
    }
    if category == Some(FileCategory::Code) {
        code = true;
        category = None;
    }
    ParsedQuery { text: words.join(" "), code, category }
}

/// The meaning gate for an index of `files` files. Among n files the best can stand at most (n − 1) / √n spreads above
/// their average, which is under `MEANING_GATE` for fewer than 6 files: a small index asks for half of that instead, so
/// its best match by meaning still shows. From about 20 files on it's `MEANING_GATE`.
pub fn gate_for(files: usize) -> f64 {
    if files < 2 {
        return f64::INFINITY;
    }
    let n = files as f64;
    MEANING_GATE.min(SMALL_INDEX_GATE * (n - 1.0) / n.sqrt())
}

/// How many of `results` (best first) to show before folding the rest: those within `FOLD_BELOW_BEST` of the best.
pub fn shown_count(results: &[SearchResult]) -> usize {
    match results.first() {
        None => 0,
        Some(first) => results.iter().take_while(|r| r.score >= first.score - FOLD_BELOW_BEST).count(),
    }
}

/// A query word, its weight (rarer words weigh more) and the share of that weight a file earns by holding it.
struct WeightedTerm {
    term: KeywordTerm,
    weight: f64,
    credit: f64,
}

pub struct SearchEngine {
    store: Arc<IndexStore>,
    vectors: Arc<VectorIndex>,
    embedder: Arc<dyn Embedder>,
}

impl SearchEngine {
    pub fn new(store: Arc<IndexStore>, vectors: Arc<VectorIndex>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, vectors, embedder }
    }

    pub fn search(&self, query: &SearchQuery, cancel: &Cancel) -> Result<Vec<SearchResult>> {
        let sw = Instant::now();
        let q = parse(&query.text, query.category);
        if q.text.is_empty() {
            return Ok(vec![]);
        }
        let store = &*self.store;

        let terms = self.weigh(query_text::terms(&q.text, query.prefix_last), q.code);
        let all_weight: f64 = terms.iter().map(|t| t.weight).sum();
        let plain: Vec<KeywordTerm> = terms.iter().map(|t| t.term.clone()).collect();
        let expr = query_text::match_expr(&plain);

        // Candidates by words: the best-ranked chunk of each of the first files keyword search finds.
        let mut best_keyword: HashMap<i64, i64> = HashMap::new(); // file → its best keyword chunk
        let mut keyword_files: Vec<i64> = vec![];
        let keyword_chunks = expr.as_ref().map(|e| store.keyword(e, q.code, KEYWORD_ROWS)).unwrap_or_default();
        let keyword_info = store.get_chunks(keyword_chunks.iter().map(|k| k.0));
        for (id, _) in &keyword_chunks {
            if best_keyword.len() >= KEYWORD_FILES {
                break;
            }
            if let Some(kc) = keyword_info.get(id)
                && !best_keyword.contains_key(&kc.file_id)
            {
                best_keyword.insert(kc.file_id, *id);
                keyword_files.push(kc.file_id);
            }
        }
        cancel.check()?;

        // Each candidate's share of the query's weight, over all its chunks (not only the best-ranked one), and
        // whether it holds the words side by side.
        let mut words: HashMap<i64, f64> = HashMap::new();
        if !best_keyword.is_empty() && all_weight > 0.0 {
            for t in &terms {
                let e = query_text::match_expr(std::slice::from_ref(&t.term)).unwrap();
                let files: HashSet<i64> = store.chunks_matching(&e, &keyword_files).into_iter().map(|m| m.1).collect();
                for f in files {
                    *words.entry(f).or_insert(0.0) += t.credit * t.weight / all_weight;
                }
            }
        }
        let phrase: HashSet<i64> = if terms.len() >= 2 && !best_keyword.is_empty() {
            store.chunks_matching(&query_text::adjacent(&plain), &keyword_files).into_iter().map(|m| m.1).collect()
        } else {
            HashSet::new()
        };

        let identifier = if q.code { code_names::name_in(&q.text) } else { None };
        let lookup = query_text::is_identifier_query(&q.text) || identifier.is_some();
        let word_weight = if q.code { CODE_KEYWORD_SCALE } else { KEYWORD_SCALE } * if lookup { 2.0 } else { 1.0 };
        let meaning_weight = if lookup { 0.5 } else { 1.0 };
        let hints = query_text::asked_kinds(&q.text);

        // Meaning: every file's best chunk on this query's scale.
        let mut meaning: HashMap<i64, (i64, f64)> = HashMap::new();
        let mut gate = MEANING_GATE;
        if query.semantic {
            // Not read yet, or released while nobody searched: read from the index. Also marks them used, so the
            // release timer can't drop them between here and the search below.
            self.vectors.ensure_loaded(|| store.vector_count(), |sink| store.load_vectors(sink));
        }
        if query.semantic && self.vectors.count() > 0 {
            let prompt = if q.code { prompts::code_query(&q.text) } else { prompts::query(&q.text) };
            let qv = self.embedder.embed(&[EmbedInput::Text(prompt)], EmbedPriority::Query, cancel)?.remove(0);
            cancel.check()?;
            let (hits, files) = self.vectors.search(&qv, q.code);
            for h in hits {
                meaning.insert(h.file_id, (h.chunk_id, h.z));
            }
            gate = gate_for(files);
        }

        let mut candidates: Vec<i64> = meaning.iter().filter(|(_, v)| v.1 >= gate).map(|(k, _)| *k).collect();
        for (f, share) in &words {
            if *share >= WORDS_GATE && !candidates.contains(f) {
                candidates.push(*f);
            }
        }
        struct Ranked {
            file: i64,
            chunk: i64,
            score: f64,
            kind: MatchKind,
        }
        let mut ranked: Vec<Ranked> = vec![];
        for file in candidates {
            let m = meaning.get(&file).copied();
            let z = m.map_or(0.0, |m| m.1);
            let share = words.get(&file).copied().unwrap_or(0.0);
            let keyword_chunk = best_keyword.get(&file).copied();
            // Point at the meaning's chunk when meaning made the file a result (or nothing else matched), else at the
            // words'.
            let pointed = match (m, keyword_chunk) {
                (Some(m), _) if z >= gate || keyword_chunk.is_none() => m.0,
                (_, Some(k)) => k,
                (Some(m), None) => m.0,
                (None, None) => continue,
            };
            let score = meaning_weight * z.max(0.0) + word_weight * share * share + if phrase.contains(&file) { PHRASE_BONUS } else { 0.0 };
            let kind = match (z >= gate, share >= WORDS_GATE) {
                (true, true) => MatchKind::Both,
                (true, false) => MatchKind::Semantic,
                _ => MatchKind::Keyword,
            };
            ranked.push(Ranked { file, chunk: pointed, score, kind });
        }
        let mut chunks = store.get_chunks(ranked.iter().map(|r| r.chunk));
        ranked.retain(|r| chunks.get(&r.chunk).is_some_and(|c| q.category.is_none_or(|cat| c.category == cat)));
        for r in &mut ranked {
            if hints.contains(&chunks[&r.chunk].category) {
                r.score += KIND_BONUS;
            }
        }
        ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.file.cmp(&b.file)));
        ranked.truncate(query.limit);
        for (id, c) in keyword_info {
            chunks.entry(id).or_insert(c);
        }

        let stems: Vec<String> = terms.iter().map(|t| t.term.stem.clone()).collect();
        let mut results: Vec<SearchResult> = Vec::with_capacity(ranked.len());
        for r in &ranked {
            let mut score = r.score;
            let mut c = chunks[&r.chunk].clone();
            let mut position = c.position;
            if c.category == FileCategory::Code {
                let words_here = best_keyword.get(&c.file_id) == Some(&c.chunk_id);
                // Definitions are looked for in the first results only: a common name is in hundreds of chunks.
                let id = if results.len() < 10 { identifier.as_deref() } else { None };
                let (place, line, defined) = self.open_at(c, id, &terms, words_here);
                c = place;
                position = line;
                if defined {
                    score += DEFINITION_BONUS;
                }
            }
            let mut text = if c.kind == chunk_kind::NAME { None } else { c.text.clone() };
            // A picture's vector has no text of its own: the OCR words keyword search matched describe the same
            // picture (on the same page, for a scanned PDF). Never another page's words.
            if text.is_none()
                && let Some(bk) = best_keyword.get(&c.file_id)
                && let Some(kc) = chunks.get(bk)
                && kc.kind != chunk_kind::NAME
                && kc.position == c.position
            {
                text = kc.text.clone();
            }
            if text.is_none() && c.position.is_none() {
                text = store.first_text(c.file_id);
            }
            results.push(SearchResult {
                path: c.path.clone(),
                name: c.name.clone(),
                category: c.category,
                score,
                match_kind: r.kind,
                kind: c.kind,
                locator: c.locator.clone(),
                position,
                snippet: snippet(text.as_deref(), &stems, 220),
                modified: c.mtime,
                size: c.size,
            });
        }
        if identifier.is_some() {
            results.sort_by(|a, b| b.score.total_cmp(&a.score));
        }
        // What was searched for stays out of the log (it's a plain text file): only its shape.
        Log::info(format!(
            "Search ({} words{}) → {} results ({} by meaning, {} by words) in {} ms",
            q.text.split_whitespace().count(),
            if q.code { ", code" } else { "" },
            results.len(),
            meaning.values().filter(|s| s.1 >= gate).count(),
            words.values().filter(|&&s| s >= WORDS_GATE).count(),
            sw.elapsed().as_millis()
        ));
        Ok(results)
    }

    /// The query's words with their weights: the inverse document frequency of each over the files of this search
    /// (code or not), ln(1 + (N + 1) / (df + 0.5)). A word in no file as typed is tried as the start of longer words
    /// ("zork" finds "Zork3"), earning `PREFIX_CREDIT` of its weight. A word in no file at all weighs as one in a single
    /// file: it tells files apart no better than that, but no file holds all of the query.
    fn weigh(&self, terms: Vec<KeywordTerm>, code: bool) -> Vec<WeightedTerm> {
        let files = self.store.file_count(code) as f64;
        terms
            .into_iter()
            .map(|term| {
                let mut t = term;
                let mut credit = 1.0;
                let mut df = self.store.file_frequency(&query_text::match_expr(std::slice::from_ref(&t)).unwrap(), code, 20_000);
                if df == 0 && !t.prefix && t.stem.chars().count() >= 3 {
                    let prefixed = KeywordTerm { prefix: true, ..t.clone() };
                    let pdf = self.store.file_frequency(&query_text::match_expr(std::slice::from_ref(&prefixed)).unwrap(), code, 20_000);
                    if pdf > 0 {
                        t = prefixed;
                        df = pdf;
                        credit = PREFIX_CREDIT;
                    }
                }
                let weight = (1.0 + (files + 1.0) / (df.max(1) as f64 + 0.5)).ln();
                WeightedTerm { term: t, weight, credit }
            })
            .collect()
    }

    /// Code: the piece to point at and the line to open. A name looked up points at the piece that defines it, if any
    /// of the file's pieces does; otherwise, when the query's words matched this piece, the line that holds most of
    /// them. A match by meaning alone opens at the piece's first line.
    fn open_at(&self, c: ChunkInfo, identifier: Option<&str>, terms: &[WeightedTerm], words_here: bool) -> (ChunkInfo, Option<f64>, bool) {
        if let Some(id) = identifier {
            let name = code_names::last_part(id);
            let ids: Vec<i64> =
                self.store.chunks_matching(&query_text::adjacent(&query_text::terms(id, false)), &[c.file_id]).into_iter().map(|m| m.0).collect();
            let mut pieces: Vec<ChunkInfo> = self
                .store
                .get_chunks(ids)
                .into_values()
                .filter(|x| x.kind == chunk_kind::CODE && x.position.is_some() && x.text.is_some())
                .collect();
            pieces.sort_by(|a, b| a.position.unwrap().total_cmp(&b.position.unwrap()));
            let mut best: Option<(ChunkInfo, usize, u8)> = None;
            for x in pieces {
                let Some((line, strength)) =
                    code_names::find_definition(name, x.text.as_deref().unwrap(), x.position.unwrap() as usize)
                else {
                    continue;
                };
                if strength <= best.as_ref().map_or(0, |b| b.2) {
                    continue;
                }
                let done = strength == 2;
                best = Some((x, line, strength));
                if done {
                    break;
                }
            }
            if let Some((chunk, line, strength)) = best {
                return (chunk, Some(line as f64), strength == 2);
            }
        }
        let Some(first) = c.position else { return (c.clone(), c.position, false) };
        if c.kind != chunk_kind::CODE || c.text.is_none() || !(words_here || identifier.is_some()) {
            return (c.clone(), c.position, false);
        }
        let line = match identifier {
            Some(id) => code_names::best_line([code_names::last_part(id)], c.text.as_deref().unwrap(), first as usize),
            None => code_names::best_line(terms.iter().map(|t| t.term.word.as_str()), c.text.as_deref().unwrap(), first as usize),
        };
        let pos = Some(line.map_or(first, |l| l as f64));
        (c, pos, false)
    }
}

/// A ~220 character excerpt centred on the first query term found.
pub fn snippet(text: Option<&str>, terms: &[String], length: usize) -> Option<String> {
    let text = text?;
    if text.trim().is_empty() {
        return None;
    }
    let text: String = text.replace('\n', " ").replace("  ", " ");
    let chars: Vec<char> = text.chars().collect();
    let lowered: Vec<char> = chars.iter().map(|&c| crate::uni::lower(c)).collect();
    let mut idx: Option<usize> = None;
    for t in terms.iter().filter(|t| t.chars().count() >= 2) {
        let needle: Vec<char> = t.chars().map(crate::uni::lower).collect();
        idx = crate::uni::find_chars(&lowered, &needle, 0);
        if idx.is_some() {
            break;
        }
    }
    if chars.len() <= length {
        return Some(text);
    }
    let mut start = idx.map_or(0, |i| i.saturating_sub(length / 3));
    let end = chars.len().min(start + length);
    if start > 0
        && let Some(sp) = chars[start..].iter().position(|&c| c == ' ').map(|p| p + start)
        && sp > 0
        && sp < start + 25
    {
        start = sp + 1;
    }
    let body: String = chars[start..end.max(start)].iter().collect();
    Some(format!("{}{}{}", if start > 0 { "…" } else { "" }, body.trim(), if end < chars.len() { "…" } else { "" }))
}

#[cfg(test)]
pub(crate) mod tests;
