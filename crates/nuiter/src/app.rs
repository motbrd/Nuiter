//! The app: the engine (`Core`, shared with background threads), the windows (UI thread only), the tray icon, the
//! shortcuts, downloads, updates and restarts.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use nuiter_core::extract::host::ExtractionHost;
use nuiter_core::index::indexer::{IndexPhase, Indexer};
use nuiter_core::index::store::IndexStore;
use nuiter_core::index::vectors::VectorIndex;
use nuiter_core::runtime::updates::{self, UpdateInfo, Version};
use nuiter_core::runtime::worker::{EmbeddingWorker, WorkerRole};
use nuiter_core::runtime::installer;
use nuiter_core::search::SearchEngine;
use nuiter_core::{Cancel, Log, Paths, Settings, file_kinds::pathx, storage_move};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::HSTRING;

use crate::search_window::SearchWindow;
use crate::settings_window::SettingsWindow;
use crate::shell::{hotkeys, launcher, registry, taskbar, tray};
use crate::ui::gfx::Color;
use crate::ui::{native, theme, window};

/// What the background threads share: settings, the index, the models, the indexer, and what downloads are doing.
pub struct Core {
    pub settings: Arc<RwLock<Settings>>,
    pub store: Arc<IndexStore>,
    pub vectors: Arc<VectorIndex>,
    /// Text-only model for searches (small, stays loaded while in use).
    pub worker: EmbeddingWorker,
    /// Full model with the vision and audio encoders, for indexing (unloads when the queue is empty).
    pub index_worker: EmbeddingWorker,
    pub indexer: Indexer,
    pub reader: Option<Arc<ExtractionHost>>,
    pub engine: SearchEngine,
    download: Mutex<Option<(String, f64)>>,
    download_error: Mutex<Option<String>>,
    /// A newer release found on GitHub, until it's installed.
    pub update: Mutex<Option<UpdateInfo>>,
    /// What the update download is doing now, or its last error.
    pub update_state: Mutex<Option<String>>,
    announced: Mutex<Option<Version>>,
    last_status: Mutex<Instant>,
}

static CORE: OnceLock<Arc<Core>> = OnceLock::new();

pub fn core() -> &'static Arc<Core> {
    CORE.get().expect("the app has started")
}

pub const GREEN: Color = Color::rgb(0x3F, 0xB9, 0x50);
pub const AMBER: Color = Color::rgb(0xD2, 0x99, 0x22);
pub const RED: Color = Color::rgb(0xF8, 0x51, 0x49);

impl Core {
    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn downloading(&self) -> bool {
        self.download.lock().unwrap().is_some()
    }

    /// The download's fraction done (0..1), when one runs.
    pub fn download_fraction(&self) -> Option<f64> {
        self.download.lock().unwrap().as_ref().map(|d| d.1)
    }

    /// One-line status for the search window footer, the tray tooltip and settings.
    pub fn status_line(&self) -> (String, Color) {
        if let Some((msg, f)) = self.download.lock().unwrap().clone() {
            return (format!("Downloading EmbeddingGemma 2 · {}% · {msg}", (f * 100.0).round()), AMBER);
        }
        if let Some(e) = self.download_error.lock().unwrap().clone() {
            return (format!("Model download failed: {e}"), RED);
        }
        let s = self.indexer.status();
        match s.phase {
            IndexPhase::Scanning => (format!("Scanning folders… · {} files indexed", thousands(s.files)), AMBER),
            IndexPhase::Indexing => (
                format!("Indexing · {} left · {}", thousands(s.queued as i64), s.current.as_deref().map(pathx::file_name).unwrap_or("")),
                AMBER,
            ),
            IndexPhase::Paused => match (self.indexer.paused(), self.indexer.wait_reason()) {
                (false, Some(why)) => (format!("Indexing waits: {why}"), AMBER),
                _ => ("Indexing paused".into(), AMBER),
            },
            IndexPhase::WaitingForRuntime => ("Search model not downloaded · words only · Download in Settings".into(), AMBER),
            IndexPhase::Idle => {
                let device = if self.worker.is_running() { format!(" · model on {}", self.worker.device()) } else { String::new() };
                (format!("Up to date · {} files · {} items{device}", thousands(s.files), thousands(s.chunks)), GREEN)
            }
        }
    }

