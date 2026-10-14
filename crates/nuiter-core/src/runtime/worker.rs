//! Runs llama.cpp's llama-server as a helper process (on 127.0.0.1, a random port, a key only Nuiter knows) and talks to
//! its /v1/embeddings endpoint. Started on demand, stopped after an idle period, and killed together with Nuiter
//! through a job object.
//!
//! `WorkerRole::Query`: the text model only (~310 MB), for searches; it stays loaded while Nuiter is in use.
//! `WorkerRole::Index`: text + vision/audio encoders (~1 GB), for indexing; it unloads soon after the queue empties.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::HANDLE;

use super::{EmbedInput, EmbedPriority, Embedder, EmbeddingError, cpu, installer, job, llama_http, prompts, reference};
use crate::cancel::{self, Cancel};
use crate::paths::{Log, Paths};
use crate::settings::Settings;
use crate::vector_math;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRole {
    Query,
    Index,
}

/// The indexing worker gives its memory back this soon after the last batch.
const INDEX_IDLE: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// Indexing requests in flight at once: llama-server runs as many slots (-np) and batches them on the GPU.
pub fn index_slots() -> usize {
    std::env::var("NUITER_INDEX_SLOTS").ok().and_then(|v| v.parse().ok()).filter(|&n: &usize| n > 0).unwrap_or(4)
}

