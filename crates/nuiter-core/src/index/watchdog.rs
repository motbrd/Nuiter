//! Watches reading done in this process (when there are no reader helpers). Decoders can loop on a broken file or keep
//! allocating, and a thread can't be stopped from outside: the way out is to name the file, mark it as one not to read
//! again, and restart. Every 5 seconds this checks how long each file being read has gone without progress and how much
//! memory the process has taken beyond what it holds by design; past a limit it calls `ran_away` with the file.
//!
//! The files being read are also listed in a marker file. One still listed at the next start was being read when the
//! process died (`left_over`); twice in a row means the file brings it down.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::paths::Log;

/// Without progress for this long a file is stuck: every page, window or frame read is progress.
pub const STALL_LIMIT: Duration = Duration::from_secs(10 * 60);

/// What reading one file may take beyond the app's own memory: 3 GB, plus a quarter of the PC's memory over 8 GB, at
/// most 12 GB.
pub fn read_memory_cap() -> u64 {
    let total = crate::win::total_memory();
    (12u64 << 30).min((3u64 << 30) + total.saturating_sub(8 << 30) / 4)
}

type RanAway = Box<dyn Fn(Option<String>, String) + Send + Sync>;

struct Inner {
    reading: Mutex<HashMap<String, (String, Instant)>>,
    marker: PathBuf,
    baseline: Box<dyn Fn() -> u64 + Send + Sync>,
    fired: AtomicBool,
    stopped: AtomicBool,
    ran_away: Mutex<Option<RanAway>>,
}

#[derive(Clone)]
pub struct ReadingWatchdog(Arc<Inner>);

impl ReadingWatchdog {
    pub fn new(marker: PathBuf, baseline: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self(Arc::new(Inner {
            reading: Mutex::new(HashMap::new()),
            marker,
            baseline: Box::new(baseline),
            fired: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            ran_away: Mutex::new(None),
        }))
    }

    /// Called once with the file (None when none was being read) and why.
    pub fn on_ran_away(&self, f: impl Fn(Option<String>, String) + Send + Sync + 'static) {
        *self.0.ran_away.lock().unwrap() = Some(Box::new(f));
    }

    /// The files the last run was reading when it ended without closing. Read once, before `start`.
    pub fn left_over(&self) -> Vec<String> {
        std::fs::read_to_string(&self.0.marker)
            .map(|t| t.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    }

    pub fn start(&self) {
        self.write_marker();
        let weak = Arc::downgrade(&self.0);
        let _ = std::thread::Builder::new().name("ReadingWatchdog".into()).stack_size(256 << 10).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let Some(inner) = weak.upgrade() else { return };
                if inner.stopped.load(Ordering::SeqCst) {
                    return;
                }
                ReadingWatchdog(inner).check();
            }
        });
    }

    pub fn begin(&self, path: &str) {
        self.0.reading.lock().unwrap().insert(crate::uni::lower_str(path), (path.to_string(), Instant::now()));
        self.write_marker();
    }

    pub fn progress(&self, path: &str) {
        if let Some(e) = self.0.reading.lock().unwrap().get_mut(&crate::uni::lower_str(path)) {
            e.1 = Instant::now();
        }
    }

    pub fn end(&self, path: &str) {
        let removed = self.0.reading.lock().unwrap().remove(&crate::uni::lower_str(path)).is_some();
        if removed {
            self.write_marker();
        }
    }

    fn write_marker(&self) {
        let reading = self.0.reading.lock().unwrap();
        let r = if reading.is_empty() {
            match std::fs::remove_file(&self.0.marker) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            std::fs::write(&self.0.marker, reading.values().map(|v| v.0.as_str()).collect::<Vec<_>>().join("\r\n"))
        };
        if let Err(e) = r {
            Log::warn(format!("Watchdog marker: {e}"));
        }
    }

    fn check(&self) {
        if self.0.fired.load(Ordering::SeqCst) {
            return;
        }
        let stuck = {
            let reading = self.0.reading.lock().unwrap();
            reading.values().find(|v| v.1.elapsed() > STALL_LIMIT).map(|v| v.0.clone())
        };
        if let Some(path) = stuck {
            self.fire(Some(path), format!("made no progress for {} minutes", STALL_LIMIT.as_secs() / 60));
            return;
        }
        let used = crate::win::private_bytes().saturating_sub((self.0.baseline)());
        let cap = read_memory_cap();
        if used > cap {
            // The file read longest is the likeliest one.
            let oldest = self.0.reading.lock().unwrap().values().min_by_key(|v| v.1).map(|v| v.0.clone());
            self.fire(oldest, format!("needed more than {} GB of memory to read", cap >> 30));
        }
    }

    fn fire(&self, path: Option<String>, why: String) {
        if self.0.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        Log::warn(format!("Watchdog: {} {why}", path.as_deref().unwrap_or("no file being read")));
        if let Some(f) = self.0.ran_away.lock().unwrap().as_ref() {
            f(path, why);
        }
    }

    /// A clean stop: nothing was left half-read.
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
        self.0.reading.lock().unwrap().clear();
        self.write_marker();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_files_being_read_until_a_clean_stop() {
        let dir = std::env::temp_dir().join(format!("nuiter-watchdog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("index.db.reading");
        let first = ReadingWatchdog::new(marker.clone(), || 0);
        first.start();
        first.begin(r"C:\d\a.pdf");
        first.begin(r"C:\d\b.mp4");
        first.end(r"C:\d\a.pdf");
        assert_eq!(ReadingWatchdog::new(marker.clone(), || 0).left_over(), vec![r"C:\d\b.mp4".to_string()]);
        first.stop();
        assert!(ReadingWatchdog::new(marker, || 0).left_over().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