    /// Something the status line shows changed: the windows and the tray follow, at most every 150 ms.
    fn raise_status(&self, force: bool) {
        {
            let mut last = self.last_status.lock().unwrap();
            if !force && last.elapsed() < Duration::from_millis(150) {
                return;
            }
            *last = Instant::now();
        }
        window::invoke(status_changed);
    }
}

/// "12,345".
pub fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

// ---------- the UI thread's state ----------

struct UiState {
    msg: HWND,
    search: Rc<RefCell<SearchWindow>>,
    settings: Option<Rc<RefCell<SettingsWindow>>>,
    tray_icon: HICON,
}

thread_local! {
    static UI: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

/// Runs `f` on the search window now, or right after the message being handled when it's busy.
pub fn with_search(f: impl FnOnce(&mut SearchWindow) + 'static) {
    let s = UI.with(|u| u.borrow().as_ref().map(|u| u.search.clone()));
    let Some(s) = s else { return };
    if let Ok(mut w) = s.try_borrow_mut() {
        let _busy = window::Busy::new();
        f(&mut w);
        return;
    }
    window::later(move || {
        if let Ok(mut w) = s.try_borrow_mut() {
            let _busy = window::Busy::new();
            f(&mut w);
        }
    });
}

pub fn with_settings(f: impl FnOnce(&mut SettingsWindow) + 'static) {
    let s = UI.with(|u| u.borrow().as_ref().and_then(|u| u.settings.clone()));
    let Some(s) = s else { return };
    if let Ok(mut w) = s.try_borrow_mut() {
        let _busy = window::Busy::new();
        f(&mut w);
        return;
    }
    window::later(move || {
        if let Ok(mut w) = s.try_borrow_mut() {
            let _busy = window::Busy::new();
            f(&mut w);
        }
    });
}

fn msg_window() -> HWND {
    UI.with(|u| u.borrow().as_ref().map(|u| u.msg).unwrap_or_default())
}

fn status_changed() {
    let (text, _) = core().status_line();
    with_search(|s| s.status_changed());
    with_settings(|s| s.status_changed());
    let tip = format!("Nuiter: {text}");
    tray::set_tip(msg_window(), &tip.chars().take(127).collect::<String>());
}

// ---------- startup ----------

pub fn run(args: Vec<String>) -> i32 {
    // Started by a restart (an update, a moved index): the old process must be gone before its files are touched.
    if let Some(i) = args.iter().position(|a| a == "--after")
        && let Some(pid) = args.get(i + 1).and_then(|p| p.parse::<u32>().ok())
    {
        // SAFETY: waits for another process to end.
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                WaitForSingleObject(h, 60_000);
                let _ = CloseHandle(h);
            }
        }
    }
    // One instance per data directory (NUITER_HOME lets a test instance run next to the normal one).
    let key = match std::env::var("NUITER_HOME") {
        Ok(h) if !h.is_empty() => format!(".{}", &nuiter_core::win::hash_hex(h.to_lowercase().as_bytes())[..12]),
        _ => String::new(),
    };
    // SAFETY: named kernel objects owned by this process for its life.
    let show_event = unsafe {
        let _mutex = CreateMutexW(None, true, &HSTRING::from(format!(r"Local\Nuiter.SingleInstance{key}")));
        let first = GetLastError() != ERROR_ALREADY_EXISTS;
        let ev = CreateEventW(None, false, false, &HSTRING::from(format!(r"Local\Nuiter.Show{key}"))).unwrap_or_default();
        if !first {
            // Already running: ask that instance to open its search window.
            let _ = SetEvent(ev);
            return 0;
        }
        std::mem::forget(_mutex);
        ev
    };

    std::panic::set_hook(Box::new(|info| Log::error(format!("Panic: {info}"))));
    nuiter_core::win::init_com_sta();
    // SAFETY: OLE for drag and drop and the clipboard on the UI thread.
    unsafe {
        let _ = windows::Win32::System::Ole::OleInitialize(None);
    }
    Paths::ensure();
    Log::info(format!("Nuiter {} starting (Rust)", nuiter_core::VERSION));
    nuiter_core::memory::limit_sqlite_heap(16 << 20);
    let mut settings = Settings::load();
    Paths::use_settings(&settings);
    // A new index or model folder chosen in Settings: the files move now, before anything opens them.
    let move_problems =
        if settings.move_index_to.is_some() || settings.move_models_to.is_some() { storage_move::apply_pending(&mut settings) } else { vec![] };
    let first_run = settings.is_first_run;
    let t = theme::current();
    native::allow_dark_menus(t.dark);

    let shared = Arc::new(RwLock::new(settings.clone()));
    let store = match IndexStore::open_default() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            Log::error(format!("Cannot open the index: {e}"));
            native::message_box(HWND::default(), &format!("Nuiter can't open its index:\n{e}"), false);
            return 1;
        }
    };
    let vectors = Arc::new(VectorIndex::default());
    let s1 = shared.clone();
    let worker = EmbeddingWorker::new(move || s1.read().unwrap().clone(), WorkerRole::Query);
    let s2 = shared.clone();
    let index_worker = EmbeddingWorker::new(move || s2.read().unwrap().clone(), WorkerRole::Index);
    // Files are read in helper processes (Nuiter.exe extract-service).
    let reader = ExtractionHost::try_create();
    Log::info(if reader.is_some() { "Reading files in helper processes" } else { "Reading files in this process" });
    let indexer = Indexer::new(store.clone(), vectors.clone(), Arc::new(index_worker.clone()), settings.clone(), reader.clone());
    let engine = SearchEngine::new(store.clone(), vectors.clone(), Arc::new(worker.clone()));
    let _ = CORE.set(Arc::new(Core {
        settings: shared,
        store,
        vectors,
        worker,
        index_worker,
        indexer,
        reader,
        engine,
        download: Mutex::new(None),
        download_error: Mutex::new(None),
        update: Mutex::new(None),
        update_state: Mutex::new(None),
        announced: Mutex::new(None),
        last_status: Mutex::new(Instant::now() - Duration::from_secs(1)),
    }));
    let c = core();
    c.indexer.on_status(|_| core().raise_status(false));
    c.indexer.on_stopped(|why| window::invoke(move || restart(&why)));
    c.index_worker.on_started(|| {
        std::thread::spawn(|| {
            if let Err(e) = core().indexer.verify_model(&Cancel::never()) {
                Log::warn(format!("Model verification failed: {e}"));
            }
        });
    });

    let msg = window::init_message_window();
    window::on_app_message(app_message);
    let icon = tray_icon();
    tray::add(msg, icon, "Nuiter");
    let search = SearchWindow::create();
    UI.with(|u| *u.borrow_mut() = Some(UiState { msg, search, settings: None, tray_icon: icon }));

    taskbar::start(settings.replace_taskbar_search);
    hotkeys::set_win_s(settings.intercept_win_s);
    hotkeys::set_hotkey(msg, &settings.hotkey);
    registry::apply_autostart(settings.start_with_windows);

    let show = show_event.0 as usize;
    let _ = std::thread::Builder::new().name("ShowSignal".into()).stack_size(128 << 10).spawn(move || loop {
        // SAFETY: waits on our own event.
        unsafe { WaitForSingleObject(HANDLE(show as _), INFINITE) };
        window::invoke(|| with_search(|s| s.show_at(taskbar::box_on_cursor_monitor())));
    });

    std::thread::spawn(|| core().indexer.start());
    for p in move_problems {
        tray::notify(msg, &p, true);
    }
    start_update_loop();
    start_release_timer();

    let background = args.iter().any(|a| a == "--background");
    let settings_first = first_run || args.iter().any(|a| a == "--settings");
    if settings_first {
        show_settings();
    }
    // The model is never downloaded without asking: 865 MB can cost money on a phone's hotspot or a capped plan. Started
    // with Windows, nobody is there to answer: the tray says where to get it instead.
    let missing = !installer::is_installed();
    if missing && background {
        tray::notify(msg, &format!("Search by meaning needs its model ({}). Download it from Settings.", installer::download_size_text()), false);
    }
    if !settings_first && !background {
        window::invoke(move || {
            if missing {
                offer_download();
            }
            with_search(|s| s.show_at(None));
        });
    } else if missing && !background {
        window::invoke(offer_download);
    }
    window::run_message_loop();
    0
}

