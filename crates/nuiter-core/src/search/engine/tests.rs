use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::extract::ExtractedItem;
use crate::index::store::status;
use crate::index::vectors::{Modality, StoredVector, calibrate, length_allowance_for};
use crate::vector_math;

/// Deterministic embedder: a bag-of-words hashed into 768 dims, so texts sharing words are similar.
#[derive(Default)]
pub(crate) struct FakeEmbedder {
    pub calls: AtomicUsize,
}

impl FakeEmbedder {
    pub fn vec(text: &str) -> Vec<f32> {
        let mut v = vec![0f32; 768];
        // A shared background, the same for every text.
        let mut seed = 7u64;
        for x in v.iter_mut() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *x = ((seed >> 33) as f64 / (1u64 << 31) as f64 * 0.02) as f32;
        }
        for w in text.to_lowercase().split([' ', '.', ',']).filter(|w| !w.is_empty()) {
            let mut h = 0xcbf29ce484222325u64;
            for b in w.bytes() {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
            v[(h % 768) as usize] += 1.0;
        }
        vector_math::normalize(&mut v);
        v
    }
}

impl Embedder for FakeEmbedder {
    fn embed(&self, inputs: &[EmbedInput], _: EmbedPriority, _: &Cancel) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(inputs
            .iter()
            .map(|i| match i {
                EmbedInput::Text(t) => {
                    let body = match (t.find("| query: "), t.find("| text: ")) {
                        (Some(q), _) => &t[q + 9..],
                        (None, Some(x)) => &t[x + 8..],
                        _ => t.as_str(),
                    };
                    Self::vec(body)
                }
                _ => Self::vec("picture"),
            })
            .collect())
    }
}

pub(crate) struct Fixture {
    pub dir: std::path::PathBuf,
    pub store: Arc<IndexStore>,
    pub vectors: Arc<VectorIndex>,
    pub embedder: Arc<FakeEmbedder>,
}