/// A counting semaphore whose waits give up when the work is cancelled.
struct Semaphore {
    count: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Self { count: Mutex::new(n), cv: Condvar::new() }
    }

    fn acquire(&self, cancel: &Cancel) -> Result<(), cancel::Cancelled> {
        let mut c = self.count.lock().unwrap();
        while *c == 0 {
            cancel.check()?;
            c = self.cv.wait_timeout(c, Duration::from_millis(100)).unwrap().0;
        }
        *c -= 1;
        Ok(())
    }

    fn release(&self) {
        *self.count.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

/// The GPU's vectors are wrong: the CPU takes over, even when the GPU was chosen.
#[derive(Debug)]
struct GpuMismatch(String);

impl std::fmt::Display for GpuMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GpuMismatch {}

struct Inner {
    settings: Box<dyn Fn() -> Settings + Send + Sync>,
    role: WorkerRole,
    start_lock: Mutex<()>,
    query_slot: Semaphore,
    index_slot: Semaphore,
    process: Mutex<Option<Child>>,
    port: AtomicU16,
    api_key: Mutex<String>,
    last_use: Mutex<Instant>,
    gpu_failed: AtomicBool,
    inflight: AtomicUsize,
    ready: AtomicBool,
    device: Mutex<String>,
    ready_since: Mutex<Option<Instant>>,
    on_started: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    disposed: AtomicBool,
}

#[derive(Clone)]
pub struct EmbeddingWorker(Arc<Inner>);

impl EmbeddingWorker {
    pub fn new(settings: impl Fn() -> Settings + Send + Sync + 'static, role: WorkerRole) -> EmbeddingWorker {
        let w = EmbeddingWorker(Arc::new(Inner {
            settings: Box::new(settings),
            role,
            start_lock: Mutex::new(()),
            query_slot: Semaphore::new(1),
            index_slot: Semaphore::new(index_slots()),
            process: Mutex::new(None),
            port: AtomicU16::new(0),
            api_key: Mutex::new(String::new()),
            last_use: Mutex::new(Instant::now()),
            gpu_failed: AtomicBool::new(false),
            inflight: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            device: Mutex::new(String::new()),
            ready_since: Mutex::new(None),
            on_started: Mutex::new(None),
            disposed: AtomicBool::new(false),
        }));
        let weak = Arc::downgrade(&w.0);
        let _ = std::thread::Builder::new().name(format!("Worker{role:?}Idle")).stack_size(256 << 10).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(30));
                let Some(inner) = weak.upgrade() else { return };
                if inner.disposed.load(Ordering::SeqCst) {
                    return;
                }
                EmbeddingWorker(inner).check_idle();
            }
        });
        w
    }

    pub fn role(&self) -> WorkerRole {
        self.0.role
    }

    /// Called (on the worker's thread) each time the model is up.
    pub fn on_started(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.0.on_started.lock().unwrap() = Some(Box::new(f));
    }

    pub fn port(&self) -> u16 {
        self.0.port.load(Ordering::SeqCst)
    }

    /// "GPU" or "CPU" once started, "" before.
    pub fn device(&self) -> String {
        self.0.device.lock().unwrap().clone()
    }

    pub fn is_running(&self) -> bool {
        self.0.ready.load(Ordering::SeqCst)
            && self.port() != 0
            && self.0.process.lock().unwrap().as_mut().is_some_and(|p| matches!(p.try_wait(), Ok(None)))
    }

    /// Starts the server in the background so the first query doesn't pay the model load time.
    pub fn warm_up(&self) {
        if !installer::is_installed() || self.is_running() {
            return;
        }
        let me = self.clone();
        let _ = std::thread::Builder::new().name("WorkerWarmUp".into()).spawn(move || {
            if let Err(e) = me.ensure_started(&Cancel::never()) {
                Log::warn(format!("WarmUp failed: {e}"));
            }
        });
    }

    fn post(&self, inputs: &[EmbedInput]) -> Result<Vec<Vec<f32>>> {
        let port = self.port();
        let key = self.0.api_key.lock().unwrap().clone();
        let length = llama_http::request_length(installer::MODEL_ID, inputs);
        let resp = llama_http::post(port, "/v1/embeddings", &key, length, REQUEST_TIMEOUT, |w| {
            llama_http::write_embeddings_request(w, installer::MODEL_ID, inputs)
        })?;
        if resp.status != 200 {
            let text = String::from_utf8_lossy(&resp.body);
            let short: String = text.chars().take(400).collect();
            return Err(EmbeddingError::refused(format!("llama-server returned {}: {short}", resp.status)).into());
        }
        let doc: serde_json::Value = serde_json::from_slice(&resp.body).map_err(|e| EmbeddingError::refused(format!("bad answer from llama-server: {e}")))?;
        let mut result: Vec<Vec<f32>> = vec![vec![]; inputs.len()];
        for d in doc["data"].as_array().ok_or_else(|| EmbeddingError::refused("no data in llama-server's answer"))? {
            let idx = d["index"].as_u64().unwrap_or(0) as usize;
            let mut v: Vec<f32> = d["embedding"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect()).unwrap_or_default();
            vector_math::normalize(&mut v);
            if idx < result.len() {
                result[idx] = v;
            }
        }
        if result.iter().any(Vec::is_empty) {
            return Err(EmbeddingError::refused("llama-server's answer lacks vectors").into());
        }
        Ok(result)
    }

    fn ensure_started(&self, cancel: &Cancel) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let _lock = self.0.start_lock.lock().unwrap();
        if self.is_running() {
            return Ok(());
        }
        // Closing: a request that was waiting mustn't start the model again.
        if self.0.disposed.load(Ordering::SeqCst) {
            return Err(EmbeddingError::unavailable("Nuiter is closing").into());
        }
        cancel.check()?;
        if !installer::is_installed() {
            return Err(EmbeddingError::unavailable("The EmbeddingGemma 2 runtime is not installed yet.").into());
        }
        let s = (self.0.settings)();
        let use_gpu = s.backend != "cpu" && !self.0.gpu_failed.load(Ordering::SeqCst);
        match self.start(use_gpu, cancel) {
            Ok(()) => Ok(()),
            // Wrong vectors fall back even when the GPU was chosen: they'd silently spoil the index.
            Err(e) if use_gpu && (s.backend == "auto" || e.downcast_ref::<GpuMismatch>().is_some()) && !cancel::is_cancelled(&e) => {
                Log::warn(format!("GPU start failed ({e}); falling back to CPU"));
                self.0.gpu_failed.store(true, Ordering::SeqCst);
                self.start(false, cancel)
            }
            Err(e) => Err(e),
        }
    }

    /// The GPU must make the vectors the CPU makes: some drivers run the model and return nonsense without an error. The
    /// index worker checks a picture too, since the vision encoder runs apart.
    fn check_gpu(&self) -> Result<()> {
        let mut checks: Vec<(EmbedInput, &str, &str)> = vec![];
        if !reference::TEXT_VECTOR.is_empty() {
            checks.push((EmbedInput::Text(prompts::document(None, reference::TEXT)), reference::TEXT_VECTOR, "text"));
        }
        if self.0.role == WorkerRole::Index && !reference::IMAGE_VECTOR.is_empty() {
            checks.push((EmbedInput::Image(reference::image()), reference::IMAGE_VECTOR, "image"));
        }
        if checks.is_empty() {
            return Ok(());
        }
        let inputs: Vec<EmbedInput> = checks.iter().map(|c| c.0.clone()).collect();
        let got = self.post(&inputs)?;
        for (i, (_, expected, what)) in checks.iter().enumerate() {
            let e = vector_math::from_blob(&vector_math::base64_decode(expected).unwrap_or_default());
            let cos = vector_math::dot(&got[i], &e);
            if cos < reference::MIN_COSINE {
                self.kill();
                return Err(GpuMismatch(format!("the GPU's {what} vectors are wrong (cosine {cos:.3} to the CPU's)")).into());
            }
        }
        Ok(())
    }

    fn start(&self, gpu: bool, cancel: &Cancel) -> Result<()> {
        self.kill();
        let port = free_port()?;
        // One thread per performance core: efficiency cores and hyper-threads make every step wait for the slowest.
        let threads = cpu::performance_cores().clamp(2, 16);
        let slots = index_slots();
        let model = installer::model_path();
        let mut args: Vec<String> = vec!["-m".into(), model.to_string_lossy().into_owned(), "--embeddings".into()];
        match self.0.role {
            WorkerRole::Query => args.extend(["--no-mmproj", "-c", "2048", "-np", "1", "-b", "2048", "-ub", "2048"].map(String::from)),
            // One input must fit one micro-batch (a 30 s clip is ~750 tokens). Images arrive already sized for their
            // token budget, so mtmd must not rescale them.
            WorkerRole::Index => args.extend([
                "--mmproj".into(),
                installer::mmproj_path().to_string_lossy().into_owned(),
                "-c".into(),
                (2048 * slots).to_string(),
                "-np".into(),
                slots.to_string(),
                "-b".into(),
                (2048 * slots).to_string(),
                "-ub".into(),
                "2048".into(),
                "--image-min-tokens".into(),
                "1".into(),
                "--image-max-tokens".into(),
                "1120".into(),
            ]),
        }
        // A key known only to this process: any program on the PC (or a web page) could otherwise call the server on
        // 127.0.0.1. /health stays open. It goes in the environment (LLAMA_API_KEY), not the command line, which any
        // process can read.
        let key = crate::win::hex(&crate::win::random_bytes(24));
        args.extend(["--host".into(), "127.0.0.1".into(), "--port".into(), port.to_string(), "--no-webui".into(), "-t".into(), threads.to_string()]);
        if gpu {
            args.extend(["-ngl", "99"].map(String::from));
        } else {
            args.extend(["--device", "none", "-ngl", "0"].map(String::from));
        }
        if !gpu && self.0.role == WorkerRole::Index {
            args.push("--no-mmproj-offload".into());
        }
        let exe = installer::server_exe();
        let log_path = Paths::home().join(format!("worker-{}.log", if self.0.role == WorkerRole::Query { "query" } else { "index" }));
        let log = std::fs::File::create(&log_path)?;
        let sw = Instant::now();
        let child = Command::new(&exe)
            .args(&args)
            .env("LLAMA_API_KEY", &key)
            .current_dir(exe.parent().unwrap_or(std::path::Path::new(".")))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .creation_flags(0x0800_0000 /* CREATE_NO_WINDOW */ | 0x0000_4000 /* BELOW_NORMAL_PRIORITY_CLASS */)
            .spawn()
            .map_err(|e| EmbeddingError::unavailable(format!("llama-server didn't start: {e}")))?;
        job::assign(HANDLE(child.as_raw_handle() as _));
        *self.0.api_key.lock().unwrap() = key;
        self.0.port.store(port, Ordering::SeqCst);
        *self.0.process.lock().unwrap() = Some(child);
        *self.0.device.lock().unwrap() = if gpu { "GPU".into() } else { "CPU".into() };

        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if let Err(e) = cancel.check() {
                self.kill();
                return Err(e.into());
            }
            if let Some(code) = self.0.process.lock().unwrap().as_mut().and_then(|p| p.try_wait().ok().flatten()) {
                self.0.port.store(0, Ordering::SeqCst);
                return Err(EmbeddingError::unavailable(format!("llama-server exited with code {code}; see {}", log_path.display())).into());
            }
            if let Ok(r) = llama_http::get(port, "/health", Duration::from_secs(2))
                && r.status == 200
            {
                // The port was free when it was picked, not necessarily when llama-server went to take it: whatever
                // answers must be the process just started, before it gets the key and the user's files.
                // (When Windows can't say who listens, it goes on: that's no sign of anything wrong.)
                let pid = self.0.process.lock().unwrap().as_ref().map(Child::id);
                if let (Some(ours), Some(listener)) = (pid, crate::win::tcp_listener_pid(port))
                    && ours != listener
                {
                    self.kill();
                    return Err(EmbeddingError::unavailable(format!("port {port} is held by another program (process {listener})")).into());
                }
                break;
            }
            if Instant::now() > deadline {
                self.kill();
                return Err(EmbeddingError::unavailable("llama-server did not become ready in time").into());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        // Checked before anyone may use it (is_running needs ready).
        if gpu {
            self.check_gpu()?;
        }
        self.0.ready.store(true, Ordering::SeqCst);
        *self.0.ready_since.lock().unwrap() = Some(Instant::now());
        Log::info(format!("Embedding worker ({:?}) ready on port {port} ({}) in {} ms", self.0.role, self.device(), sw.elapsed().as_millis()));
        *self.0.last_use.lock().unwrap() = Instant::now();
        if let Some(f) = self.0.on_started.lock().unwrap().as_ref() {
            f();
        }
        Ok(())
    }

    fn check_idle(&self) {
        let idle = match self.0.role {
            WorkerRole::Index => INDEX_IDLE,
            WorkerRole::Query => Duration::from_secs(60 * (self.0.settings)().worker_idle_minutes.max(0) as u64),
        };
        if idle.is_zero() || !self.is_running() || self.0.inflight.load(Ordering::SeqCst) > 0 {
            return;
        }
        if self.0.last_use.lock().unwrap().elapsed() > idle {
            Log::info(format!("Embedding worker ({:?}) idle; stopping", self.0.role));
            self.kill();
        }
    }

    pub fn kill(&self) {
        self.0.ready.store(false, Ordering::SeqCst);
        // Out of the lock first: waiting for the process to go (a GPU driver can take a while) mustn't hold up
        // `is_running`, which the UI thread asks for the status line.
        let process = self.0.process.lock().unwrap().take();
        if let Some(mut p) = process {
            let _ = p.kill();
            let _ = p.wait();
        }
        self.0.port.store(0, Ordering::SeqCst);
    }

    pub fn dispose(&self) {
        self.0.disposed.store(true, Ordering::SeqCst);
        self.kill();
    }
}

