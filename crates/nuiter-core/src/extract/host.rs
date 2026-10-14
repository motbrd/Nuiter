//! Runs the extractor in helper processes (`Nuiter.exe extract-service`), one file at a time each, as many as the
//! indexer reads in parallel. A helper that crashes, makes no progress for `stall_limit()` or uses more than
//! `watchdog::read_memory_cap()` is ended, and that one file fails; the next file gets a fresh helper. Helpers idle for
//! `IDLE_LIMIT` exit, giving their memory back. Without the helper executable reading stays in this process.

use std::io::{BufReader, BufWriter};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::Win32::Foundation::HANDLE;

use super::protocol::{self, CallBody, ResultBody};
use super::{ExtractContext, ExtractEnd, Sink};
use crate::cancel::Cancel;
use crate::file_kinds::FileCategory;
use crate::index::watchdog;
use crate::paths::{Log, Paths};
use crate::runtime::job;

/// A file the reader helper couldn't read: it crashed, hung or ran out of bounds on it.
#[derive(Debug, Clone)]
pub struct ExtractionError(pub String);

impl std::fmt::Display for ExtractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ExtractionError {}

pub fn stall_limit() -> Duration {
    std::env::var("NUITER_READER_STALL_SECONDS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(8 * 60))
}

pub const IDLE_LIMIT: Duration = Duration::from_secs(60);

/// A helper past these is retired after its file: Media Foundation and the WIC codecs keep some handles and memory
/// per file they opened.
const RETIRE_HANDLES: u32 = 3000;
const RETIRE_BYTES: u64 = 1 << 30;

struct Helper {
    child: Mutex<Child>,
    process: usize,
    pid: u32,
    input: Mutex<Option<BufWriter<ChildStdin>>>,
    output: Mutex<BufReader<ChildStdout>>,
    file: Mutex<Option<String>>,
    progress: Mutex<Instant>,
    deadline: Mutex<Option<(Instant, Duration)>>,
    killed_for: Mutex<Option<String>>,
    last_use: Mutex<Instant>,
    exited: AtomicBool,
    /// The app is busy with an item the helper sent (embedding it, or indexing is paused): the helper waits on the
    /// app then, and the time doesn't count as the helper making no progress.
    waiting_on_app: AtomicBool,
}

impl Helper {
    fn start(exe: &PathBuf) -> Result<Arc<Helper>> {
        let mut child = Command::new(exe)
            .arg("extract-service")
            .current_dir(Paths::exe_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000 /* CREATE_NO_WINDOW */ | 0x0000_4000 /* BELOW_NORMAL_PRIORITY_CLASS */)
            .spawn()
            .map_err(|e| ExtractionError(format!("the reader helper didn't start: {e}")))?;
        let process = child.as_raw_handle() as usize;
        job::assign(HANDLE(process as _));
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        Ok(Arc::new(Helper {
            pid: child.id(),
            child: Mutex::new(child),
            process,
            input: Mutex::new(Some(BufWriter::with_capacity(64 << 10, input))),
            output: Mutex::new(BufReader::with_capacity(64 << 10, output)),
            file: Mutex::new(None),
            progress: Mutex::new(Instant::now()),
            deadline: Mutex::new(None),
            killed_for: Mutex::new(None),
            last_use: Mutex::new(Instant::now()),
            exited: AtomicBool::new(false),
            waiting_on_app: AtomicBool::new(false),
        }))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.process as _)
    }

    fn has_exited(&self) -> bool {
        if self.exited.load(Ordering::SeqCst) {
            return true;
        }
        let done = matches!(self.child.lock().unwrap().try_wait(), Ok(Some(_)));
        if done {
            self.exited.store(true, Ordering::SeqCst);
        }
        done
    }

    fn busy(&self, file: &str) {
        *self.file.lock().unwrap() = Some(file.to_string());
        self.touch();
    }

    fn touch(&self) {
        *self.progress.lock().unwrap() = Instant::now();
    }

    fn idle(&self) {
        *self.file.lock().unwrap() = None;
        *self.deadline.lock().unwrap() = None;
        *self.last_use.lock().unwrap() = Instant::now();
    }

    fn read(&self) -> std::result::Result<(u8, Vec<u8>), ExtractionError> {
        let frame = protocol::read_frame(&mut *self.output.lock().unwrap());
        match frame {
            Ok(Some(f)) => Ok(f),
            _ => Err(self.failure()),
        }
    }

    /// Why the helper's output ended: what it was ended for, or its crash.
    fn failure(&self) -> ExtractionError {
        let mut code = None;
        for _ in 0..20 {
            if let Ok(Some(s)) = self.child.lock().unwrap().try_wait() {
                code = s.code();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let why = match (self.killed_for.lock().unwrap().clone(), code) {
            (Some(k), _) => k,
            (None, Some(c)) => format!("crashed the reader (exit code 0x{:X})", c as u32),
            (None, None) => "made the reader stop answering".into(),
        };
        ExtractionError(format!("It {why}"))
    }

    /// Why this helper should be replaced by a fresh one, or None.
    fn worn(&self) -> Option<String> {
        if self.has_exited() {
            return None;
        }
        let handles = crate::win::process_handle_count(self.handle());
        if handles > RETIRE_HANDLES {
            return Some(format!("{handles} handles"));
        }
        let bytes = crate::win::process_private_bytes(self.handle());
        (bytes > RETIRE_BYTES).then(|| format!("{} MB", bytes >> 20))
    }

    /// No more files: the helper reads the end of its input and exits.
    fn close(&self) {
        drop(self.input.lock().unwrap().take());
        for _ in 0..30 {
            if self.has_exited() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.kill("idle");
    }

    fn send<T: serde::Serialize>(&self, kind: u8, body: &T) -> std::io::Result<()> {
        match self.input.lock().unwrap().as_mut() {
            Some(w) => protocol::write_json(w, kind, body),
            None => Err(std::io::Error::other("closed")),
        }
    }

    fn kill(&self, why: &str) {
        let file = self.file.lock().unwrap().clone();
        if let Some(f) = &file
            && why != "closing"
            && why != "stopped halfway"
        {
            Log::warn(format!("Reader helper {} ended: {f} {why}", self.pid));
        }
        {
            let mut k = self.killed_for.lock().unwrap();
            if k.is_none() {
                *k = Some(why.to_string());
            }
        }
        let _ = self.child.lock().unwrap().kill();
        self.exited.store(true, Ordering::SeqCst);
    }

    /// Ends a helper that's stuck on its file, past its call's time, or too big.
    fn check(&self, memory_too: bool) {
        if self.file.lock().unwrap().is_none() || self.has_exited() {
            return;
        }
        if let Some((deadline, timeout)) = *self.deadline.lock().unwrap()
            && Instant::now() > deadline
        {
            self.kill(&format!("took over {} s", timeout.as_secs_f64().round()));
            return;
        }
        let stall = stall_limit();
        if !self.waiting_on_app.load(Ordering::SeqCst) && self.progress.lock().unwrap().elapsed() > stall {
            let s = stall.as_secs();
            self.kill(&if s >= 60 { format!("made no progress for {} minutes", s / 60) } else { format!("made no progress for {s} seconds") });
            return;
        }
        if memory_too {
            let cap = watchdog::read_memory_cap();
            if crate::win::process_private_bytes(self.handle()) > cap {
                self.kill(&format!("needed more than {} GB of memory to read", cap >> 30));
            }
        }
    }
}

pub struct ExtractionHost {
    exe: PathBuf,
    idle: Mutex<Vec<Arc<Helper>>>,
    all: Mutex<Vec<Weak<Helper>>>,
    disposed: AtomicBool,
}

impl ExtractionHost {
    /// Helpers run this executable again when it can be one (Nuiter.exe and nuiter-cli.exe both take
    /// "extract-service"), else nuiter-cli.exe beside it or a folder up (tests). None when neither is there: reading
    /// stays in this process.
    pub fn try_create() -> Option<Arc<ExtractionHost>> {
        if std::env::var_os("NUITER_IN_PROCESS_READING").is_some_and(|v| !v.is_empty()) {
            return None;
        }
        let me = std::env::current_exe().ok()?;
        let name = me.file_name()?.to_string_lossy().to_lowercase();
        let exe = if name == "nuiter.exe" || name == "nuiter-cli.exe" {
            me
        } else {
            let dir = me.parent()?;
            [dir.join("nuiter-cli.exe"), dir.parent()?.join("nuiter-cli.exe")].into_iter().find(|p| p.is_file())?
        };
        let host = Arc::new(ExtractionHost { exe, idle: Mutex::new(vec![]), all: Mutex::new(vec![]), disposed: AtomicBool::new(false) });
        let weak = Arc::downgrade(&host);
        let _ = std::thread::Builder::new().name("ReaderHelpers".into()).stack_size(256 << 10).spawn(move || {
            let mut tick = 0u64;
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let Some(h) = weak.upgrade() else { return };
                if h.disposed.load(Ordering::SeqCst) {
                    return;
                }
                tick += 1;
                h.tick(tick.is_multiple_of(8));
            }
        });
        Some(host)
    }

    fn take(&self) -> Result<Arc<Helper>> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(ExtractionError("closing".into()).into());
        }
        while let Some(h) = self.idle.lock().unwrap().pop() {
            if !h.has_exited() {
                return Ok(h);
            }
        }
        let h = Helper::start(&self.exe)?;
        let mut all = self.all.lock().unwrap();
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&h));
        Ok(h)
    }

    fn give_back(&self, h: Arc<Helper>) {
        // A helper that has grown too big goes, and the next file gets a fresh one.
        if let Some(why) = h.worn() {
            Log::info(format!("Reader helper retired: {why}"));
            h.close();
            return;
        }
        if self.disposed.load(Ordering::SeqCst) {
            h.kill("closing");
        } else {
            self.idle.lock().unwrap().push(h);
        }
    }

    /// What `extract::extract` yields, read by a helper.
    pub fn extract(&self, ctx: &ExtractContext, category: FileCategory, cancel: &Cancel, sink: Sink<'_>) -> Result<ExtractEnd> {
        let helper = self.take()?;
        let request = protocol::RequestBody {
            path: ctx.path.clone(),
            category,
            start_unit: ctx.start_unit,
            unit_budget: ctx.unit_budget,
            settings: ctx.settings.clone(),
        };
        let mut finished = false;
        let result = (|| -> Result<ExtractEnd> {
            if helper.send(protocol::REQUEST, &request).is_err() {
                return Err(helper.failure().into());
            }
            helper.busy(&ctx.path);
            loop {
                cancel.check()?;
                let (kind, payload) = helper.read()?;
                helper.touch();
                match kind {
                    protocol::ITEM => {
                        let item = protocol::decode_item(&payload)?;
                        drop(payload);
                        // Embedding a batch, or a pause, can take longer than the stall limit: the helper is waiting
                        // on the app meanwhile, not stuck (it was killed for it and the file marked unreadable).
                        helper.waiting_on_app.store(true, Ordering::SeqCst);
                        let sunk = sink(item);
                        helper.waiting_on_app.store(false, Ordering::SeqCst);
                        helper.touch();
                        sunk?;
                    }
                    protocol::END => {
                        let end: protocol::EndBody = serde_json::from_slice(&payload)?;
                        finished = true;
                        return Ok(ExtractEnd { next_unit: end.next_unit, title: end.title });
                    }
                    protocol::FAILED => {
                        let failed: protocol::FailedBody = serde_json::from_slice(&payload)?;
                        finished = true;
                        // A reader's own error says it all; a protocol problem or a panic says what it was.
                        let why = if failed.kind == "Error" { failed.message } else { format!("{}: {}", failed.kind, failed.message) };
                        return Err(ExtractionError(why).into());
                    }
                    _ => {}
                }
            }
        })();
        // A helper whose file was read to the end takes the next one; one stopped halfway (an error, a cancel) is still
        // writing, and goes.
        helper.idle();
        if finished && !helper.has_exited() {
            self.give_back(helper);
        } else {
            helper.kill("stopped halfway");
        }
        result
    }

    /// Runs a small job in a helper. None when the helper fails, crashes or takes longer than `timeout` (it's ended
    /// then): a preview or an estimate simply goes without.
    pub fn call(&self, call: &CallBody, timeout: Duration) -> Option<ResultBody> {
        let helper = match self.take() {
            Ok(h) => h,
            Err(e) => {
                Log::warn(format!("Reader helper for {}: {e}", call.op));
                return None;
            }
        };
        helper.busy(&call.path);
        *helper.deadline.lock().unwrap() = Some((Instant::now() + timeout, timeout));
        let mut ok = false;
        let result = (|| -> std::result::Result<Option<ResultBody>, ExtractionError> {
            if helper.send(protocol::CALL, call).is_err() {
                return Err(helper.failure());
            }
            let (kind, payload) = helper.read()?;
            ok = true;
            if kind == protocol::RESULT {
                return Ok(protocol::decode_result(&payload).ok());
            }
            Log::warn(format!("Reader helper {} {}: {}", call.op, call.path, String::from_utf8_lossy(&payload)));
            Ok(None)
        })();
        helper.idle();
        if ok && !helper.has_exited() {
            self.give_back(helper);
        } else {
            helper.kill("stopped halfway");
        }
        match result {
            Ok(r) => r,
            Err(e) => {
                Log::warn(format!("Reader helper {} {}: {e}", call.op, call.path));
                None
            }
        }
    }

    fn tick(&self, memory_too: bool) {
        let expired: Vec<Arc<Helper>> = {
            let mut idle = self.idle.lock().unwrap();
            let (old, keep): (Vec<_>, Vec<_>) = idle.drain(..).partition(|h| h.last_use.lock().unwrap().elapsed() > IDLE_LIMIT);
            *idle = keep;
            old
        };
        for h in expired {
            h.close();
        }
        let running: Vec<Arc<Helper>> = self.all.lock().unwrap().iter().filter_map(Weak::upgrade).collect();
        for h in running {
            h.check(memory_too);
        }
    }

    /// Ends every helper (the app is closing).
    pub fn shutdown(&self) {
        self.disposed.store(true, Ordering::SeqCst);
        let running: Vec<Arc<Helper>> = self.all.lock().unwrap().iter().filter_map(Weak::upgrade).collect();
        for h in running {
            h.kill("closing");
        }
        self.idle.lock().unwrap().clear();
    }

    /// How many helpers run, and their memory (for `nuiter-cli bench`).
    pub fn helpers(&self) -> (usize, u64, u32) {
        let running: Vec<Arc<Helper>> = self.all.lock().unwrap().iter().filter_map(Weak::upgrade).filter(|h| !h.has_exited()).collect();
        let bytes = running.iter().map(|h| crate::win::process_private_bytes(h.handle())).sum();
        let handles = running.iter().map(|h| crate::win::process_handle_count(h.handle())).sum();
        (running.len(), bytes, handles)
    }
}

impl Drop for ExtractionHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}