fn tray_icon() -> HICON {
    // SAFETY: plain call.
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let ico = if theme::current().taskbar_dark { native::ICON_LIGHT } else { native::ICON_DARK };
    native::icon_from_ico(ico, size).unwrap_or_default()
}

const CMD_SEARCH: u32 = 1;
const CMD_SETTINGS: u32 = 2;
const CMD_PAUSE: u32 = 3;
const CMD_RESCAN: u32 = 4;
const CMD_DOWNLOAD: u32 = 5;
const CMD_DATA: u32 = 6;
const CMD_UPDATE: u32 = 7;
const CMD_QUIT: u32 = 8;

fn app_message(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    if msg == WM_HOTKEY && w.0 as i32 == hotkeys::HOTKEY_ID {
        on_hotkey();
        return Some(LRESULT(0));
    }
    if msg == tray::WM_TRAY {
        let event = (l.0 & 0xFFFF) as u32;
        match event {
            WM_LBUTTONUP => with_search(|s| s.toggle(None)),
            WM_CONTEXTMENU | WM_RBUTTONUP => tray_menu(hwnd),
            0x0405
                if core().update.lock().unwrap().is_some() => {
                    show_settings();
                }
            _ => {}
        }
        return Some(LRESULT(0));
    }
    if msg == tray::taskbar_created_message() {
        let icon = UI.with(|u| u.borrow().as_ref().map(|u| u.tray_icon).unwrap_or_default());
        tray::add(hwnd, icon, "Nuiter");
        return Some(LRESULT(0));
    }
    if msg == WM_SETTINGCHANGE && l.0 != 0 {
        // SAFETY: for WM_SETTINGCHANGE, lParam is a null-terminated string when set.
        let area = unsafe { windows::core::PCWSTR(l.0 as *const u16).to_string() }.unwrap_or_default();
        if area == "ImmersiveColorSet" {
            theme_changed();
        }
        return None;
    }
    None
}

