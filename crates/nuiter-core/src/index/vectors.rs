//! All vectors in one contiguous float16 matrix (as they're stored on disk: 1.5 KB a vector) for brute-force cosine
//! search, turned into float32 a block of rows at a time. Removed rows are tombstoned and their slots reused. The
//! matrix is dropped while nobody searches (`release`) and read back from the index by the next search.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use half::f16;

use crate::extract::chunk_kind;
use crate::vector_math;

/// What made a vector. Scores are calibrated per modality: text, image and audio vectors sit at different cosine
/// levels for the same query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Modality {
    Text = 0,
    Image = 1,
    Audio = 2,
}

impl Modality {
    pub fn of(kind: &str) -> Modality {
        match kind {
            chunk_kind::IMAGE | chunk_kind::FRAME => Modality::Image,
            chunk_kind::AUDIO => Modality::Audio,
            _ => Modality::Text,
        }
    }
}

/// A chunk's vector, with what the search needs to know about it.
#[derive(Debug, Clone)]
pub struct StoredVector {
    pub chunk_id: i64,
    pub file_id: i64,
    pub modality: Modality,
    pub is_code: bool,
    pub vector: Vec<f16>,
}

impl StoredVector {
    pub fn new(chunk_id: i64, file_id: i64, kind: &str, is_code: bool, v: &[f32]) -> Self {
        Self { chunk_id, file_id, modality: Modality::of(kind), is_code, vector: vector_math::to_half(v) }
    }
}

/// A file's best chunk for a query: its calibrated score (`z`, after the allowance for the file's length) and raw cosine.
#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub chunk_id: i64,
    pub file_id: i64,
    pub modality: Modality,
    pub score: f32,
    pub z: f64,
}

/// How many files' worth of the overall average a modality's own average is blended with.
pub const PRIOR_FILES: f64 = 10.0;
/// The smallest spread a query's scores are divided by: a near-empty index can't blow scores up.
const MIN_SPREAD: f64 = 0.025;
/// How much of a long file's head start is taken back: its best chunk is the best of many draws.
const LENGTH_ALLOWANCE: f64 = 0.25;
const ROWS_PER_BLOCK: usize = 256;
const INITIAL_CAPACITY: usize = 1024;

struct Matrix {
    dim: usize,
    data: Vec<f16>,
    chunk: Vec<i64>,
    file: Vec<i64>,
    modality: Vec<Modality>,
    code: Vec<bool>,
    count: usize,
    /// Live vectors per file.
    per_file: HashMap<i64, u32>,
    free: Vec<usize>,
    slot_of: HashMap<i64, usize>,
    /// Each slot's weight (1 / its file's vectors), rebuilt lazily after changes.
    weights: Mutex<Option<std::sync::Arc<Vec<f32>>>>,
}

impl Matrix {
    fn new(dim: usize, capacity: usize) -> Matrix {
        Matrix {
            dim,
            data: Vec::with_capacity(capacity * dim),
            chunk: Vec::with_capacity(capacity),
            file: Vec::with_capacity(capacity),
            modality: Vec::with_capacity(capacity),
            code: Vec::with_capacity(capacity),
            count: 0,
            per_file: HashMap::new(),
            free: vec![],
            slot_of: HashMap::new(),
            weights: Mutex::new(None),
        }
    }

    fn add(&mut self, vectors: impl IntoIterator<Item = StoredVector>) {
        for v in vectors {
            if v.vector.len() != self.dim || self.slot_of.contains_key(&v.chunk_id) {
                continue;
            }
            let slot = match self.free.pop() {
                Some(s) => {
                    self.data[s * self.dim..(s + 1) * self.dim].copy_from_slice(&v.vector);
                    self.chunk[s] = v.chunk_id;
                    self.file[s] = v.file_id;
                    self.modality[s] = v.modality;
                    self.code[s] = v.is_code;
                    s
                }
                None => {
                    self.data.extend_from_slice(&v.vector);
                    self.chunk.push(v.chunk_id);
                    self.file.push(v.file_id);
                    self.modality.push(v.modality);
                    self.code.push(v.is_code);
                    self.count += 1;
                    self.count - 1
                }
            };
            self.slot_of.insert(v.chunk_id, slot);
            *self.per_file.entry(v.file_id).or_insert(0) += 1;
        }
        *self.weights.lock().unwrap() = None;
    }

