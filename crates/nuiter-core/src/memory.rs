//! Giving freed memory back to Windows. Rust frees as it goes (no collector keeps garbage around), but the process
//! heaps keep committed pages for reuse, and SQLite keeps page caches: after indexing, a search window that was hidden,
//! or vectors that were released, both are handed back.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{GetProcessHeaps, HEAP_FLAGS, HeapCompact};

pub fn relieve() {
    rusqlite_release();
    let mut heaps = [HANDLE::default(); 256];
    // SAFETY: the array is valid for its length; HeapCompact takes any heap handle of this process.
    unsafe {
        let n = (GetProcessHeaps(&mut heaps) as usize).min(heaps.len());
        for h in &heaps[..n] {
            HeapCompact(*h, HEAP_FLAGS(0));
        }
    }
}

fn rusqlite_release() {
    // SAFETY: sqlite3_release_memory is thread-safe and takes a byte count.
    unsafe {
        rusqlite::ffi::sqlite3_release_memory(i32::MAX);
    }
}

/// SQLite's caches stop growing past this (it frees pages it can when it gets there).
pub fn limit_sqlite_heap(bytes: i64) {
    // SAFETY: sets a process-wide soft limit.
    unsafe {
        rusqlite::ffi::sqlite3_soft_heap_limit64(bytes);
    }
}
