use serde::{Deserialize, Serialize};

use crate::paths::{Log, Paths};

/// What the user chose, in settings.json (the same file and names the .NET version used).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Settings {
    /// Folders indexed for documents, images, PDFs, audio and video.
    pub folders: Vec<String>,
    /// Git repositories / folders indexed for code search (searched with the "code:" prefix).
    pub code_folders: Vec<String>,
    /// Paths (or path prefixes) that are never indexed.
    pub excluded_paths: Vec<String>,
    /// File extensions (with dot) that are never indexed.
    pub excluded_extensions: Vec<String>,

    pub index_images: bool,
    pub index_audio: bool,
    pub index_video: bool,
    pub ocr_images: bool,

    /// Open Nuiter from the Windows taskbar search box.
    pub replace_taskbar_search: bool,
    /// Open Nuiter instead of Windows Search when Win+S is pressed.
    pub intercept_win_s: bool,
    /// Additional global hotkey, e.g. "Win+Alt+S". Empty disables it.
    pub hotkey: String,
    pub start_with_windows: bool,

    /// The backlog waits on battery power. Off by default: new files are indexed right away either way, and Energy
    /// Saver makes the backlog wait anyway.
    pub pause_on_battery: bool,
    /// The backlog waits while Windows' Energy Saver is on (new files are still indexed).
    pub pause_on_energy_saver: bool,
    /// Index the backlog only between `index_from` and `index_to` ("HH:mm", across midnight when To comes first).
    pub index_only_at_hours: bool,
    pub index_from: String,
    pub index_to: String,

    /// auto | gpu | cpu
    pub backend: String,
    /// The search model stops after this many idle minutes (0 = keep running).
    pub worker_idle_minutes: i32,
    /// Preferred editor for code results: "code", "cursor", "zed", ... or empty for the default app.
    pub code_editor: String,
    pub max_pdf_pages: i32,

    /// Folder of the index (index.db); empty: the data folder. Changed through `move_index_to`.
    pub index_directory: String,
    /// Folder of the model files (~865 MB); empty: the runtime's models folder. Changed through `move_models_to`.
    pub models_directory: String,
    /// Set by Settings when a new folder is chosen: the next start moves the files there before anything opens them
    /// (`storage_move`), then makes it `index_directory` / `models_directory`.
    pub move_index_to: Option<String>,
    pub move_models_to: Option<String>,

    /// Once a day, ask GitHub's releases of `update_repository` for a newer version. Installing is always a click;
    /// the download is checked against the release's SHA-256.
    pub check_for_updates: bool,
    pub update_repository: String,

    #[serde(skip)]
    pub is_first_run: bool,
    /// settings.json was there but couldn't be read: these are defaults standing in for the user's choices (a copy of
    /// the file is kept as settings.json.bad). Nothing may be removed from the index on their account.
    #[serde(skip)]
    pub damaged: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            folders: vec![],
            code_folders: vec![],
            excluded_paths: vec![],
            excluded_extensions: vec![],
            index_images: true,
            index_audio: true,
            index_video: true,
            ocr_images: true,
            replace_taskbar_search: true,
            intercept_win_s: true,
            hotkey: "Win+Alt+S".into(),
            start_with_windows: true,
            pause_on_battery: false,
            pause_on_energy_saver: true,
            index_only_at_hours: false,
            index_from: "01:00".into(),
            index_to: "07:00".into(),
            backend: "auto".into(),
            worker_idle_minutes: 10,
            code_editor: "code".into(),
            max_pdf_pages: crate::extract::pdf::MAX_PAGES as i32,
            index_directory: String::new(),
            models_directory: String::new(),
            move_index_to: None,
            move_models_to: None,
            check_for_updates: true,
            update_repository: "motbrd/Nuiter".into(),
            is_first_run: false,
            damaged: false,
        }
    }
}

impl Settings {
    /// Reads settings.json; a fresh install gets the defaults (nothing is read until folders are chosen) and saves them.
    pub fn load() -> Settings {
        Paths::ensure();
        let path = Paths::settings_file();
        if path.exists() {
            let parsed = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|bytes| {
                // .NET wrote a UTF-8 BOM in some versions.
                let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&bytes);
                serde_json::from_slice::<Settings>(bytes).map_err(|e| e.to_string())
            });
            match parsed {
                Ok(s) => return s,
                Err(e) => {
                    // The defaults have no folders: a scan with them would take every file out of the index. They stand
                    // in until the user saves settings again, and the file that failed is kept for them to look at.
                    let bad = path.with_extension("json.bad");
                    let kept = std::fs::copy(&path, &bad).is_ok();
                    Log::error(format!(
                        "Failed to read settings, using defaults until they're saved again{}: {e}",
                        if kept { format!(" (the file is kept as {})", bad.display()) } else { String::new() }
                    ));
                    return Settings { damaged: true, ..Settings::default() };
                }
            }
        }
        let s = Settings { is_first_run: true, ..Settings::default() };
        if let Err(e) = s.save() {
            Log::warn(format!("Cannot save settings: {e}"));
        }
        s
    }

    /// Writes settings.json. Defaults standing in for an unreadable file (`damaged`) aren't written: they'd become the
    /// user's settings at the next start. Saving from Settings clears `damaged` first.
    pub fn save(&self) -> std::io::Result<()> {
        if self.damaged {
            return Ok(());
        }
        Paths::ensure();
        let path = Paths::settings_file();
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }
}
