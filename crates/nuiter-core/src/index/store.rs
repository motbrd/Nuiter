//! The index in one SQLite file: files, chunks (float16 vectors) and an FTS5 keyword index. The same schema as the .NET
//! version, so an index built by one opens in the other.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use half::f16;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::extract::{ExtractedItem, chunk_kind};
use crate::file_kinds::{FileCategory, pathx};
use crate::index::vectors::{Modality, StoredVector};
use crate::paths::{Log, Paths};
use crate::vector_math;

#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub status: String,
    pub next_page: Option<i64>,
    pub category: FileCategory,
    pub is_code: bool,
}

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub chunk_id: i64,
    pub file_id: i64,
    pub path: String,
    pub name: String,
    pub category: FileCategory,
    pub kind: &'static str,
    pub locator: Option<String>,
    pub position: Option<f64>,
    pub text: Option<String>,
    pub mtime: i64,
    pub size: i64,
}

pub mod status {
    pub const DONE: &str = "done";
    /// A long file with pages or chunks left for later steps.
    pub const PARTIAL: &str = "partial";
    pub const ERROR: &str = "error";
    pub const INDEXING: &str = "indexing";
}

/// Keyword search's tokenizer: English words by their Porter stem ("running" finds "runs"; other scripts pass through
/// unchanged), and "_" splits words, so "parse" finds parse_duration. Marks (M*) count as letters, so Hindi and Bengali
/// vowel signs and Arabic harakat stay inside words.
const FTS_TOKENIZER: &str = "porter unicode61 remove_diacritics 2 categories 'L* N* Co M*'";
const FTS_VERSION: &str = "porter-v3";

/// Connections kept for reuse; more are opened when threads need them and closed when they're handed back.
const POOL_SIZE: usize = 3;

pub struct IndexStore {
    /// The database file.
    pub file_path: PathBuf,
    pool: Mutex<Vec<Connection>>,
}

/// A pooled connection: back to the pool when dropped.
struct Pooled<'a> {
    store: &'a IndexStore,
    conn: Option<Connection>,
}

impl std::ops::Deref for Pooled<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for Pooled<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().unwrap()
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take()
            && let Ok(mut pool) = self.store.pool.lock()
            && pool.len() < POOL_SIZE
        {
            pool.push(c);
        }
    }
}

fn file_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: r.get(0)?,
        path: r.get(1)?,
        size: r.get(2)?,
        mtime: r.get(3)?,
        status: r.get(4)?,
        next_page: r.get(5)?,
        category: FileCategory::from_i32(r.get(6)?),
        is_code: r.get::<_, i64>(7)? != 0,
    })
}

const FILE_COLUMNS: &str = "id,path,size,mtime,status,next_page,category,is_code";

impl IndexStore {
    pub fn open_default() -> Result<IndexStore> {
        Self::open(&Paths::database())
    }

