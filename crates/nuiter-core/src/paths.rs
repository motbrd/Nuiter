use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use crate::settings::Settings;

/// Well-known locations. Everything lives under %LOCALAPPDATA%\Nuiter unless NUITER_HOME is set.
pub struct Paths;

static INDEX_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);
static MODELS_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

impl Paths {
    fn default_home() -> PathBuf {
        let base = env_dir("LOCALAPPDATA").unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\AppData\Local"));
        base.join("Nuiter")
    }

    pub fn home() -> PathBuf {
        env_dir("NUITER_HOME").unwrap_or_else(Self::default_home)
    }

    /// The model runtime is shared between data directories (override with NUITER_RUNTIME).
    pub fn runtime() -> PathBuf {
        env_dir("NUITER_RUNTIME").unwrap_or_else(|| Self::default_home().join("runtime"))
    }

    pub fn llama_dir() -> PathBuf {
        Self::runtime().join("llama")
    }

    pub fn default_models_dir() -> PathBuf {
        Self::runtime().join("models")
    }

    /// Where the model files are: `Settings::models_directory`, else the runtime's models folder.
    pub fn models_dir() -> PathBuf {
        MODELS_DIR.read().unwrap().clone().unwrap_or_else(Self::default_models_dir)
    }

    /// Where the index lives (index.db with its -wal/-shm and .reading): `Settings::index_directory`, else `home`.
    /// Settings, the log and costs.json always stay in `home`.
    pub fn index_dir() -> PathBuf {
        INDEX_DIR.read().unwrap().clone().unwrap_or_else(Self::home)
    }

    pub fn database() -> PathBuf {
        Self::index_dir().join("index.db")
    }

    pub fn settings_file() -> PathBuf {
        Self::home().join("settings.json")
    }

    pub fn log_file() -> PathBuf {
        Self::home().join("nuiter.log")
    }

    pub fn temp() -> PathBuf {
        Self::home().join("tmp")
    }

    /// Takes the index and model folders from settings (empty: the defaults). Called once settings are loaded.
    pub fn use_settings(s: &Settings) {
        let index = (!s.index_directory.trim().is_empty()).then(|| PathBuf::from(&s.index_directory));
        let models = (!s.models_directory.trim().is_empty()).then(|| PathBuf::from(&s.models_directory));
        if let Some(i) = &index {
            let _ = std::fs::create_dir_all(i);
        }
        *INDEX_DIR.write().unwrap() = index;
        *MODELS_DIR.write().unwrap() = models;
    }

    pub fn ensure() {
        let _ = std::fs::create_dir_all(Self::home());
        let _ = std::fs::create_dir_all(Self::runtime());
        let _ = std::fs::create_dir_all(Self::temp());
    }

    /// The folder this executable is in.
    pub fn exe_dir() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// The log, `nuiter.log` in the data folder (kept under 5 MB: the older half goes to nuiter.log.old).
pub struct Log;

static LOG_GATE: Mutex<()> = Mutex::new(());
/// Reader helpers write to the app's log too: only the app turns it over, or two processes doing it at once lost the
/// older half.
static ROTATES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

impl Log {
    /// This process only appends to the log; the app keeps it under its size.
    pub fn never_rotate() {
        ROTATES.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn info(msg: impl AsRef<str>) {
        write("INFO", msg.as_ref());
    }

    pub fn warn(msg: impl AsRef<str>) {
        write("WARN", msg.as_ref());
    }

    pub fn error(msg: impl AsRef<str>) {
        write("ERROR", msg.as_ref());
    }
}

fn write(level: &str, msg: &str) {
    let line = format!("{} [{level}] {msg}\r\n", crate::win::local_time_text());
    let Ok(_gate) = LOG_GATE.lock() else { return };
    let path = Paths::log_file();
    // Logging must never fail the caller.
    if ROTATES.load(std::sync::atomic::Ordering::SeqCst)
        && let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > 5_000_000
    {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}