fn theme_changed() {
    let t = theme::refresh();
    native::allow_dark_menus(t.dark);
    with_search(|s| s.theme_changed());
    with_settings(|s| s.theme_changed());
    let icon = tray_icon();
    let old = UI.with(|u| {
        let mut u = u.borrow_mut();
        let u = u.as_mut()?;
        Some(std::mem::replace(&mut u.tray_icon, icon))
    });
    tray::set_icon(msg_window(), icon);
    if let Some(old) = old {
        // SAFETY: the old icon isn't used any more.
        unsafe {
            let _ = DestroyIcon(old);
        }
    }
}

fn tray_menu(hwnd: HWND) {
    let c = core();
    let pause = if c.indexer.paused() { "Resume indexing" } else { "Pause indexing" };
    let update = c.update.lock().unwrap().as_ref().map(|u| format!("Install version {}", u.version));
    // Offered while the model is missing and no download runs; after a failed one it's a retry.
    let download = (!installer::is_installed() && !c.downloading()).then(|| {
        if c.download_error.lock().unwrap().is_some() {
            "Retry model download".to_string()
        } else {
            format!("Download search model ({})", installer::download_size_text())
        }
    });
    let mut items = vec![
        native::MenuItem::Bold(CMD_SEARCH, "Search"),
        native::MenuItem::Item(CMD_SETTINGS, "Settings…"),
        native::MenuItem::Separator,
        native::MenuItem::Item(CMD_PAUSE, pause),
        native::MenuItem::Item(CMD_RESCAN, "Rescan folders"),
    ];
    if let Some(d) = &download {
        items.push(native::MenuItem::Item(CMD_DOWNLOAD, d));
    }
    items.push(native::MenuItem::Item(CMD_DATA, "Open data folder"));
    if let Some(u) = &update {
        items.push(native::MenuItem::Item(CMD_UPDATE, u));
    }
    items.push(native::MenuItem::Separator);
    items.push(native::MenuItem::Item(CMD_QUIT, "Quit Nuiter"));
    match native::popup_menu(hwnd, window::cursor(), &items) {
        CMD_SEARCH => with_search(|s| s.show_at(None)),
        CMD_SETTINGS => show_settings(),
        CMD_PAUSE => {
            core().indexer.set_paused(!core().indexer.paused());
        }
        CMD_RESCAN => {
            core().indexer.rescan_in_background(false);
        }
        CMD_DOWNLOAD => download_runtime(),
        CMD_DATA => launcher::open_folder(&Paths::home().display().to_string()),
        CMD_UPDATE => install_update(),
        CMD_QUIT => quit(),
        _ => {}
    }
}