impl Embedder for EmbeddingWorker {
    fn embed(&self, inputs: &[EmbedInput], priority: EmbedPriority, cancel: &Cancel) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(vec![]);
        }
        let slot = if priority == EmbedPriority::Query { &self.0.query_slot } else { &self.0.index_slot };
        slot.acquire(cancel)?;
        self.0.inflight.fetch_add(1, Ordering::SeqCst);
        let result = (|| -> Result<Vec<Vec<f32>>> {
            let mut attempt = 0;
            loop {
                cancel.check()?;
                // A model that can't be started is no fault of the input (the indexer tries the file again later).
                if let Err(e) = self.ensure_started(cancel) {
                    if cancel::is_cancelled(&e) || EmbeddingError::is_unavailable(&e) {
                        return Err(e);
                    }
                    return Err(EmbeddingError::unavailable(format!("the model didn't start: {e:#}")).into());
                }
                *self.0.last_use.lock().unwrap() = Instant::now();
                match self.post(inputs) {
                    Ok(v) => return Ok(v),
                    // Still loading.
                    Err(e) if e.downcast_ref::<EmbeddingError>().is_some_and(|x| x.message.contains(" 503")) && attempt < 20 => {
                        cancel.check()?;
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    Err(e) if e.downcast_ref::<EmbeddingError>().is_none() && attempt == 0 && !self.is_running() => {
                        Log::warn(format!("Embedding worker died ({e}); restarting"));
                    }
                    // No local port to connect from: this PC's trouble, not the input's (the file is read again later).
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|io| matches!(io.kind(), std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AddrNotAvailable)) =>
                    {
                        return Err(EmbeddingError::unavailable(format!("couldn't connect to llama-server: {e}")).into());
                    }
                    Err(e) if e.downcast_ref::<EmbeddingError>().is_none() => {
                        return Err(EmbeddingError::refused(format!("llama-server didn't answer: {e}")).into());
                    }
                    Err(e) => return Err(e),
                }
                attempt += 1;
            }
        })();
        *self.0.last_use.lock().unwrap() = Instant::now();
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
        slot.release();
        result
    }

    fn ready_since(&self) -> Option<Instant> {
        *self.0.ready_since.lock().unwrap()
    }
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr().map_err(|e| anyhow!(e))?.port())
}