    fn remove(&mut self, ids: &[i64]) {
        for id in ids {
            let Some(slot) = self.slot_of.remove(id) else { continue };
            self.chunk[slot] = -1;
            self.free.push(slot);
            let file = self.file[slot];
            match self.per_file.get_mut(&file) {
                Some(n) if *n > 1 => *n -= 1,
                _ => {
                    self.per_file.remove(&file);
                }
            }
        }
        *self.weights.lock().unwrap() = None;
    }

    fn shrink_to_fit(&mut self) {
        self.data.shrink_to_fit();
        self.chunk.shrink_to_fit();
        self.file.shrink_to_fit();
        self.modality.shrink_to_fit();
        self.code.shrink_to_fit();
    }

    fn weights(&self) -> std::sync::Arc<Vec<f32>> {
        let mut w = self.weights.lock().unwrap();
        if let Some(w) = w.as_ref()
            && w.len() >= self.count
        {
            return w.clone();
        }
        let built: Vec<f32> = (0..self.count)
            .map(|i| if self.chunk[i] >= 0 { 1.0 / *self.per_file.get(&self.file[i]).unwrap_or(&1) as f32 } else { 0.0 })
            .collect();
        let arc = std::sync::Arc::new(built);
        *w = Some(arc.clone());
        arc
    }
}

enum Change {
    Add(Vec<StoredVector>),
    Remove(Vec<i64>),
}

struct State {
    loaded: bool,
    pending: Option<Vec<Change>>,
}

pub struct VectorIndex {
    dim: usize,
    matrix: RwLock<Matrix>,
    state: Mutex<State>,
    load_gate: Mutex<()>,
    last_used: Mutex<Instant>,
}

impl Default for VectorIndex {
    fn default() -> Self {
        Self::new(768)
    }
}

impl VectorIndex {
    pub fn new(dim: usize) -> VectorIndex {
        Self::with_capacity(dim, INITIAL_CAPACITY)
    }

    pub fn with_capacity(dim: usize, capacity: usize) -> VectorIndex {
        VectorIndex {
            dim,
            matrix: RwLock::new(Matrix::new(dim, capacity)),
            state: Mutex::new(State { loaded: true, pending: None }),
            load_gate: Mutex::new(()),
            last_used: Mutex::new(Instant::now()),
        }
    }

    pub fn count(&self) -> usize {
        self.matrix.read().unwrap().slot_of.len()
    }

    /// False after `release`, until `ensure_loaded`.
    pub fn loaded(&self) -> bool {
        self.state.lock().unwrap().loaded
    }

    /// How long since a search (or the load) last used the vectors.
    pub fn idle(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }

    /// Memory the matrix holds (bytes).
    pub fn bytes(&self) -> usize {
        let m = self.matrix.read().unwrap();
        m.data.capacity() * 2 + m.chunk.capacity() * 18
    }

    /// Frees the vectors' memory until the next search, once nobody has searched for a while: the app stays small
    /// while idle. The database keeps them; adds and removes meanwhile are dropped, since the next load reads them.
    pub fn release(&self) {
        let _gate = self.load_gate.lock().unwrap();
        self.release_held();
    }

    /// `release`, with the load gate held by the caller.
    fn release_held(&self) {
        {
            let mut s = self.state.lock().unwrap();
            if !s.loaded {
                return;
            }
            s.loaded = false;
        }
        *self.matrix.write().unwrap() = Matrix::new(self.dim, 0);
    }

    /// `release`, unless the vectors were used within `idle` (checked under the load gate, so a search that has just
    /// made sure of them with `ensure_loaded` never finds them gone). Whether they were released.
    pub fn release_if_idle(&self, idle: Duration) -> bool {
        let _gate = self.load_gate.lock().unwrap();
        if !self.state.lock().unwrap().loaded || self.idle() < idle {
            return false;
        }
        self.release_held();
        true
    }

    /// Makes sure the vectors are in memory, reading them again after `release` (`load` hands over every stored vector;
    /// `expected()` of them, about: the matrix is sized for them at once instead of doubling its way there, which held
    /// twice the memory at its peak). Counts as a use.
    pub fn ensure_loaded(&self, expected: impl FnOnce() -> usize, load: impl FnOnce(&mut dyn FnMut(StoredVector))) {
        *self.last_used.lock().unwrap() = Instant::now();
        let _gate = self.load_gate.lock().unwrap();
        {
            let mut s = self.state.lock().unwrap();
            if s.loaded {
                return;
            }
            s.pending = Some(vec![]);
        }
        *self.matrix.write().unwrap() = Matrix::new(self.dim, expected());
        let mut batch: Vec<StoredVector> = Vec::with_capacity(1024);
        load(&mut |v| {
            batch.push(v);
            if batch.len() >= 1024 {
                self.matrix.write().unwrap().add(batch.drain(..));
            }
        });
        self.matrix.write().unwrap().add(batch.drain(..));
        let mut s = self.state.lock().unwrap();
        let pending = s.pending.take().unwrap_or_default();
        {
            let mut m = self.matrix.write().unwrap();
            for change in pending {
                match change {
                    Change::Add(v) => m.add(v),
                    Change::Remove(ids) => m.remove(&ids),
                }
            }
            m.shrink_to_fit();
        }
        s.loaded = true;
    }