pub fn on_hotkey() {
    let b = taskbar::box_on_cursor_monitor();
    with_search(move |s| s.toggle(b));
}

pub fn on_taskbar_click(b: taskbar::SearchBox) {
    with_search(move |s| s.toggle(Some(b)));
}

pub fn show_settings() {
    let existing = UI.with(|u| u.borrow().as_ref().and_then(|u| u.settings.clone()));
    if let Some(s) = existing {
        if let Ok(w) = s.try_borrow() {
            native::force_foreground(w.hwnd());
        }
        return;
    }
    let w = SettingsWindow::create();
    UI.with(|u| {
        if let Some(u) = u.borrow_mut().as_mut() {
            u.settings = Some(w);
        }
    });
}

pub fn settings_open() -> bool {
    UI.with(|u| u.borrow().as_ref().is_some_and(|u| u.settings.is_some()))
}

/// The settings window closed.
pub fn settings_closed() {
    UI.with(|u| {
        if let Some(u) = u.borrow_mut().as_mut() {
            u.settings = None;
        }
    });
    let search_shown = UI.with(|u| u.borrow().as_ref().is_some_and(|u| u.search.try_borrow().map_or(true, |s| s.is_visible())));
    if !search_shown {
        crate::ui::gfx::release();
    }
    // What showing it took (text layouts, the folder estimates) goes back to Windows.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(1));
        nuiter_core::memory::relieve();
    });
}

/// Applies changed settings everywhere and saves them.
pub fn apply_settings(mut s: Settings) {
    // What the user saves is their choice now, even if it started from defaults standing in for a broken file.
    s.damaged = false;
    let c = core();
    let old = c.settings();
    if s.backend != old.backend {
        // Restart on the chosen device when next needed.
        c.worker.kill();
        c.index_worker.kill();
    }
    if let Err(e) = s.save() {
        Log::warn(format!("Cannot save settings: {e}"));
    }
    *c.settings.write().unwrap() = s.clone();
    c.indexer.update_settings(s.clone());
    hotkeys::set_win_s(s.intercept_win_s);
    hotkeys::set_hotkey(msg_window(), &s.hotkey);
    registry::apply_autostart(s.start_with_windows);
    taskbar::set_enabled(s.replace_taskbar_search);
    c.raise_status(true);
}