    pub fn open(path: &Path) -> Result<IndexStore> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file_path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let store = IndexStore { file_path, pool: Mutex::new(vec![]) };
        let mut c = store.conn()?;
        c.execute_batch(&format!(
            r#"
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
            CREATE TABLE IF NOT EXISTS files(
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE COLLATE NOCASE,
                name TEXT NOT NULL,
                category INTEGER NOT NULL,
                is_code INTEGER NOT NULL DEFAULT 0,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                status TEXT NOT NULL,
                next_page INTEGER,
                error TEXT,
                title TEXT,
                indexed_at INTEGER);
            CREATE TABLE IF NOT EXISTS chunks(
                id INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                loc TEXT,
                pos REAL,
                text TEXT,
                vec BLOB);
            CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);
            -- Counting vectors (the status line, every Settings paint) without reading 1.5 KB rows: 200 ms → 5 ms at
            -- 200k vectors. Indexes only: an index with them still opens in the .NET version.
            CREATE INDEX IF NOT EXISTS chunks_vec ON chunks(file_id) WHERE vec IS NOT NULL;
            CREATE INDEX IF NOT EXISTS files_status ON files(status);
            -- Moved files are found by size and date (`find_by_identity`).
            CREATE INDEX IF NOT EXISTS files_identity ON files(size, mtime);
            CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(name, text, tokenize = "{FTS_TOKENIZER}");
            "#
        ))?;
        migrate_keyword_index(&mut c)?;
        drop(c);
        Ok(store)
    }

    fn conn(&self) -> Result<Pooled<'_>> {
        if let Some(c) = self.pool.lock().unwrap().pop() {
            return Ok(Pooled { store: self, conn: Some(c) });
        }
        let c = Connection::open(&self.file_path)?;
        // A small page cache per connection: the index is read in bursts, and the OS caches the file anyway.
        // `synchronous` is a setting of the connection, not of the file: without it here every pooled connection synced
        // the WAL to disk at each commit (FULL).
        c.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000; PRAGMA cache_size=-1024; PRAGMA temp_store=FILE; PRAGMA synchronous=NORMAL;",
        )?;
        Ok(Pooled { store: self, conn: Some(c) })
    }

    /// Closes the pooled connections (their caches go with them).
    pub fn release_memory(&self) {
        self.pool.lock().unwrap().clear();
    }

    // ---------- meta ----------

    pub fn get_meta(&self, key: &str) -> Option<String> {
        let c = self.conn().ok()?;
        c.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get::<_, Option<String>>(0)).optional().ok().flatten().flatten()
    }

    pub fn set_meta(&self, key: &str, value: Option<&str>) {
        let r = self.conn().and_then(|c| {
            c.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
            Ok(())
        });
        if let Err(e) = r {
            Log::warn(format!("Index meta {key}: {e}"));
        }
    }

    // ---------- files ----------

    pub fn get_file(&self, path: &str) -> Option<FileRecord> {
        let c = self.conn().ok()?;
        c.query_row(&format!("SELECT {FILE_COLUMNS} FROM files WHERE path=?1"), [path], file_from_row).optional().ok().flatten()
    }

    pub fn all_files(&self) -> Vec<FileRecord> {
        let Ok(c) = self.conn() else { return vec![] };
        let Ok(mut st) = c.prepare(&format!("SELECT {FILE_COLUMNS} FROM files")) else { return vec![] };
        st.query_map([], file_from_row).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Files that couldn't be read, with why (path order).
    pub fn unreadable_files(&self) -> Vec<(String, Option<String>)> {
        let Ok(c) = self.conn() else { return vec![] };
        let Ok(mut st) = c.prepare("SELECT path, error FROM files WHERE status='error' ORDER BY path") else { return vec![] };
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Indexed files below the folder `dir` (at any depth), by a range of the path index: '\' + 1 is ']'.
    pub fn files_under(&self, dir: &str) -> Vec<FileRecord> {
        let d = dir.trim_end_matches('\\');
        let (from, to) = (format!("{d}\\"), format!("{d}]"));
        let Ok(c) = self.conn() else { return vec![] };
        let Ok(mut st) = c.prepare_cached(&format!("SELECT {FILE_COLUMNS} FROM files WHERE path >= ?1 AND path < ?2")) else {
            return vec![];
        };
        st.query_map(params![from, to], file_from_row)
            .map(|rows| rows.flatten().filter(|f| pathx::is_strictly_under(&f.path, d)).collect())
            .unwrap_or_default()
    }

    /// Creates or resets a file row and removes its previous chunks. Returns the file id and the removed chunk ids.
    pub fn begin_file(&self, path: &str, category: FileCategory, is_code: bool, size: i64, mtime: i64) -> Result<(i64, Vec<i64>)> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: i64 = tx.query_row(
            r#"INSERT INTO files(path,name,category,is_code,size,mtime,status) VALUES(?1,?2,?3,?4,?5,?6,'indexing')
               ON CONFLICT(path) DO UPDATE SET path=excluded.path, name=excluded.name, category=excluded.category, is_code=excluded.is_code,
                   size=excluded.size, mtime=excluded.mtime, status='indexing', next_page=NULL, error=NULL
               RETURNING id"#,
            params![path, pathx::file_name(path), category as i32, is_code as i32, size, mtime],
            |r| r.get(0),
        )?;
        let removed = delete_chunks(&tx, id)?;
        tx.commit()?;
        Ok((id, removed))
    }

    /// Marks a file for its next step without deleting the chunks already stored.
    pub fn resume_file(&self, file_id: i64) -> Result<()> {
        self.conn()?.execute("UPDATE files SET status='indexing' WHERE id=?1", [file_id])?;
        Ok(())
    }

    /// Marks every file that has image vectors (pictures, video frames, rendered PDF pages) for re-indexing; their old
    /// vectors stay searchable until each file is processed again. Returns how many files were marked.
    pub fn mark_image_files_for_reindex(&self) -> usize {
        self.exec(&format!(
            "UPDATE files SET status='indexing' WHERE category IN ({},{}) OR id IN (SELECT DISTINCT file_id FROM chunks WHERE kind IN ('image','frame'))",
            FileCategory::Image as i32,
            FileCategory::Video as i32
        ))
    }

    /// Marks every file of `category` to be read and embedded again.
    pub fn mark_category_for_reindex(&self, category: FileCategory) -> usize {
        self.exec(&format!("UPDATE files SET status='indexing' WHERE category = {}", category as i32))
    }

    /// Marks the files (outside code search) with these extensions (".eml") to be read again.
    pub fn mark_extensions_for_reindex(&self, extensions: &[&str]) -> usize {
        let Ok(c) = self.conn() else { return 0 };
        let mut n = 0;
        for e in extensions {
            match c.execute("UPDATE files SET status='indexing' WHERE is_code=0 AND path LIKE ?1", [format!("%{e}")]) {
                Ok(k) => n += k,
                Err(err) => Log::warn(format!("Index update failed: {err}")),
            }
        }
        n
    }

    /// Marks recordings with no sound vector, and videos with no sound windows, to be read again.
    pub fn mark_soundless_for_reindex(&self) -> usize {
        self.exec(&format!(
            "UPDATE files SET status='indexing' WHERE status='done' AND ((category = {} AND NOT EXISTS (SELECT 1 FROM chunks WHERE file_id=files.id AND vec IS NOT NULL)) OR (category = {} AND NOT EXISTS (SELECT 1 FROM chunks WHERE file_id=files.id AND kind='audio')))",
            FileCategory::Audio as i32,
            FileCategory::Video as i32
        ))
    }

    fn exec(&self, sql: &str) -> usize {
        match self.conn().and_then(|c| Ok(c.execute(sql, [])?)) {
            Ok(n) => n,
            Err(e) => {
                Log::warn(format!("Index update failed: {e}"));
                0
            }
        }
    }

    pub fn finish_file(&self, file_id: i64, status: &str, next_page: Option<i64>, error: Option<&str>, title: Option<&str>) {
        let r = self.conn().and_then(|c| {
            c.execute(
                "UPDATE files SET status=?1, next_page=?2, error=?3, title=COALESCE(?4,title), indexed_at=?5 WHERE id=?6",
                params![status, next_page, error, title, crate::win::unix_now(), file_id],
            )?;
            Ok(())
        });
        if let Err(e) = r {
            Log::warn(format!("Index: finishing file {file_id}: {e}"));
        }
    }

    /// Removes the vectors of one kind of chunk (their text stays for keyword search). Returns how many.
    pub fn drop_vectors(&self, kind: &str) -> usize {
        self.conn().and_then(|c| Ok(c.execute("UPDATE chunks SET vec=NULL WHERE kind=?1 AND vec IS NOT NULL", [kind])?)).unwrap_or(0)
    }

    /// A file was moved or renamed: keep its vectors and keyword index, update its path and name.
    pub fn repath_file(&self, file_id: i64, new_path: &str, name_text: &str) -> Result<()> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE files SET path=?1, name=?2 WHERE id=?3", params![new_path, pathx::file_name(new_path), file_id])?;
        tx.execute(
            "UPDATE fts SET name=?1 WHERE rowid IN (SELECT id FROM chunks WHERE file_id=?2 AND kind='name')",
            params![name_text, file_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A folder was moved or renamed: re-point every file under it. Returns how many files moved.
    pub fn repath_folder(&self, old_dir: &str, new_dir: &str) -> usize {
        let prefix = format!("{}\\", old_dir.trim_end_matches('\\'));
        let to = format!("{}\\", new_dir.trim_end_matches('\\'));
        self.conn()
            .and_then(|c| {
                Ok(c.execute(
                    "UPDATE files SET path = ?2 || substr(path, length(?1) + 1) WHERE substr(path, 1, length(?1)) = ?1 COLLATE NOCASE",
                    params![prefix, to],
                )?)
            })
            .unwrap_or(0)
    }

    /// Indexed files with this name, size and modification time (candidates for "the same file, moved").
    pub fn find_by_identity(&self, name: &str, size: i64, mtime: i64) -> Vec<FileRecord> {
        let Ok(c) = self.conn() else { return vec![] };
        let Ok(mut st) = c.prepare(&format!("SELECT {FILE_COLUMNS} FROM files WHERE name=?1 COLLATE NOCASE AND size=?2 AND mtime=?3"))
        else {
            return vec![];
        };
        st.query_map(params![name, size, mtime], file_from_row).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Deletes a file and its chunks. Returns the removed chunk ids (for the in-memory vector index).
    pub fn delete_file(&self, path: &str) -> Vec<i64> {
        let r = (|| -> Result<Vec<i64>> {
            let mut c = self.conn()?;
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let id: Option<i64> = tx.query_row("SELECT id FROM files WHERE path=?1", [path], |r| r.get(0)).optional()?;
            let Some(id) = id else { return Ok(vec![]) };
            let removed = delete_chunks(&tx, id)?;
            tx.execute("DELETE FROM files WHERE id=?1", [id])?;
            tx.commit()?;
            Ok(removed)
        })();
        r.unwrap_or_else(|e| {
            Log::warn(format!("Index: deleting {path}: {e}"));
            vec![]
        })
    }

    /// Adds chunks (vector None for keyword-only rows). Returns the stored vectors with their new ids. The name row's
    /// keyword text is `name_text`.
    pub fn add_chunks(&self, file_id: i64, is_code: bool, items: &[(&ExtractedItem, Option<&[f32]>)], name_text: Option<&str>) -> Result<Vec<StoredVector>> {
        let mut stored = vec![];
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut ins = tx.prepare_cached("INSERT INTO chunks(file_id,kind,loc,pos,text,vec) VALUES(?1,?2,?3,?4,?5,?6) RETURNING id")?;
            let mut fts = tx.prepare_cached("INSERT INTO fts(rowid,name,text) VALUES(?1,?2,?3)")?;
            for (item, vec) in items {
                let half: Option<Vec<f16>> = vec.map(vector_math::to_half);
                let blob = half.as_ref().map(|h| vector_math::half_to_blob(h));
                let id: i64 = ins.query_row(params![file_id, item.kind, item.locator, item.position, item.text, blob], |r| r.get(0))?;
                if item.text.is_some() || item.kind == chunk_kind::NAME {
                    let is_name = item.kind == chunk_kind::NAME;
                    let name = if is_name { name_text.or(item.text.as_deref()).unwrap_or("") } else { "" };
                    let text = if is_name { "" } else { item.text.as_deref().unwrap_or("") };
                    fts.execute(params![id, name, text])?;
                }
                if let Some(h) = half {
                    stored.push(StoredVector { chunk_id: id, file_id, modality: Modality::of(item.kind), is_code, vector: h });
                }
            }
        }
        tx.commit()?;
        Ok(stored)
    }

    /// How many chunks have a vector (counted on the `chunks_vec` index).
    pub fn vector_count(&self) -> usize {
        self.conn()
            .and_then(|c| Ok(c.query_row("SELECT count(*) FROM chunks WHERE vec IS NOT NULL", [], |r| r.get::<_, i64>(0))?))
            .unwrap_or(0) as usize
    }

    /// Hands every stored vector to `sink`, read straight from float16 (no float32 copies).
    pub fn load_vectors(&self, sink: &mut dyn FnMut(StoredVector)) {
        let r = (|| -> Result<()> {
            let c = self.conn()?;
            let mut st = c.prepare("SELECT c.id, c.file_id, c.kind, f.is_code, c.vec FROM chunks c JOIN files f ON f.id=c.file_id WHERE c.vec IS NOT NULL")?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let kind: String = r.get(2)?;
                let blob = r.get_ref(4)?.as_blob()?;
                sink(StoredVector {
                    chunk_id: r.get(0)?,
                    file_id: r.get(1)?,
                    modality: Modality::of(&kind),
                    is_code: r.get::<_, i64>(3)? != 0,
                    vector: vector_math::blob_to_half(blob),
                });
            }
            Ok(())
        })();
        if let Err(e) = r {
            Log::error(format!("Loading vectors failed: {e}"));
        }
    }

    // ---------- search support ----------

    /// FTS5 keyword search for a MATCH expression. Returns chunk ids with a rank score in (0,1], best first.
    pub fn keyword(&self, expr: &str, code: bool, limit: usize) -> Vec<(i64, f64)> {
        let r = (|| -> Result<Vec<(i64, f64)>> {
            let c = self.conn()?;
            let mut st = c.prepare_cached(
                "SELECT fts.rowid, bm25(fts, 3.0, 1.0) AS rank FROM fts JOIN chunks ch ON ch.id = fts.rowid JOIN files f ON f.id = ch.file_id WHERE fts MATCH ?1 AND f.is_code = ?2 ORDER BY rank LIMIT ?3",
            )?;
            let ids: Vec<i64> = st.query_map(params![expr, code as i32, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(ids.into_iter().enumerate().map(|(i, id)| (id, 1.0 / (1.0 + 0.05 * i as f64))).collect())
        })();
        r.unwrap_or_else(|e| {
            Log::warn(format!("FTS query failed: {e}"));
            vec![]
        })
    }

    /// How many files (of code search or not) keyword search finds `expr` in, looking at no more than `rows` matching
    /// chunks: a word's rarity, for weighing it. Exact for all but the commonest words.
    pub fn file_frequency(&self, expr: &str, code: bool, rows: usize) -> usize {
        self.conn()
            .and_then(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(DISTINCT file_id) FROM (SELECT ch.file_id FROM fts JOIN chunks ch ON ch.id = fts.rowid JOIN files f ON f.id = ch.file_id WHERE fts MATCH ?1 AND f.is_code = ?2 LIMIT ?3)",
                    params![expr, code as i32, rows as i64],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap_or(0) as usize
    }

    /// Files in the index, of code search or not.
    pub fn file_count(&self, code: bool) -> usize {
        self.conn()
            .and_then(|c| Ok(c.query_row("SELECT COUNT(*) FROM files WHERE is_code = ?1", [code as i32], |r| r.get::<_, i64>(0))?))
            .unwrap_or(0) as usize
    }

    /// The chunks of `file_ids` that keyword search finds `expr` in (any chunk of the file, not only the best-ranked
    /// ones): which query words a file holds.
    pub fn chunks_matching(&self, expr: &str, file_ids: &[i64]) -> Vec<(i64, i64)> {
        let mut list = vec![];
        if file_ids.is_empty() {
            return list;
        }
        let Ok(c) = self.conn() else { return list };
        for batch in file_ids.chunks(500) {
            let ids = batch.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            let sql = format!("SELECT ch.id, ch.file_id FROM fts JOIN chunks ch ON ch.id = fts.rowid WHERE fts MATCH ?1 AND ch.file_id IN ({ids})");
            let r = c.prepare(&sql).and_then(|mut st| {
                let rows: Vec<(i64, i64)> = st.query_map([expr], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            });
            match r {
                Ok(rows) => list.extend(rows),
                Err(e) => Log::warn(format!("FTS query failed: {e}")),
            }
        }
        list
    }

    pub fn get_chunks(&self, ids: impl IntoIterator<Item = i64>) -> std::collections::HashMap<i64, ChunkInfo> {
        let mut result = std::collections::HashMap::new();
        let mut list: Vec<i64> = ids.into_iter().collect();
        list.sort_unstable();
        list.dedup();
        if list.is_empty() {
            return result;
        }
        let Ok(c) = self.conn() else { return result };
        for batch in list.chunks(500) {
            let ids = batch.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT ch.id, ch.file_id, f.path, f.name, f.category, ch.kind, ch.loc, ch.pos, ch.text, f.mtime, f.size, f.is_code FROM chunks ch JOIN files f ON f.id = ch.file_id WHERE ch.id IN ({ids})"
            );
            let r = c.prepare(&sql).and_then(|mut st| {
                let rows: Vec<ChunkInfo> = st
                    .query_map([], |r| {
                        let is_code = r.get::<_, i64>(11)? != 0;
                        let kind: String = r.get(5)?;
                        Ok(ChunkInfo {
                            chunk_id: r.get(0)?,
                            file_id: r.get(1)?,
                            path: r.get(2)?,
                            name: r.get(3)?,
                            category: if is_code { FileCategory::Code } else { FileCategory::from_i32(r.get(4)?) },
                            kind: chunk_kind::intern(&kind),
                            locator: r.get(6)?,
                            position: r.get(7)?,
                            text: r.get(8)?,
                            mtime: r.get(9)?,
                            size: r.get(10)?,
                        })
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(rows)
            });
            match r {
                Ok(rows) => result.extend(rows.into_iter().map(|c| (c.chunk_id, c))),
                Err(e) => Log::warn(format!("Reading chunks failed: {e}")),
            }
        }
        result
    }

    /// Best text excerpt of a file for display (used when the matching chunk is an image/audio item).
    pub fn first_text(&self, file_id: i64) -> Option<String> {
        let c = self.conn().ok()?;
        c.query_row(
            "SELECT text FROM chunks WHERE file_id=?1 AND text IS NOT NULL AND kind <> 'name' ORDER BY id LIMIT 1",
            [file_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// (files indexed, vectors, files that failed, files read in part).
    pub fn stats(&self) -> (i64, i64, i64, i64) {
        self.conn()
            .and_then(|c| {
                Ok(c.query_row(
                    "SELECT (SELECT count(*) FROM files WHERE status IN ('done','partial')), (SELECT count(*) FROM chunks WHERE vec IS NOT NULL), (SELECT count(*) FROM files WHERE status='error'), (SELECT count(*) FROM files WHERE status='partial')",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?)
            })
            .unwrap_or((0, 0, 0, 0))
    }

    pub fn vacuum(&self) {
        if let Ok(c) = self.conn() {
            let _ = c.execute_batch("INSERT INTO fts(fts) VALUES('optimize'); PRAGMA wal_checkpoint(TRUNCATE);");
        }
    }
}

fn delete_chunks(tx: &rusqlite::Transaction<'_>, file_id: i64) -> rusqlite::Result<Vec<i64>> {
    let ids: Vec<i64> = {
        let mut st = tx.prepare_cached("SELECT id FROM chunks WHERE file_id=?1")?;
        st.query_map([file_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    if !ids.is_empty() {
        tx.execute("DELETE FROM fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id=?1)", [file_id])?;
        tx.execute("DELETE FROM chunks WHERE file_id=?1", [file_id])?;
    }
    Ok(ids)
}

/// An index made with an older tokenizer gets its keyword table rebuilt from what it stores (nothing is read again).
fn migrate_keyword_index(c: &mut Connection) -> Result<()> {
    let v: Option<String> = c.query_row("SELECT value FROM meta WHERE key='fts'", [], |r| r.get(0)).optional()?;
    if v.as_deref() == Some(FTS_VERSION) {
        return Ok(());
    }
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(&format!(
        r#"
        DROP TABLE IF EXISTS fts_new;
        CREATE VIRTUAL TABLE fts_new USING fts5(name, text, tokenize = "{FTS_TOKENIZER}");
        INSERT INTO fts_new(rowid, name, text) SELECT rowid, name, text FROM fts;
        DROP TABLE fts;
        ALTER TABLE fts_new RENAME TO fts;
        INSERT INTO meta(key, value) VALUES('fts', '{FTS_VERSION}') ON CONFLICT(key) DO UPDATE SET value=excluded.value;
        "#
    ))?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::extract::chunk_kind;
    use crate::file_kinds::FileCategory;
    use crate::search::engine::tests::Fixture;

    #[test]
    fn counts_and_the_files_under_a_folder() {
        let f = Fixture::new("store");
        for p in [r"C:\Photos\a.txt", r"C:\photos\trip\b.txt", r"C:\Photos2\c.txt", r"C:\Photo\d.txt"] {
            f.add_doc(p, "some words", FileCategory::Document, chunk_kind::TEXT);
        }
        assert_eq!(f.store.vector_count(), 4);
        assert_eq!(f.store.stats(), (4, 4, 0, 0));
        // Case doesn't matter; a folder whose name only starts the same isn't inside.
        let mut under: Vec<String> = f.store.files_under(r"C:\PHOTOS\").into_iter().map(|r| r.path).collect();
        under.sort();
        assert_eq!(under, [r"C:\Photos\a.txt", r"C:\photos\trip\b.txt"]);
        assert!(f.store.files_under(r"C:\Photos\a.txt").is_empty());
    }

    #[test]
    fn every_connection_syncs_lightly_and_a_new_case_is_kept() {
        let f = Fixture::new("conns");
        let a = f.store.conn().unwrap();
        let b = f.store.conn().unwrap();
        for c in [&a, &b] {
            assert_eq!(c.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        }
        drop((a, b));
        // A file renamed only in case is stored as it's spelled now.
        f.store.begin_file(r"C:\d\report.txt", FileCategory::Document, false, 1, 1).unwrap();
        f.store.begin_file(r"C:\d\Report.txt", FileCategory::Document, false, 1, 1).unwrap();
        assert_eq!(f.store.get_file(r"c:\d\report.txt").unwrap().path, r"C:\d\Report.txt");
    }

    #[test]
    fn unreadable_files_come_with_why() {
        let f = Fixture::new("unreadable");
        f.add_doc(r"C:\b\fine.txt", "words", FileCategory::Document, chunk_kind::TEXT);
        let (id, _) = f.store.begin_file(r"C:\a\broken.jpg", FileCategory::Image, false, 12, 1).unwrap();
        f.store.finish_file(id, super::status::ERROR, None, Some("not a picture Windows can read"), None);
        assert_eq!(f.store.unreadable_files(), vec![(r"C:\a\broken.jpg".to_string(), Some("not a picture Windows can read".to_string()))]);
        assert_eq!(f.store.stats().2, 1);
    }
}