    /// Runs a change now when the vectors are loaded, after the load when one is running, and not at all when they're
    /// released (the database has it). Returns the change back when it should run now.
    fn defer(&self, change: Change) -> Option<Change> {
        let mut s = self.state.lock().unwrap();
        if s.loaded {
            return Some(change);
        }
        if let Some(p) = s.pending.as_mut() {
            p.push(change);
        }
        None
    }

    pub fn add(&self, vectors: Vec<StoredVector>) {
        if vectors.is_empty() {
            return;
        }
        if let Some(Change::Add(v)) = self.defer(Change::Add(vectors)) {
            self.matrix.write().unwrap().add(v);
        }
    }

    pub fn remove(&self, chunk_ids: Vec<i64>) {
        if chunk_ids.is_empty() {
            return;
        }
        if let Some(Change::Remove(ids)) = self.defer(Change::Remove(chunk_ids)) {
            self.matrix.write().unwrap().remove(&ids);
        }
    }

    /// Scores every live vector of the requested domain (code or not) and returns each file's best chunk on this
    /// query's scale (`calibrate`), best first, files at or below the query's average left out. That best is then
    /// lowered a little for long files (`length_allowance_for`). Also returns how many files the domain's vectors
    /// belong to: what the scale was measured on.
    pub fn search(&self, query: &[f32], code: bool) -> (Vec<Hit>, usize) {
        *self.last_used.lock().unwrap() = Instant::now();
        let m = self.matrix.read().unwrap();
        let count = m.count;
        let dim = m.dim;
        let mut scores = vec![0f32; count];
        let mut live = vec![false; count];
        let blocks = count.div_ceil(ROWS_PER_BLOCK);

        // Blocks of rows on all cores, each row's float16 read straight into the dot product (`dot_half`; it gives what
        // turning the row into float32 first gave, without the copy). Small indexes stay on this thread.
        let fast = vector_math::has_f16c();
        let score_range = |first_block: usize, scores: &mut [f32], live: &mut [bool]| {
            let start = first_block * ROWS_PER_BLOCK;
            for (r, (score, live)) in scores.iter_mut().zip(live.iter_mut()).enumerate() {
                let i = start + r;
                if m.chunk[i] < 0 || m.code[i] != code {
                    continue;
                }
                *live = true;
                *score = vector_math::dot_half(query, &m.data[i * dim..(i + 1) * dim], fast);
            }
        };
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(32);
        if blocks < 64 || threads < 2 {
            score_range(0, &mut scores, &mut live);
        } else {
            let per = blocks.div_ceil(threads);
            let sr = &score_range;
            std::thread::scope(|s| {
                let mut sc: &mut [f32] = &mut scores;
                let mut lv: &mut [bool] = &mut live;
                let mut b = 0;
                while b < blocks {
                    let n_blocks = per.min(blocks - b);
                    let rows = (n_blocks * ROWS_PER_BLOCK).min(count - b * ROWS_PER_BLOCK);
                    let (a, rest) = std::mem::take(&mut sc).split_at_mut(rows);
                    let (l, lrest) = std::mem::take(&mut lv).split_at_mut(rows);
                    sc = rest;
                    lv = lrest;
                    let first = b;
                    s.spawn(move || sr(first, a, l));
                    b += n_blocks;
                }
            });
        }

        let weights = m.weights();
        let z = calibrate(&scores, &m.modality[..count], &weights[..count], &live);
        let mass: f64 = (0..count).filter(|&i| live[i]).map(|i| weights[i] as f64).sum();
        let files = mass.round() as usize;

        let mut best: HashMap<i64, usize> = HashMap::new();
        for i in 0..count {
            if !live[i] || z[i] <= 0.0 {
                continue;
            }
            match best.get(&m.file[i]) {
                Some(&b) if z[b] >= z[i] => {}
                _ => {
                    best.insert(m.file[i], i);
                }
            }
        }
        let mut hits: Vec<Hit> = best
            .values()
            .map(|&i| Hit {
                chunk_id: m.chunk[i],
                file_id: m.file[i],
                modality: m.modality[i],
                score: scores[i],
                z: z[i] - length_allowance_for(*m.per_file.get(&m.file[i]).unwrap_or(&1) as usize),
            })
            .filter(|h| h.z > 0.0)
            .collect();
        hits.sort_by(|a, b| b.z.total_cmp(&a.z));
        (hits, files)
    }
}