/// Asks whether to download the model now. "No" leaves search by words; Settings and the tray offer the download.
fn offer_download() {
    if installer::is_installed() || core().downloading() {
        return;
    }
    let metered = nuiter_core::net::metered();
    let text = format!(
        "Nuiter finds files by what they mean with EmbeddingGemma 2, a model that runs on this PC. It is downloaded \
         once: {} from Hugging Face.{}\n\nDownload it now?\n\nUntil then Nuiter finds files by their words. You can \
         download the model later in Settings or from the tray icon's menu.",
        installer::download_size_text(),
        if metered { "\n\nThis connection is metered: the download may use up paid data." } else { "" }
    );
    let owner = UI.with(|u| u.borrow().as_ref().and_then(|u| u.settings.as_ref().and_then(|s| s.try_borrow().ok().map(|s| s.hwnd()))))
        .unwrap_or_else(msg_window);
    if native::ask(owner, &text, !metered) {
        download_runtime();
    } else {
        Log::info(format!("Model download declined{}", if metered { " (metered connection)" } else { "" }));
        core().raise_status(true);
    }
}

pub fn download_runtime() {
    let c = core();
    {
        let mut d = c.download.lock().unwrap();
        if d.is_some() {
            return;
        }
        *d = Some(("Preparing download…".into(), 0.0));
    }
    *c.download_error.lock().unwrap() = None;
    c.raise_status(true);
    std::thread::spawn(|| {
        let c = core();
        let r = installer::install(
            &|msg, f| {
                *core().download.lock().unwrap() = Some((msg.to_string(), f));
                core().raise_status(false);
            },
            &Cancel::never(),
        );
        *c.download.lock().unwrap() = None;
        match r {
            Ok(()) => {
                Log::info("Runtime installed");
                c.worker.warm_up();
            }
            Err(e) => {
                Log::error(format!("Runtime download failed: {e:#}"));
                *c.download_error.lock().unwrap() = Some(format!("{e:#}"));
            }
        }
        c.raise_status(true);
    });
}

// ---------- updates ----------

/// Checks GitHub 2 minutes after the start, then once a day, while updates are on.
fn start_update_loop() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        loop {
            if core().settings().check_for_updates {
                check_for_updates();
            }
            std::thread::sleep(Duration::from_secs(24 * 3600));
        }
    });
}

/// Asks GitHub for a newer release; announces a new one once (a notification and a tray menu item). Blocks: call it
/// off the UI thread.
pub fn check_for_updates() {
    let c = core();
    match updates::check(&c.settings().update_repository) {
        Ok(u) => {
            *c.update_state.lock().unwrap() = if u.is_none() { Some(format!("Up to date (checked {})", &nuiter_core::win::local_time_text()[11..16])) } else { None };
            if let Some(u) = &u {
                let mut announced = c.announced.lock().unwrap();
                if *announced != Some(u.version) {
                    *announced = Some(u.version);
                    Log::info(format!("Update available: {}", u.version));
                    let text = format!("Version {} is available.", u.version);
                    window::invoke(move || tray::notify(msg_window(), &text, false));
                }
            }
            *c.update.lock().unwrap() = u;
        }
        Err(e) => {
            Log::warn(format!("Update check failed: {e}"));
            *c.update_state.lock().unwrap() = Some(format!("Couldn't check: {e}"));
        }
    }
    c.raise_status(true);
}

/// The files that couldn't be read and why, as a text file opened for reading ("unreadable files.txt" in the data folder,
/// written again each time).
pub fn show_unreadable() {
    let files = core().store.unreadable_files();
    let mut text = format!(
        "Files Nuiter couldn't read ({}). They're still found by their names.\r\nA file is tried again when it changes, \
         or with \"Try again\" in Settings > Indexing.\r\n\r\n",
        files.len()
    );
    for (path, why) in &files {
        text += &format!("{path}\r\n    {}\r\n\r\n", why.as_deref().unwrap_or("no reason recorded"));
    }
    let report = Paths::home().join("unreadable files.txt");
    match std::fs::write(&report, text) {
        Ok(()) => launcher::shell_open(&report.display().to_string(), &core().settings()),
        Err(e) => Log::warn(format!("Cannot write {}: {e}", report.display())),
    }
}