impl Fixture {
    pub fn new(name: &str) -> Fixture {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("nuiter-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(IndexStore::open(&dir.join("index.db")).unwrap());
        Fixture { dir, store, vectors: Arc::new(VectorIndex::default()), embedder: Arc::new(FakeEmbedder::default()) }
    }

    pub fn engine(&self) -> SearchEngine {
        SearchEngine::new(self.store.clone(), self.vectors.clone(), self.embedder.clone())
    }

    pub fn add_doc(&self, path: &str, text: &str, cat: FileCategory, kind: &'static str) {
        let (id, _) = self.store.begin_file(path, cat, false, text.len() as i64, 1).unwrap();
        let name = crate::index::indexer::name_tokens(crate::file_kinds::pathx::file_name(path));
        self.store.add_chunks(id, false, &[(&ExtractedItem::name(), None)], Some(&name)).unwrap();
        let v = FakeEmbedder::vec(text);
        let item = ExtractedItem::text(kind, text.to_string(), None, None);
        let stored = self.store.add_chunks(id, false, &[(&item, Some(&v))], None).unwrap();
        self.vectors.add(stored);
        self.store.finish_file(id, status::DONE, None, None, None);
    }

    pub fn doc(&self, path: &str, text: &str) {
        self.add_doc(path, text, FileCategory::Document, chunk_kind::TEXT);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.store.release_memory();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn q(text: &str, semantic: bool) -> SearchQuery {
    SearchQuery { semantic, ..SearchQuery::new(text) }
}

fn run(f: &Fixture, query: SearchQuery) -> Vec<SearchResult> {
    f.engine().search(&query, &Cancel::never()).unwrap()
}

#[test]
fn semantic_match_ranks_first() {
    let f = Fixture::new("search");
    f.doc(r"C:\d\heater.txt", "install the electric water heater tank on the wall");
    f.doc(r"C:\d\pie.txt", "bake the apple pie with cinnamon");
    for i in 0..20 {
        f.doc(&format!(r"C:\d\noise{i}.txt"), &format!("unrelated filler number{i} lorem ipsum"));
    }
    let results = run(&f, SearchQuery::new("water heater install"));
    assert!(!results.is_empty());
    assert!(results[0].path.ends_with("heater.txt"));
    assert!(!results.iter().any(|r| r.path.ends_with("pie.txt")));
}

#[test]
fn exact_codes_match_by_keyword() {
    let f = Fixture::new("search");
    f.doc(r"C:\d\log.txt", "ERROR upstream timeout code E-4417 while charging");
    f.doc(r"C:\d\other.txt", "everything is fine today");
    let results = run(&f, q("E-4417", false));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].match_kind, MatchKind::Keyword);
    assert!(results[0].snippet.as_deref().unwrap().contains("E-4417"));
    assert_eq!(f.embedder.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn file_names_are_searchable_including_camel_case_parts() {
    let f = Fixture::new("search");
    f.doc(r"C:\d\QuarterlyReport_2026.docx", "numbers");
    assert_eq!(run(&f, q("report", false)).len(), 1);
}

#[test]
fn category_filter_and_type_prefix_apply() {
    let f = Fixture::new("search");
    f.doc(r"C:\d\a.txt", "sunset beach");
    f.add_doc(r"C:\d\b.pdf", "sunset beach", FileCategory::Pdf, chunk_kind::TEXT);
    let pdfs = run(&f, q("sunset type:pdf", false));
    assert_eq!(pdfs.len(), 1);
    assert_eq!(pdfs[0].category, FileCategory::Pdf);
}

#[test]
fn deleting_a_file_removes_its_vectors_and_keywords() {
    let f = Fixture::new("search");
    f.doc(r"C:\d\gone.txt", "temporary zebra content");
    assert!(!run(&f, q("zebra", false)).is_empty());
    f.vectors.remove(f.store.delete_file(r"C:\d\gone.txt"));
    assert!(run(&f, SearchQuery::new("zebra")).is_empty());
    assert_eq!(f.vectors.count(), 0);
}

#[test]
fn query_parsing_handles_code_prefix_and_types() {
    let p = parse("code: retry failed request", None);
    assert!(p.code);
    assert_eq!(p.text, "retry failed request");
    let t = parse("dog type:photo", None);
    assert_eq!(t.category, Some(FileCategory::Image));
    assert_eq!(t.text, "dog");
}

#[test]
fn vector_index_reuses_slots_and_keeps_code_apart() {
    let idx = VectorIndex::default();
    let noise = |code: bool| -> Vec<StoredVector> {
        (100..130)
            .map(|i| {
                let id = i + if code { 100 } else { 0 };
                StoredVector::new(id, id, chunk_kind::TEXT, code, &FakeEmbedder::vec(&format!("unrelated words {i}")))
            })
            .collect()
    };
    idx.add(noise(false));
    idx.add(noise(true));
    idx.add(vec![
        StoredVector::new(1, 1, chunk_kind::TEXT, false, &FakeEmbedder::vec("cat")),
        StoredVector::new(2, 2, chunk_kind::IMAGE, false, &FakeEmbedder::vec("cat")),
    ]);
    idx.remove(vec![1]);
    idx.add(vec![StoredVector::new(3, 3, chunk_kind::TEXT, true, &FakeEmbedder::vec("cat"))]);
    assert_eq!(idx.count(), 62);
    let (top, _) = idx.search(&FakeEmbedder::vec("cat"), false);
    assert_eq!(top[0].file_id, 2);
    assert!(!top.iter().any(|h| h.file_id == 3));
    assert_eq!(idx.search(&FakeEmbedder::vec("cat"), true).0[0].chunk_id, 3);
}

#[test]
fn vector_index_returns_every_files_best_chunk() {
    let idx = VectorIndex::default();
    idx.add((1..=300).map(|i| StoredVector::new(i, 1, chunk_kind::TEXT, false, &FakeEmbedder::vec(&format!("garden roses soil part{i}")))).collect());
    idx.add(vec![StoredVector::new(1000, 2, chunk_kind::TEXT, false, &FakeEmbedder::vec("garden roses notes"))]);
    idx.add((0..200).map(|i| StoredVector::new(2000 + i, 10 + i, chunk_kind::TEXT, false, &FakeEmbedder::vec(&format!("tax invoice {i}")))).collect());
    let (hits, _) = idx.search(&FakeEmbedder::vec("garden roses soil"), false);
    let mut top: Vec<i64> = hits.iter().take(2).map(|h| h.file_id).collect();
    top.sort();
    assert_eq!(top, vec![1, 2]);
}

#[test]
fn released_vectors_load_again_and_changes_made_during_the_load_follow_it() {
    let v = |id: i64, text: &str| StoredVector::new(id, id, chunk_kind::TEXT, false, &FakeEmbedder::vec(text));
    let idx = VectorIndex::default();
    idx.add(vec![v(1, "cat"), v(2, "dog")]);
    idx.release();
    assert!(!idx.loaded());
    assert_eq!(idx.count(), 0);
    idx.add(vec![v(9, "ignored: the database has it")]);
    assert_eq!(idx.count(), 0);
    idx.ensure_loaded(|| 2, |sink| {
        sink(v(1, "cat"));
        sink(v(2, "dog"));
        // The indexer adds and removes while the load runs: they wait for it, then apply.
        idx.add(vec![v(3, "bird")]);
        idx.remove(vec![2]);
    });
    assert!(idx.loaded());
    assert_eq!(idx.count(), 2);
    assert!(idx.search(&FakeEmbedder::vec("bird"), false).0.iter().any(|h| h.chunk_id == 3));
    assert!(!idx.search(&FakeEmbedder::vec("dog"), false).0.iter().any(|h| h.chunk_id == 2));
    idx.ensure_loaded(|| 0, |_| panic!("already loaded"));
}

#[test]
fn a_small_index_asks_less_of_a_meaning_match() {
    assert!(gate_for(1).is_infinite());
    assert!((0.7..=0.8).contains(&gate_for(4)));
    assert!(gate_for(10) < MEANING_GATE);
    assert_eq!(gate_for(20), MEANING_GATE);
    assert_eq!(gate_for(100_000), MEANING_GATE);
}

#[test]
fn every_file_counts_once_in_a_querys_scale() {
    let scores = [0.5f32, 0.1, 0.1, 0.1];
    let modality = [Modality::Text; 4];
    let live = [true; 4];
    let by_chunk = calibrate(&scores, &modality, &[1.0; 4], &live);
    let by_file = calibrate(&scores, &modality, &[1.0, 1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0], &live);
    assert!(by_file[0] < by_chunk[0]);
    assert_eq!(by_file[2], by_file[3]);
    assert!(calibrate(&scores, &modality, &[1.0; 4], &[true, false, true, true])[1].is_infinite());
    assert_eq!(length_allowance_for(1), 0.0);
    assert!((0.6..=0.75).contains(&length_allowance_for(40)));
    assert!(length_allowance_for(1000) > length_allowance_for(40));
}

#[test]
fn a_whole_word_in_the_index_stays_whole_and_an_unknown_one_matches_as_a_prefix() {
    let f = Fixture::new("search");
    f.doc(r"C:\d\ship.txt", "the ship left the harbour");
    f.doc(r"C:\d\parts.txt", "shipment of spare parts");
    f.doc(r"C:\d\zork.txt", "notes on Zork3 puzzles");
    let ship = run(&f, q("ship", false));
    assert_eq!(ship.len(), 1);
    assert_eq!(ship[0].path, r"C:\d\ship.txt");
    assert_eq!(run(&f, SearchQuery { prefix_last: true, ..q("ship", false) }).len(), 2);
    let zork = run(&f, q("zork", false));
    assert_eq!(zork.len(), 1);
    assert_eq!(zork[0].path, r"C:\d\zork.txt");
}

#[test]
fn snake_case_names_are_found_by_their_words() {
    let f = Fixture::new("search");
    f.doc(r"C:\d\units.py", "def parse_duration(text): pass");
    assert_eq!(run(&f, q("duration", false)).len(), 1);
    assert_eq!(run(&f, q("parse_duration", false)).len(), 1);
}

#[test]
fn a_word_in_a_chunk_outside_the_best_ranked_ones_still_counts() {
    let f = Fixture::new("search");
    for i in 0..30 {
        f.doc(&format!(r"C:\d\b{i}.txt"), "budget budget budget report");
    }
    let (id, _) = f.store.begin_file(r"C:\d\both.txt", FileCategory::Document, false, 1, 1).unwrap();
    f.store.add_chunks(id, false, &[(&ExtractedItem::name(), None)], Some("both txt")).unwrap();
    let a = ExtractedItem::text(chunk_kind::TEXT, "a short budget line".into(), None, None);
    let b = ExtractedItem::text(chunk_kind::TEXT, "the zeppelin flies".into(), None, None);
    f.store.add_chunks(id, false, &[(&a, None), (&b, None)], None).unwrap();
    let r = run(&f, q("budget zeppelin", false));
    assert!(r[0].path.ends_with("both.txt"));
}

#[test]
fn a_name_looked_up_in_code_opens_where_it_is_defined() {
    let f = Fixture::new("search");
    let add_code = |path: &str, text: &str, first: usize| {
        let (id, _) = f.store.begin_file(path, FileCategory::Code, true, 1, 1).unwrap();
        let name = crate::index::indexer::name_tokens(crate::file_kinds::pathx::file_name(path));
        f.store.add_chunks(id, true, &[(&ExtractedItem::name(), None)], Some(&name)).unwrap();
        let item = ExtractedItem::text(chunk_kind::CODE, text.into(), Some(format!("L{first}")), Some(first as f64));
        let v = FakeEmbedder::vec(text);
        f.vectors.add(f.store.add_chunks(id, true, &[(&item, Some(&v))], None).unwrap());
        f.store.finish_file(id, status::DONE, None, None, None);
    };
    add_code(r"C:\r\timer.py", "import units\n\nseconds = parse_duration(spec)", 1);
    add_code(r"C:\r\units.py", "import re\n\n\ndef parse_duration(text):\n    return 0", 40);
    let results = run(&f, q("code: parse_duration", false));
    let units = results.iter().find(|r| r.path.ends_with("units.py")).unwrap();
    assert_eq!(units.position, Some(43.0));
    assert_eq!(results.iter().find(|r| r.path.ends_with("timer.py")).unwrap().position, Some(3.0));
}

#[test]
fn russian_word_forms_and_partial_queries_match_by_keyword() {
    let f = Fixture::new("search");
    f.doc(r"C:\d\pets.txt", "Две кошки спали на диване весь день");
    f.doc(r"C:\d\other.txt", "Отчёт о продажах за квартал");
    let r = run(&f, q("кошка", false));
    assert_eq!(r.len(), 1);
    assert!(r[0].path.ends_with("pets.txt"));
    assert_eq!(run(&f, q("кошка жираф", false)).len(), 1);
}

#[test]
fn ocr_text_is_kept_for_keywords_but_not_embedded() {
    let f = Fixture::new("search");
    let (id, _) = f.store.begin_file(r"C:\d\shot.png", FileCategory::Image, false, 1, 1).unwrap();
    let image = ExtractedItem::media(chunk_kind::IMAGE, EmbedInput::Image(vec![]), None, None);
    let ocr = ExtractedItem::text(chunk_kind::OCR, "error CS0246".into(), None, None).keyword_only(true);
    let v = FakeEmbedder::vec("picture");
    let stored = f.store.add_chunks(id, false, &[(&image, Some(&v)), (&ocr, None)], None).unwrap();
    assert_eq!(stored.len(), 1);
    assert!(!f.store.keyword("\"CS0246\"", false, 400).is_empty());
}

#[test]
fn snippets_center_on_the_first_word_found() {
    let long = format!("{} needle {}", "lorem ipsum ".repeat(40), "dolor sit ".repeat(40));
    let s = snippet(Some(&long), &["needle".into()], 220).unwrap();
    assert!(s.starts_with('…') && s.ends_with('…') && s.contains("needle"));
    assert_eq!(snippet(Some("short text"), &[], 220).as_deref(), Some("short text"));
    assert_eq!(snippet(Some("   "), &[], 220), None);
}