/// What a file of `vectors` vectors gives back of its best score. The best of n draws from noise sits about
/// √(2 ln n) spreads up, so a long file looks better than it is on any query; a quarter of that is taken back (a
/// 40-page PDF: 0.68, a 1,000-chunk book: 0.93, one vector: nothing).
pub fn length_allowance_for(vectors: usize) -> f64 {
    if vectors <= 1 { 0.0 } else { LENGTH_ALLOWANCE * (2.0 * (vectors as f64).ln()).sqrt() }
}

/// A query's calibrated scores: how far each cosine is above its modality's typical score, in spreads of the whole
/// index. Each modality is centred on its own average, blended with the overall one by how many files it has
/// (`PRIOR_FILES`). Averages and the spread are weighted so that every file counts once. The spread is at least
/// `MIN_SPREAD`.
pub fn calibrate(scores: &[f32], modality: &[Modality], weight: &[f32], live: &[bool]) -> Vec<f64> {
    let mut sum = [0f64; 3];
    let mut mass = [0f64; 3];
    for i in 0..scores.len() {
        if !live[i] {
            continue;
        }
        let m = modality[i] as usize;
        sum[m] += weight[i] as f64 * scores[i] as f64;
        mass[m] += weight[i] as f64;
    }
    let total = mass[0] + mass[1] + mass[2];
    let overall = if total > 0.0 { (sum[0] + sum[1] + sum[2]) / total } else { 0.0 };
    let centre: [f64; 3] = std::array::from_fn(|m| (sum[m] + PRIOR_FILES * overall) / (mass[m] + PRIOR_FILES));
    let mut squares = 0f64;
    for i in 0..scores.len() {
        if !live[i] {
            continue;
        }
        let d = scores[i] as f64 - centre[modality[i] as usize];
        squares += weight[i] as f64 * d * d;
    }
    let spread = MIN_SPREAD.max(if total > 0.0 { (squares / total).sqrt() } else { 0.0 });
    (0..scores.len())
        .map(|i| if live[i] { (scores[i] as f64 - centre[modality[i] as usize]) / spread } else { f64::NEG_INFINITY })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(seed: u64, dim: usize) -> Vec<f32> {
        let mut s = seed;
        let mut v: Vec<f32> = (0..dim)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((s >> 33) as f64 / (1u64 << 31) as f64 - 0.5) as f32
            })
            .collect();
        vector_math::normalize(&mut v);
        v
    }

    #[test]
    fn a_load_sizes_the_matrix_once() {
        let idx = VectorIndex::default();
        idx.release();
        idx.ensure_loaded(|| 1000, |sink| {
            for i in 0..1000 {
                sink(StoredVector::new(i, i / 10, chunk_kind::TEXT, false, &unit(i as u64, 768)));
            }
        });
        assert_eq!(idx.count(), 1000);
        // Exactly what 1,000 vectors take: no doubled capacity left over from growing.
        assert_eq!(idx.bytes(), 1000 * (768 * 2 + 18));
    }

    #[test]
    fn removed_slots_are_reused_and_search_finds_the_new_vector() {
        let idx = VectorIndex::new(768);
        idx.add((0..300).map(|i| StoredVector::new(i, i, chunk_kind::TEXT, false, &unit(i as u64, 768))).collect());
        idx.remove(vec![5, 6]);
        let q = unit(4242, 768);
        idx.add(vec![StoredVector::new(1000, 1000, chunk_kind::TEXT, false, &q)]);
        assert_eq!(idx.count(), 299);
        let before = idx.bytes();
        let (hits, files) = idx.search(&q, false);
        assert_eq!(files, 299);
        assert_eq!(hits[0].chunk_id, 1000);
        assert!(hits.iter().all(|h| h.chunk_id != 5 && h.chunk_id != 6));
        assert_eq!(idx.bytes(), before);
    }
}