/// Downloads and checks the update, unpacks it beside this version, and restarts into it.
pub fn install_update() {
    let c = core();
    let Some(u) = c.update.lock().unwrap().clone() else { return };
    if c.update_state.lock().unwrap().as_deref().is_some_and(|s| s.starts_with("Downloading")) {
        return;
    }
    *c.update_state.lock().unwrap() = Some(format!("Downloading {}…", u.version));
    c.raise_status(true);
    std::thread::spawn(move || {
        let c = core();
        let progress = |p: f64| {
            *core().update_state.lock().unwrap() = Some(format!("Downloading {} · {}%", u.version, (p * 100.0).round()));
            core().raise_status(false);
        };
        match updates::install(&u, &Paths::exe_dir(), &progress, &Cancel::never()) {
            Ok(exe) => {
                let why = format!("updating to {}", u.version);
                window::invoke(move || restart_into(Some(exe.display().to_string()), &why));
            }
            Err(e) => {
                Log::error(format!("Update failed: {e:#}"));
                *c.update_state.lock().unwrap() = Some(format!("Update failed: {e:#}"));
                c.raise_status(true);
            }
        }
    });
}

fn shutdown_parts() {
    tray::remove(msg_window());
    taskbar::stop();
    hotkeys::set_win_s(false);
    let c = core();
    // Helpers and the model first: a worker waiting on one of them would otherwise hold up the join for minutes.
    c.indexer.cancel();
    if let Some(r) = &c.reader {
        r.shutdown();
    }
    c.worker.dispose();
    c.index_worker.dispose();
    c.indexer.stop();
}

/// Ends this process cleanly and starts `exe` (this one by default), which waits for it to be gone: for an update, or
/// for moving the index or the model.
pub fn restart_into(exe: Option<String>, why: &str) {
    Log::info(format!("Restarting: {why}"));
    shutdown_parts();
    let exe = exe.unwrap_or_else(|| std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
    if let Err(e) = std::process::Command::new(&exe).args(["--background", "--after", &std::process::id().to_string()]).spawn() {
        Log::error(format!("Restart failed: {e}"));
    }
    std::process::exit(0);
}

/// A file made reading run away and is marked as unreadable now. The thread stuck in it can't be stopped, so Nuiter
/// starts again in the background and this process ends.
fn restart(why: &str) {
    Log::warn(format!("Restarting: {why}"));
    tray::remove(msg_window());
    taskbar::stop();
    hotkeys::set_win_s(false);
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).args(["--background", "--after", &std::process::id().to_string()]).spawn();
    }
    std::process::exit(0);
}

pub fn quit() {
    Log::info("Quitting");
    shutdown_parts();
    std::process::exit(0);
}

/// Once nobody has searched for as long as the search model may idle (it stops then too), the vectors go (1.5 KB each;
/// the next search reads them from the index again).
fn start_release_timer() {
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(60));
            let c = core();
            let idle = c.settings().worker_idle_minutes;
            if idle <= 0 || !c.vectors.loaded() {
                continue;
            }
            let n = c.vectors.count();
            // Checked and released in one step: a search starting now keeps them.
            if !c.vectors.release_if_idle(Duration::from_secs(idle as u64 * 60)) {
                continue;
            }
            c.store.release_memory();
            nuiter_core::memory::relieve();
            Log::info(format!("No search for {idle} min: released {} vectors (private {} MB)", thousands(n as i64), nuiter_core::win::private_bytes() >> 20));
        }
    });
}

/// Gets search ready while the window opens: the query model, and the vectors when they aren't in memory.
pub fn warm_up() {
    let c = core();
    c.worker.warm_up();
    if !c.vectors.loaded() {
        let _ = std::thread::Builder::new().name("VectorsWarmUp".into()).spawn(|| {
            let c = core();
            c.vectors.ensure_loaded(|| c.store.vector_count(), |sink| c.store.load_vectors(sink));
        });
    }
}

