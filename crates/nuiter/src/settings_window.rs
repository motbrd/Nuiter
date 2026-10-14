//! Settings, laid out like Windows 11's Settings and PowerToys: pages on the left, a card per setting. Every change
//! applies at once; text fields apply when you leave them or press Enter, and turn red when they can't.
//!
//! One topic per page: how Nuiter opens (General), what it reads (Folders, Code), when and how it reads (Indexing),
//! the model it searches with (Search model), and the app itself (About: version, updates, privacy, its files).

use std::cell::RefCell;
use std::rc::Rc;

use nuiter_core::index::forecast::{self, Workload};
use nuiter_core::runtime::updates::{self, Version};
use nuiter_core::runtime::installer;
use nuiter_core::{Cancel, Paths, Settings, file_kinds, storage_move};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::app::{self, core};
use crate::shell::{hotkeys, launcher, registry};
use crate::ui::gfx::{Canvas, Color, Font, Rect, TextStyle};
use crate::ui::textbox::{Edit, TextBox};
use crate::ui::widgets::{self, Hits};
use crate::ui::{native, theme, window};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    General,
    Folders,
    Code,
    Indexing,
    Model,
    About,
}

const PAGES: [(Page, char, &str); 6] = [
    (Page::General, '\u{E713}', "General"),
    (Page::Folders, '\u{E8B7}', "Folders"),
    (Page::Code, '\u{E943}', "Code"),
    (Page::Indexing, '\u{E895}', "Indexing"),
    (Page::Model, '\u{E8F1}', "Search model"),
    (Page::About, '\u{E946}', "About"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Toggle {
    ReplaceTaskbar,
    WinS,
    Autostart,
    Images,
    Ocr,
    Audio,
    Video,
    EnergySaver,
    Battery,
    AtHours,
    CheckUpdates,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Hotkey,
    ExcludedExt,
    IndexFrom,
    IndexTo,
    MaxPdfPages,
    Idle,
    UpdateRepo,
}

const FIELDS: [Field; 7] =
    [Field::Hotkey, Field::ExcludedExt, Field::IndexFrom, Field::IndexTo, Field::MaxPdfPages, Field::Idle, Field::UpdateRepo];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Combo {
    Editor,
    Backend,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Btn {
    Download,
    ShowTaskbarBox,
    AddFolder,
    AddExcluded,
    AddCode,
    Pause,
    Rescan,
    OpenModels,
    MoveModels,
    OpenIndex,
    MoveIndex,
    OpenData,
    InstallUpdate,
    CheckNow,
    ShowUnreadable,
    RetryUnreadable,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum List {
    Folders,
    Code,
    Excluded,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Nav(Page),
    Toggle(Toggle),
    Field(Field),
    Combo(Combo),
    Button(Btn),
    Expand(List, usize),
    OpenRow(List, usize),
    Remove(List, usize),
    Sub(List, usize, usize),
    ScrollBar,
}

const EDITORS: [(&str, &str); 8] = [
    ("code", "VS Code"),
    ("cursor", "Cursor"),
    ("windsurf", "Windsurf"),
    ("zed", "Zed"),
    ("rider", "Rider"),
    ("sublime", "Sublime Text"),
    ("notepad++", "Notepad++"),
    ("", "The default app"),
];

const BACKENDS: [(&str, &str); 3] = [("auto", "Automatic"), ("gpu", "GPU"), ("cpu", "CPU")];

/// A folder in a list, with how much it holds and how long indexing it takes.
struct FolderRow {
    path: String,
    files: usize,
    detail: String,
    /// A searched folder (not a skipped one) can have its subfolders chosen.
    can_expand: bool,
    expanded: bool,
    subs: Vec<SubRow>,
    /// Looked for subfolders and found none: the row says so (once) and has nothing to expand.
    no_subfolders: bool,
}

/// A folder one level down: ticked, it's searched with its parent; unticked, it's skipped.
struct SubRow {
    path: String,
    name: String,
    included: bool,
    detail: String,
    seconds: f64,
}

struct FieldState {
    id: Field,
    text: TextBox,
    applied: String,
    error: bool,
}

pub struct SettingsWindow {
    hwnd: HWND,
    canvas: Option<Canvas>,
    dpi: f32,
    s: Settings,
    page: Page,
    scroll: f32,
    content_h: f32,
    /// The page's scroll bar: where it is (as last drawn) and, while the mouse holds it, the drag.
    bar_view: Rect,
    bar_drag: Option<widgets::ScrollDrag>,
    hits: Hits<Hit>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    fields: Vec<FieldState>,
    focus: Option<Field>,
    caret_on: bool,
    selecting: bool,
    folders: Vec<FolderRow>,
    code: Vec<FolderRow>,
    excluded: Vec<FolderRow>,
    first_run: bool,
    taskbar_hidden: bool,
    /// Indexing was running at the last status: when it stops, the folders' counts are taken again.
    was_busy: bool,
    closing: Cancel,
    icon: HICON,
}

const TIMER_CARET: usize = 1;

/// The navigation's width: 268 in the window's usual size, narrower in a narrow window so the pages keep room for a
/// card's title next to its control.
fn nav_width(window_w: f32) -> f32 {
    (window_w * 0.28).clamp(200.0, 268.0).round()
}

/// Space between groups: the page title and its first card, a section and the one before it, a banner and the page.
const SECTION_GAP: f32 = 20.0;
/// Space between cards in a group.
const CARD_GAP: f32 = 4.0;
/// A folder's row in a list, the line saying a list is empty, and a subfolder's row (with what's around them).
const ROW_H: f32 = 56.0;
const EMPTY_ROW_H: f32 = 44.0;
const SUB_ROW_H: f32 = 38.0;
const SUB_ROW_GAP: f32 = 6.0;
const SUB_ROWS_START: f32 = 2.0;
const SUB_ROWS_END: f32 = 6.0;

/// A folder's row with its subfolders when they're shown.
fn row_height(row: &FolderRow) -> f32 {
    if row.expanded { ROW_H + SUB_ROWS_START + row.subs.len() as f32 * (SUB_ROW_H + SUB_ROW_GAP) + SUB_ROWS_END } else { ROW_H }
}

impl SettingsWindow {
    pub fn create() -> Rc<RefCell<SettingsWindow>> {
        let class = w!("NuiterSettings");
        window::register_class(class, false);
        let s = core().settings();
        let first_run = s.is_first_run;
        let fields = FIELDS
            .iter()
            .map(|&id| {
                let value = field_value(&s, id);
                FieldState { id, text: TextBox::new(&value), applied: value, error: false }
            })
            .collect();
        let state = Rc::new(RefCell::new(SettingsWindow {
            hwnd: HWND::default(),
            canvas: None,
            dpi: 96.0,
            page: if first_run { Page::Folders } else { Page::General },
            s: s.clone(),
            scroll: 0.0,
            content_h: 0.0,
            bar_view: Rect::default(),
            bar_drag: None,
            hits: Hits::default(),
            hover: None,
            pressed: None,
            fields,
            focus: None,
            caret_on: true,
            selecting: false,
            folders: vec![],
            code: vec![],
            excluded: vec![],
            first_run,
            taskbar_hidden: registry::taskbar_search_mode() == 0,
            was_busy: false,
            closing: Cancel::new(),
            icon: HICON::default(),
        }));
        // Centred on the monitor the cursor is on.
        let work = window::work_area_at(window::cursor());
        let dpi = {
            let mut dx = 96u32;
            let mut dy = 96u32;
            // SAFETY: plain calls.
            unsafe {
                let m = windows::Win32::Graphics::Gdi::MonitorFromPoint(window::cursor(), windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST);
                let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(m, windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            }
            dx as f32
        };
        let scale = dpi / 96.0;
        let (w, h) = ((980.0 * scale) as i32, (700.0 * scale) as i32);
        let w = w.min(work.right - work.left);
        let h = h.min(work.bottom - work.top);
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + (work.bottom - work.top - h) / 2;
        let hwnd = window::create(
            class,
            "Nuiter Settings",
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            WINDOW_EX_STYLE(0),
            RECT { left: x, top: y, right: x + w, bottom: y + h },
            state.clone(),
        );
        {
            let mut st = state.borrow_mut();
            st.hwnd = hwnd;
            st.dpi = window::dpi_of(hwnd);
            let s = st.s.clone();
            for f in &s.folders {
                st.add_row(List::Folders, f);
            }
            for f in &s.code_folders {
                st.add_row(List::Code, f);
            }
            for f in &s.excluded_paths {
                st.add_row(List::Excluded, f);
            }
            st.set_icon();
        }
        theme::apply_backdrop(hwnd, theme::MICA);
        native::show_cloaked(hwnd, SW_SHOW);
        native::force_foreground(hwnd);
        // SAFETY: the caret's blink.
        unsafe { SetTimer(Some(hwnd), TIMER_CARET, 530, None) };
        state
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    fn set_icon(&mut self) {
        let ico = if theme::current().dark { native::ICON_LIGHT } else { native::ICON_DARK };
        let big = native::icon_from_ico(ico, 32).unwrap_or_default();
        let small = native::icon_from_ico(ico, 16).unwrap_or_default();
        // SAFETY: the window keeps the icons; the old one is destroyed.
        unsafe {
            SendMessageW(self.hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));
            SendMessageW(self.hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
            if !self.icon.0.is_null() {
                let _ = DestroyIcon(self.icon);
            }
        }
        self.icon = big;
    }

    pub fn theme_changed(&mut self) {
        theme::apply_backdrop(self.hwnd, theme::MICA);
        self.set_icon();
        self.invalidate();
    }

    pub fn status_changed(&mut self) {
        use nuiter_core::index::indexer::IndexPhase;
        let busy = matches!(core().indexer.status().phase, IndexPhase::Scanning | IndexPhase::Indexing);
        if self.was_busy && !busy {
            for l in [List::Folders, List::Code] {
                let paths: Vec<String> = self.list(l).iter().map(|r| r.path.clone()).collect();
                for p in paths {
                    self.estimate_row(l, p);
                }
            }
        }
        self.was_busy = busy;
        self.invalidate();
    }

    fn invalidate(&self) {
        window::invalidate(self.hwnd);
    }

    fn scale(&self) -> f32 {
        self.dpi / 96.0
    }

    // ---------- applying ----------

    fn apply(&mut self) {
        self.s.folders = self.folders.iter().map(|r| r.path.clone()).collect();
        self.s.code_folders = self.code.iter().map(|r| r.path.clone()).collect();
        self.s.excluded_paths = self.excluded.iter().map(|r| r.path.clone()).collect();
        app::apply_settings(self.s.clone());
        self.invalidate();
    }

    fn toggle_value(&self, t: Toggle) -> bool {
        let s = &self.s;
        match t {
            Toggle::ReplaceTaskbar => s.replace_taskbar_search,
            Toggle::WinS => s.intercept_win_s,
            Toggle::Autostart => s.start_with_windows,
            Toggle::Images => s.index_images,
            Toggle::Ocr => s.ocr_images,
            Toggle::Audio => s.index_audio,
            Toggle::Video => s.index_video,
            Toggle::EnergySaver => s.pause_on_energy_saver,
            Toggle::Battery => s.pause_on_battery,
            Toggle::AtHours => s.index_only_at_hours,
            Toggle::CheckUpdates => s.check_for_updates,
        }
    }

    fn flip(&mut self, t: Toggle) {
        let v = !self.toggle_value(t);
        let s = &mut self.s;
        match t {
            Toggle::ReplaceTaskbar => s.replace_taskbar_search = v,
            Toggle::WinS => s.intercept_win_s = v,
            Toggle::Autostart => s.start_with_windows = v,
            Toggle::Images => s.index_images = v,
            Toggle::Ocr => s.ocr_images = v,
            Toggle::Audio => s.index_audio = v,
            Toggle::Video => s.index_video = v,
            Toggle::EnergySaver => s.pause_on_energy_saver = v,
            Toggle::Battery => s.pause_on_battery = v,
            Toggle::AtHours => s.index_only_at_hours = v,
            Toggle::CheckUpdates => s.check_for_updates = v,
        }
        self.apply();
    }

    fn field(&mut self, id: Field) -> &mut FieldState {
        self.fields.iter_mut().find(|f| f.id == id).unwrap()
    }

    /// A field still being edited is applied when it's left (or the window closes), if what's in it makes sense.
    fn commit(&mut self, id: Field) {
        let f = self.field(id);
        let text = f.text.value().trim().to_string();
        if !field_valid(id, &text) {
            f.error = true;
            self.invalidate();
            return;
        }
        f.error = false;
        if text == f.applied {
            return;
        }
        f.applied = text.clone();
        let s = &mut self.s;
        match id {
            Field::Hotkey => s.hotkey = text,
            Field::ExcludedExt => s.excluded_extensions = parse_extensions(&text),
            Field::IndexFrom => s.index_from = text,
            Field::IndexTo => s.index_to = text,
            Field::Idle => s.worker_idle_minutes = text.parse().unwrap_or(10),
            Field::MaxPdfPages => s.max_pdf_pages = text.parse().unwrap_or(nuiter_core::extract::pdf::MAX_PAGES as i32),
            Field::UpdateRepo => s.update_repository = text,
        }
        self.apply();
    }

    fn set_focus(&mut self, f: Option<Field>) {
        if let Some(old) = self.focus
            && Some(old) != f
        {
            self.commit(old);
        }
        self.focus = f;
        self.caret_on = true;
        self.invalidate();
    }

    // ---------- folder lists ----------

    fn list(&mut self, l: List) -> &mut Vec<FolderRow> {
        match l {
            List::Folders => &mut self.folders,
            List::Code => &mut self.code,
            List::Excluded => &mut self.excluded,
        }
    }

    fn add_row(&mut self, l: List, path: &str) -> bool {
        if self.list(l).iter().any(|r| nuiter_core::uni::eq_ignore_case(&r.path, path)) {
            return false;
        }
        let estimate = l != List::Excluded;
        self.list(l).push(FolderRow {
            path: path.into(),
            files: 0,
            detail: if estimate { "Counting…".into() } else { "Not read".into() },
            can_expand: estimate,
            expanded: false,
            subs: vec![],
            no_subfolders: false,
        });
        if estimate {
            self.estimate_row(l, path.to_string());
        }
        true
    }

    /// Counts a folder's files and estimates their indexing time: metadata only (counts, PDF page counts, media
    /// durations), off the UI thread.
    fn estimate_row(&mut self, l: List, p: String) {
        let (cancel, code) = (self.closing.clone(), l == List::Code);
        std::thread::spawn(move || {
            nuiter_core::win::init_com_mta();
            let Ok(e) = core().indexer.estimate_folder(&p, code, &cancel) else { return };
            let (detail, files) = (describe(&e, code), e.files);
            window::invoke(move || {
                app::with_settings(move |w| {
                    if let Some(row) = w.list(l).iter_mut().find(|r| r.path == p) {
                        row.files = files;
                        row.detail = detail;
                    }
                    w.invalidate();
                })
            });
        });
    }

    /// Shows or hides a folder's subfolders; the first time, lists them (biggest first once counted).
    fn expand(&mut self, l: List, i: usize) {
        let excluded: Vec<String> = self.excluded.iter().map(|r| r.path.clone()).collect();
        let code = l == List::Code;
        let row = &mut self.list(l)[i];
        row.expanded = !row.expanded;
        if !row.expanded || !row.subs.is_empty() {
            self.invalidate();
            return;
        }
        let mut dirs: Vec<String> = match std::fs::read_dir(&row.path) {
            Ok(rd) => rd
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let attrs = e.metadata().map(|m| std::os::windows::fs::MetadataExt::file_attributes(&m)).unwrap_or(0);
                    !file_kinds::should_skip_directory(&name, code) && attrs & (0x2 | 0x4 | 0x400) == 0
                })
                .map(|e| e.path().display().to_string())
                .collect(),
            Err(e) => {
                row.expanded = false;
                row.detail = format!("Can't list its folders: {e}");
                self.invalidate();
                return;
            }
        };
        dirs.sort_by_key(|d| d.to_lowercase());
        if dirs.is_empty() {
            // Said once, by the row itself: appended to the detail, every click added another " · no subfolders".
            row.expanded = false;
            row.no_subfolders = true;
            self.invalidate();
            return;
        }
        row.subs = dirs
            .iter()
            .map(|d| SubRow {
                path: d.clone(),
                name: file_kinds::pathx::file_name(d).to_string(),
                included: !excluded.iter().any(|x| nuiter_core::uni::eq_ignore_case(x, d)),
                detail: "Counting…".into(),
                seconds: 0.0,
            })
            .collect();
        let parent = row.path.clone();
        let cancel = self.closing.clone();
        std::thread::spawn(move || {
            nuiter_core::win::init_com_mta();
            for d in dirs {
                if cancel.is_cancelled() {
                    return;
                }
                let Ok(e) = core().indexer.estimate_folder(&d, code, &cancel) else { return };
                let (detail, secs, parent) = (describe(&e, code), e.seconds, parent.clone());
                window::invoke(move || {
                    app::with_settings(move |w| {
                        if let Some(row) = w.list(l).iter_mut().find(|r| r.path == parent)
                            && let Some(sub) = row.subs.iter_mut().find(|s| s.path == d)
                        {
                            sub.detail = detail;
                            sub.seconds = secs;
                        }
                        w.invalidate();
                    })
                });
            }
            // Biggest first, once all are counted.
            window::invoke(move || {
                app::with_settings(move |w| {
                    if let Some(row) = w.list(l).iter_mut().find(|r| r.path == parent) {
                        row.subs.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
                    }
                    w.invalidate();
                })
            });
        });
        self.invalidate();
    }

    /// A subfolder ticked or unticked: it leaves or joins the skipped folders, and indexing follows.
    fn flip_sub(&mut self, l: List, i: usize, j: usize) {
        let sub = &mut self.list(l)[i].subs[j];
        sub.included = !sub.included;
        let (path, included) = (sub.path.clone(), sub.included);
        let existing = self.excluded.iter().position(|x| nuiter_core::uni::eq_ignore_case(&x.path, &path));
        match (included, existing) {
            (true, Some(k)) => {
                self.excluded.remove(k);
            }
            (false, None) => {
                self.add_row(List::Excluded, &path);
            }
            _ => {}
        }
        self.apply();
    }

    fn remove_row(&mut self, l: List, i: usize) {
        if l != List::Excluded {
            let row = &self.list(l)[i];
            let what = if row.files > 0 { format!("Its {} files leave the index.", app::thousands(row.files as i64)) } else { "Its files leave the index.".into() };
            let text = format!("Stop searching {}?\n\n{what}", row.path);
            if !native::message_box(self.hwnd, &text, true) {
                return;
            }
        }
        self.list(l).remove(i);
        self.apply();
    }

    fn pick_and_add(&mut self, l: List) {
        let title = match l {
            List::Folders => "Choose a folder to search",
            List::Code => "Choose a folder with code",
            List::Excluded => "Choose a folder to skip",
        };
        if let Some(p) = native::pick_folder(self.hwnd, title)
            && self.add_row(l, &p)
        {
            self.apply();
        }
    }

    // ---------- buttons ----------

    fn button(&mut self, b: Btn) {
        match b {
            Btn::Download => app::download_runtime(),
            Btn::ShowTaskbarBox => {
                registry::set_taskbar_search_mode(2);
                self.taskbar_hidden = false;
            }
            Btn::AddFolder => self.pick_and_add(List::Folders),
            Btn::AddCode => self.pick_and_add(List::Code),
            Btn::AddExcluded => self.pick_and_add(List::Excluded),
            Btn::Pause => core().indexer.set_paused(!core().indexer.paused()),
            Btn::Rescan => core().indexer.rescan_in_background(false),
            Btn::ShowUnreadable => app::show_unreadable(),
            Btn::RetryUnreadable => core().indexer.rescan_in_background(true),
            Btn::OpenData => launcher::open_folder(&Paths::home().display().to_string()),
            Btn::OpenIndex => launcher::open_folder(&Paths::index_dir().display().to_string()),
            Btn::OpenModels => {
                let _ = std::fs::create_dir_all(Paths::models_dir());
                launcher::open_folder(&Paths::models_dir().display().to_string());
            }
            Btn::MoveIndex => self.move_files("the index", &Paths::index_dir(), storage_move::INDEX_FILES, index_bytes(), true),
            Btn::MoveModels => {
                if core().downloading() {
                    native::message_box(self.hwnd, "Wait for the download to finish.", false);
                    return;
                }
                let bytes: f64 = storage_move::MODEL_FILES.iter().filter_map(|f| std::fs::metadata(Paths::models_dir().join(f)).ok()).map(|m| m.len() as f64).sum();
                self.move_files("the model", &Paths::models_dir(), storage_move::MODEL_FILES, bytes, false);
            }
            Btn::CheckNow => {
                std::thread::spawn(app::check_for_updates);
            }
            Btn::InstallUpdate => app::install_update(),
        }
        self.invalidate();
    }

    /// A new folder for the index or the model: Nuiter restarts and moves the files before opening them.
    fn move_files(&mut self, what: &str, current: &std::path::Path, files: &[&str], bytes: f64, index: bool) {
        let Some(to) = native::pick_folder(self.hwnd, &format!("Choose a folder for {what}")) else { return };
        let problem = if index { storage_move::index_problem(&to, current) } else { storage_move::problem(&to, current, files) };
        if let Some(why) = problem {
            native::message_box(self.hwnd, &why, false);
            return;
        }
        let text = format!("Move {what} ({}) to {to}?\n\nNuiter will restart.", forecast::size_text(bytes));
        if !native::message_box(self.hwnd, &text, true) {
            return;
        }
        if index {
            self.s.move_index_to = Some(to.clone());
        } else {
            self.s.move_models_to = Some(to.clone());
        }
        self.apply();
        let why = format!("moving {what} to {to}");
        window::later(move || app::restart_into(None, &why));
    }

    fn combo(&mut self, c: Combo) {
        let Some(r) = self.hits.rect_of(Hit::Combo(c)) else { return };
        let s = self.scale();
        let mut pt = windows::Win32::Foundation::POINT { x: (r.x * s) as i32, y: (r.bottom() * s) as i32 };
        // SAFETY: plain call.
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::ClientToScreen(self.hwnd, &mut pt);
        }
        let choices: Vec<(String, String)> = match c {
            Combo::Editor => {
                let mut v: Vec<(String, String)> = EDITORS.iter().map(|(id, n)| (id.to_string(), n.to_string())).collect();
                if !EDITORS.iter().any(|(id, _)| *id == self.s.code_editor) {
                    v.insert(0, (self.s.code_editor.clone(), self.s.code_editor.clone()));
                }
                v
            }
            Combo::Backend => BACKENDS.iter().map(|(id, n)| (id.to_string(), n.to_string())).collect(),
        };
        let items: Vec<native::MenuItem<'_>> = choices.iter().enumerate().map(|(i, (_, n))| native::MenuItem::Item(i as u32 + 1, n)).collect();
        let cmd = native::popup_menu(self.hwnd, pt, &items);
        if cmd == 0 {
            return;
        }
        let id = choices[cmd as usize - 1].0.clone();
        match c {
            Combo::Editor => self.s.code_editor = id,
            Combo::Backend => self.s.backend = id,
        }
        self.apply();
    }

    // ---------- drawing ----------

    fn paint(&mut self) {
        if self.canvas.is_none() {
            self.canvas = Canvas::new(self.hwnd, self.dpi).ok();
        }
        let Some(c) = self.canvas.take() else { return };
        let t = theme::current();
        self.hits.clear();
        c.begin(if t.backdrop { Color::TRANSPARENT } else { t.window_bg });
        self.draw(&c, &t);
        if c.end() {
            self.canvas = Some(c);
        }
    }

    fn size(&self) -> (f32, f32) {
        let (w, h) = window::client_size(self.hwnd);
        (w as f32 / self.scale(), h as f32 / self.scale())
    }

    fn draw(&mut self, c: &Canvas, t: &theme::Theme) {
        // The hit map is filled while drawing; taken out of self so the page builders can borrow both.
        let mut hits = std::mem::take(&mut self.hits);
        let (w, h) = self.size();
        let nav_w = nav_width(w);
        // Navigation.
        let mut y = 14.0;
        for (page, glyph, label) in PAGES {
            let r = Rect::new(12.0, y, nav_w - 20.0, 38.0);
            // The open page: its background only, no accent bar on its left edge (the user's choice).
            if self.page == page {
                c.round(r, widgets::RADIUS, t.selected);
            } else if self.hover == Some(Hit::Nav(page)) {
                c.round(r, widgets::RADIUS, t.hover);
            }
            c.icon(Rect::new(r.x + 14.0, r.y, 16.0, r.h), glyph, 16.0, t.text);
            c.text(Rect::new(r.x + 44.0, r.y, r.w - 44.0 - 28.0, r.h), label, &TextStyle::new(14.0, t.text).middle());
            // The page that needs something: the model isn't downloaded.
            if page == Page::Model && !installer::is_installed() {
                c.ellipse(r.right() - 16.0, r.y + r.h / 2.0, 4.0, 4.0, t.accent);
            }
            hits.add(r, Hit::Nav(page));
            y += 41.0;
        }
        // The live status, always in view: as tall as its text, the dot on its first line.
        let (status, dot) = core().status_line();
        let style = TextStyle::new(12.0, t.text2).wrap();
        let text_w = nav_w - 20.0 - 30.0 - 12.0;
        let line_h = crate::ui::gfx::with(|g| g.line_height(&style));
        let text_h = crate::ui::gfx::with(|g| g.measure(&status, &style, text_w)).1.max(line_h);
        let sr = Rect::new(12.0, h - 12.0 - (text_h + 24.0), nav_w - 20.0, text_h + 24.0);
        c.round(sr, widgets::CARD_RADIUS, t.panel_bg);
        c.stroke_round(sr, widgets::CARD_RADIUS, t.stroke, 1.0);
        c.ellipse(sr.x + 12.0 + 4.0, sr.y + 12.0 + line_h / 2.0, 4.0, 4.0, dot);
        c.text(Rect::new(sr.x + 30.0, sr.y + 12.0, text_w, text_h + 2.0), &status, &style);

        // The page.
        let area = Rect::new(nav_w, 0.0, w - nav_w, h);
        c.clip(area);
        let pw = (area.w - 28.0 - 30.0).min(1000.0);
        let x = area.x + 28.0;
        let mut y = 22.0 - self.scroll;
        let mut p = PageDraw { c, t, x, w: pw, hover: self.hover, hits: &mut hits };
        let title = PAGES.iter().find(|pg| pg.0 == self.page).map(|pg| pg.2).unwrap_or("");
        y = page_title(&mut p, y, title);
        // The model's download, where it's looked for: the start, the model's own page, and the first run's folders.
        if !installer::is_installed() && (matches!(self.page, Page::General | Page::Model) || (self.first_run && self.page == Page::Folders)) {
            y = draw_download_card(&mut p, y);
        }
        y = match self.page {
            Page::General => self.draw_general(&mut p, y),
            Page::Folders => self.draw_folders(&mut p, y),
            Page::Code => self.draw_code(&mut p, y),
            Page::Indexing => self.draw_indexing(&mut p, y),
            Page::Model => self.draw_model(&mut p, y),
            Page::About => self.draw_about(&mut p, y),
        };
        self.content_h = y + self.scroll + 24.0;
        c.unclip();
        self.bar_view = Rect::new(area.x, 0.0, area.w - 2.0, h);
        let hot = self.hover == Some(Hit::ScrollBar) || self.bar_drag.is_some();
        if let Some(bar) = widgets::scroll_bar(c, t, self.bar_view, self.content_h, self.scroll, hot) {
            hits.add(bar, Hit::ScrollBar);
        }
        self.hits = hits;
    }

    fn draw_field(&mut self, p: &mut PageDraw<'_>, r: Rect, id: Field) {
        let focused = self.focus == Some(id);
        let caret = self.caret_on;
        let (c, t) = (p.c, p.t);
        let f = self.fields.iter_mut().find(|f| f.id == id).unwrap();
        widgets::field_frame(c, t, r, focused, f.error);
        f.text.draw(c, r.inset(9.0, 0.0, 9.0, 0.0), &TextStyle::new(14.0, t.text), Self::field_placeholder(id), t.text3, t.accent.with_alpha(0.45), focused, caret);
        p.hits.add(r, Hit::Field(id));
    }

    fn field_error(&self, id: Field) -> Option<&'static str> {
        let f = self.fields.iter().find(|f| f.id == id)?;
        f.error.then_some(match id {
            Field::Hotkey => "Not a shortcut, e.g. Win+Alt+S",
            Field::UpdateRepo => "Use owner/name",
            Field::IndexFrom | Field::IndexTo => "Use HH:MM, e.g. 01:00",
            Field::Idle => "0 or more minutes",
            Field::MaxPdfPages => "1 to 100,000",
            Field::ExcludedExt => "",
        })
    }

    /// What an empty field shows.
    fn field_placeholder(id: Field) -> &'static str {
        match id {
            Field::Hotkey => "Win+Alt+S",
            Field::ExcludedExt => ".log, .csv",
            Field::UpdateRepo => "owner/name",
            Field::IndexFrom => "01:00",
            Field::IndexTo => "07:00",
            Field::Idle | Field::MaxPdfPages => "",
        }
    }

    fn draw_general(&mut self, p: &mut PageDraw<'_>, mut y: f32) -> f32 {
        y = section(p, y, "Open Nuiter with", true);
        y = toggle_card(p, y, '\u{E721}', "Taskbar search box", None, Toggle::ReplaceTaskbar, self.s.replace_taskbar_search);
        if self.taskbar_hidden {
            y = buttons_card(p, y, '\u{E7BA}', "The search box is hidden on the taskbar", None, &[(Btn::ShowTaskbarBox, "Show it", false)]);
        }
        y = toggle_card(p, y, '\u{E765}', "Win+S", None, Toggle::WinS, self.s.intercept_win_s);
        let (y2, ctl) = card_error(p, y, Some('\u{E765}'), "Shortcut", self.field_error(Field::Hotkey), 180.0, 32.0);
        self.draw_field(p, ctl, Field::Hotkey);
        y = section(p, y2, "Startup", false);
        toggle_card(p, y, '\u{E7E8}', "Start with Windows", None, Toggle::Autostart, self.s.start_with_windows)
    }

    fn draw_folder_list(&mut self, p: &mut PageDraw<'_>, mut y: f32, l: List, header: (char, &str, Btn, &str, bool), empty: Option<&str>) -> f32 {
        let (c, t, x, w) = (p.c, p.t, p.x, p.w);
        let top = y;
        // The header row of a list card: the list's frame goes round it and the rows.
        let (icon, title, btn, label, accent) = header;
        let bw = widgets::button_width(label);
        let (h, ctl) = card_layout(p, y, Some(icon), title, None, bw, widgets::BUTTON_HEIGHT, false);
        let rows = match l {
            List::Folders => &self.folders,
            List::Code => &self.code,
            List::Excluded => &self.excluded,
        };
        // The header and the rows are filled edge to edge, cut to the frame's rounded corners (square fills poked
        // out of them, and the header's own rounded bottom left notches where the rows began).
        let empty_h = if rows.is_empty() && empty.is_some() { EMPTY_ROW_H } else { 0.0 };
        let frame = Rect::new(x, top, w, h + rows.iter().map(row_height).sum::<f32>() + empty_h);
        c.push_round_clip(frame, widgets::CARD_RADIUS);
        c.fill(Rect::new(x, top, w, h), t.panel_bg);
        draw_card_text(p, y, h, Some(icon), title, None, bw, false);
        if accent {
            widgets::accent_button(c, t, ctl, label, self.hover == Some(Hit::Button(btn)));
        } else {
            widgets::subtle_button(c, t, ctl, label, self.hover == Some(Hit::Button(btn)), true);
        }
        p.hits.add(ctl, Hit::Button(btn));
        y += h;
        for (i, row) in rows.iter().enumerate() {
            let row_top = y;
            // The row's text starts where the cards' titles do; its buttons end where the cards' controls do.
            let inner = Rect::new(x + 56.0, y, w - 56.0 - 16.0, ROW_H);
            let expandable = row.can_expand && !row.no_subfolders;
            let icons_w = if expandable { 3.0 * 32.0 } else { 2.0 * 32.0 };
            let text_w = inner.w - icons_w - 12.0;
            c.fill(Rect::new(x, row_top, w, row_height(row)), t.panel_bg.with_alpha(0.6));
            c.fill(Rect::new(x + 1.0, y, w - 2.0, 1.0), t.stroke);
            let detail = if row.no_subfolders { format!("{} · no subfolders", row.detail) } else { row.detail.clone() };
            two_lines(c, Rect::new(inner.x, inner.y, text_w, ROW_H), &row.path, &detail, t);
            let mut bx = inner.right() - icons_w;
            let by = inner.y + (ROW_H - 32.0) / 2.0;
            if expandable {
                let r = Rect::new(bx, by, 32.0, 32.0);
                widgets::icon_button(c, t, r, if row.expanded { '\u{E70E}' } else { '\u{E70D}' }, self.hover == Some(Hit::Expand(l, i)), t.text);
                p.hits.add(r, Hit::Expand(l, i));
                bx += 32.0;
            }
            let r = Rect::new(bx, by, 32.0, 32.0);
            widgets::icon_button(c, t, r, '\u{E838}', self.hover == Some(Hit::OpenRow(l, i)), t.text);
            p.hits.add(r, Hit::OpenRow(l, i));
            let r = Rect::new(bx + 32.0, by, 32.0, 32.0);
            widgets::icon_button(c, t, r, '\u{E74D}', self.hover == Some(Hit::Remove(l, i)), t.text);
            p.hits.add(r, Hit::Remove(l, i));
            y += ROW_H;
            if row.expanded {
                // One level down, biggest first: unticked ones are skipped.
                y += SUB_ROWS_START;
                for (j, sub) in row.subs.iter().enumerate() {
                    let r = Rect::new(inner.x, y, inner.w, SUB_ROW_H);
                    let hot = self.hover == Some(Hit::Sub(l, i, j));
                    widgets::check_box(c, t, Rect::new(r.x, r.y, 20.0, r.h), sub.included, hot);
                    two_lines(c, Rect::new(r.x + 28.0, r.y, r.w - 28.0, r.h), &sub.name, &sub.detail, t);
                    p.hits.add(Rect::new(r.x, r.y, 300f32.min(r.w), r.h), Hit::Sub(l, i, j));
                    y += SUB_ROW_H + SUB_ROW_GAP;
                }
                y += SUB_ROWS_END;
            }
        }
        if rows.is_empty()
            && let Some(e) = empty
        {
            // A row of its own, like the folders that will be listed there.
            c.fill(Rect::new(x, y, w, EMPTY_ROW_H), t.panel_bg.with_alpha(0.6));
            c.fill(Rect::new(x + 1.0, y, w - 2.0, 1.0), t.stroke);
            c.text(Rect::new(x + 56.0, y, w - 56.0 - 16.0, EMPTY_ROW_H), e, &TextStyle::new(12.0, t.text2).middle());
            y += EMPTY_ROW_H;
        }
        c.pop_layer();
        c.stroke_round(frame, widgets::CARD_RADIUS, t.stroke, 1.0);
        y + CARD_GAP
    }

    fn draw_folders(&mut self, p: &mut PageDraw<'_>, mut y: f32) -> f32 {
        if self.first_run {
            // First start: one line on what to do.
            y = card(p, y, None, "Welcome! Add the folders you want to search.", None, 0.0, 0.0).0;
        }
        y = self.draw_folder_list(p, y, List::Folders, ('\u{E8B7}', "Folders to search", Btn::AddFolder, "Add folder", true), Some("No folders yet"));
        y = section(p, y, "File types", false);
        y = toggle_card(p, y, '\u{EB9F}', "Pictures", None, Toggle::Images, self.s.index_images);
        y = toggle_card(p, y, '\u{E8D2}', "Text in pictures (OCR)", None, Toggle::Ocr, self.s.ocr_images);
        y = toggle_card(p, y, '\u{E8D6}', "Audio", None, Toggle::Audio, self.s.index_audio);
        y = toggle_card(p, y, '\u{E714}', "Video", None, Toggle::Video, self.s.index_video);
        y = section(p, y, "Exclusions", false);
        y = self.draw_folder_list(p, y, List::Excluded, ('\u{E733}', "Skipped folders", Btn::AddExcluded, "Add", false), None);
        let (y2, ctl) = card(p, y, Some('\u{E8A5}'), "Skipped file types", None, 220.0, 32.0);
        self.draw_field(p, ctl, Field::ExcludedExt);
        y2
    }

    fn draw_code(&mut self, p: &mut PageDraw<'_>, mut y: f32) -> f32 {
        y = self.draw_folder_list(p, y, List::Code, ('\u{E943}', "Code folders", Btn::AddCode, "Add folder", true), Some("No code folders yet"));
        let label = EDITORS.iter().find(|e| e.0 == self.s.code_editor).map(|e| e.1.to_string()).unwrap_or_else(|| self.s.code_editor.clone());
        combo_card(p, y, '\u{E70F}', "Open code in", None, Combo::Editor, &label, 180.0)
    }

    fn draw_indexing(&mut self, p: &mut PageDraw<'_>, mut y: f32) -> f32 {
        use nuiter_core::index::indexer::IndexPhase;
        let (c, t) = (p.c, p.t);
        let ix = &core().indexer;
        let st = ix.status();
        let paused = ix.paused();
        let title = match st.phase {
            IndexPhase::Idle => "Up to date".to_string(),
            IndexPhase::Scanning => "Scanning folders".to_string(),
            IndexPhase::Indexing => format!("Indexing · {} left", app::thousands(st.queued as i64)),
            IndexPhase::Paused if paused => "Paused".to_string(),
            IndexPhase::Paused => "Waiting".to_string(),
            IndexPhase::WaitingForRuntime => "Waiting for the search model".to_string(),
        };
        let mut desc = format!("{} files · {} items", app::thousands(st.files), app::thousands(st.chunks));
        match st.phase {
            IndexPhase::Indexing if st.current.is_some() => {
                desc += &format!(" · {}", file_kinds::pathx::file_name(st.current.as_deref().unwrap_or("")));
            }
            IndexPhase::Paused if !paused => {
                if let Some(why) = ix.wait_reason() {
                    desc += &format!(" · {why}");
                }
            }
            _ => {}
        }
        let pause = if paused { "Resume" } else { "Pause" };
        y = buttons_card(p, y, '\u{E895}', &title, Some(&desc), &[(Btn::Pause, pause, false), (Btn::Rescan, "Rescan", false)]);
        if st.errors > 0 {
            let title = format!("{} {} couldn't be read", app::thousands(st.errors), if st.errors == 1 { "file" } else { "files" });
            y = buttons_card(p, y, '\u{E7BA}', &title, None, &[(Btn::ShowUnreadable, "Show", false), (Btn::RetryUnreadable, "Retry", false)]);
        }
        y = section(p, y, "Schedule", false);
        y = toggle_card(p, y, '\u{EC48}', "Pause in Energy Saver", None, Toggle::EnergySaver, self.s.pause_on_energy_saver);
        y = toggle_card(p, y, '\u{E83F}', "Pause on battery", None, Toggle::Battery, self.s.pause_on_battery);
        let err = self.field_error(Field::IndexFrom).or(self.field_error(Field::IndexTo));
        let (y2, ctl) = card_error(p, y, Some('\u{E823}'), "Only between", err, 66.0 + 30.0 + 66.0 + 18.0 + widgets::TOGGLE_WIDTH, 32.0);
        self.draw_field(p, Rect::new(ctl.x, ctl.y, 66.0, 32.0), Field::IndexFrom);
        c.text(Rect::new(ctl.x + 66.0, ctl.y, 30.0, 32.0), "and", &TextStyle::new(14.0, t.text2).center().middle());
        self.draw_field(p, Rect::new(ctl.x + 96.0, ctl.y, 66.0, 32.0), Field::IndexTo);
        let tr = Rect::new(ctl.x + 96.0 + 66.0 + 18.0, ctl.y, widgets::TOGGLE_WIDTH, 32.0);
        widgets::toggle(c, t, tr, self.s.index_only_at_hours, self.hover == Some(Hit::Toggle(Toggle::AtHours)), true);
        p.hits.add(tr, Hit::Toggle(Toggle::AtHours));
        y = section(p, y2, "Storage", false);
        let (y2, ctl) = card_error(p, y, Some('\u{EA90}'), "Max PDF pages", self.field_error(Field::MaxPdfPages), 90.0, 32.0);
        self.draw_field(p, ctl, Field::MaxPdfPages);
        let index_desc = format!("{} · {}", forecast::size_text(index_bytes()), Paths::index_dir().display());
        buttons_card(p, y2, '\u{E7B8}', "Index", Some(&index_desc), &[(Btn::OpenIndex, "Open", false), (Btn::MoveIndex, "Move…", false)])
    }

    fn draw_model(&mut self, p: &mut PageDraw<'_>, mut y: f32) -> f32 {
        let (c, t) = (p.c, p.t);
        let installed = installer::is_installed();
        let device = [core().worker.device(), core().index_worker.device()].into_iter().find(|d| !d.is_empty());
        let state = match (installed, device) {
            (false, _) => "Not downloaded".to_string(),
            (true, Some(d)) => format!("Downloaded · running on {d}"),
            (true, None) => "Downloaded".to_string(),
        };
        y = text_card(p, y, '\u{E8F1}', "EmbeddingGemma 2", &state);
        let backend = BACKENDS.iter().find(|b| b.0 == self.s.backend).map(|b| b.1).unwrap_or(BACKENDS[0].1).to_string();
        y = combo_card(p, y, '\u{E7F4}', "Run on", None, Combo::Backend, &backend, 220.0);
        // The field, then its unit, ending where the other cards' controls end.
        let unit = TextStyle::new(14.0, t.text2).middle();
        let unit_w = crate::ui::gfx::with(|g| g.measure("min", &unit, 100.0)).0.ceil();
        let (y2, ctl) = card_error(p, y, Some('\u{E708}'), "Unload when idle", self.field_error(Field::Idle), 66.0 + 8.0 + unit_w, 32.0);
        self.draw_field(p, Rect::new(ctl.x, ctl.y, 66.0, 32.0), Field::Idle);
        c.text(Rect::new(ctl.x + 66.0 + 8.0, ctl.y, unit_w + 2.0, 32.0), "min", &unit);
        let bytes: f64 = storage_move::MODEL_FILES.iter().filter_map(|f| std::fs::metadata(Paths::models_dir().join(f)).ok()).map(|m| m.len() as f64).sum();
        let files_desc = if bytes > 0.0 {
            format!("{} · {}", forecast::size_text(bytes), Paths::models_dir().display())
        } else {
            Paths::models_dir().display().to_string()
        };
        buttons_card(p, y2, '\u{E8B7}', "Model files", Some(&files_desc), &[(Btn::OpenModels, "Open", false), (Btn::MoveModels, "Move…", false)])
    }

    fn draw_about(&mut self, p: &mut PageDraw<'_>, mut y: f32) -> f32 {
        let update = core().update.lock().unwrap().clone();
        let state = core().update_state.lock().unwrap().clone();
        let desc = match (&update, state) {
            (_, Some(s)) => Some(s),
            (Some(u), None) => Some(format!("{} is available", u.version)),
            (None, None) => None,
        };
        let install_label = update.as_ref().map(|u| format!("Install {}", u.version));
        let buttons: Vec<(Btn, &str, bool)> = match &install_label {
            Some(l) => vec![(Btn::InstallUpdate, l.as_str(), true), (Btn::CheckNow, "Check now", false)],
            None => vec![(Btn::CheckNow, "Check now", false)],
        };
        y = buttons_card(p, y, '\u{E946}', &format!("Nuiter {}", Version::current()), desc.as_deref(), &buttons);
        y = toggle_card(p, y, '\u{E895}', "Check for updates automatically", None, Toggle::CheckUpdates, self.s.check_for_updates);
        y = buttons_card(p, y, '\u{E8B7}', "Data folder", Some(&Paths::home().display().to_string()), &[(Btn::OpenData, "Open", false)]);
        y = section(p, y, "Advanced", false);
        let (y2, ctl) = card_error(p, y, Some('\u{E774}'), "Update source", self.field_error(Field::UpdateRepo), 220.0, 32.0);
        self.draw_field(p, ctl, Field::UpdateRepo);
        // Licences, as a footnote.
        let credits = format!("EmbeddingGemma 2 (Apache 2.0) · llama.cpp {} (MIT) · Nuiter (MIT)", installer::flavor());
        p.c.text(Rect::new(p.x + 2.0, y2 + 14.0, p.w, 18.0), &credits, &TextStyle::new(12.0, p.t.text3));
        y2 + 14.0 + 18.0
    }

    // ---------- input ----------

    fn dip(&self, l: LPARAM) -> (f32, f32) {
        let (x, y) = window::mouse_xy(l);
        (x as f32 / self.scale(), y as f32 / self.scale())
    }

    fn mouse_down(&mut self, x: f32, y: f32, double: bool) {
        let hit = self.hits.at(x, y);
        self.pressed = hit;
        // SAFETY: mouse capture of our window.
        unsafe { SetCapture(self.hwnd) };
        match hit {
            Some(Hit::Field(f)) => {
                self.set_focus(Some(f));
                let style = TextStyle::new(14.0, theme::current().text);
                // SAFETY: plain call.
                let shift = (unsafe { GetKeyState(VK_SHIFT.0 as i32) } as u16 & 0x8000) != 0;
                let fs = self.field(f);
                if double {
                    fs.text.select_word(x, &style);
                } else {
                    fs.text.click(x, &style, shift);
                }
                self.selecting = !double;
            }
            Some(Hit::ScrollBar) => {
                let (drag, scroll) = widgets::ScrollDrag::press(self.bar_view, self.content_h, self.scroll, y);
                self.scroll = scroll;
                self.bar_drag = Some(drag);
            }
            _ => self.set_focus(None),
        }
        self.invalidate();
    }

    fn mouse_up(&mut self, x: f32, y: f32) {
        // SAFETY: releases our capture.
        unsafe {
            let _ = ReleaseCapture();
        }
        self.selecting = false;
        if self.bar_drag.take().is_some() {
            self.invalidate();
        }
        let hit = self.hits.at(x, y);
        let pressed = self.pressed.take();
        if hit.is_none() || hit != pressed {
            return;
        }
        let hwnd = self.hwnd;
        // Actions that show dialogs or menus run once this message is handled.
        let run = move |f: Box<dyn FnOnce(&mut SettingsWindow)>| {
            let _ = hwnd;
            window::later(move || app::with_settings(f));
        };
        match hit.unwrap() {
            Hit::Nav(page) => {
                self.page = page;
                self.scroll = 0.0;
            }
            Hit::Toggle(t) => self.flip(t),
            Hit::Combo(c) => run(Box::new(move |w| w.combo(c))),
            Hit::Button(b) => run(Box::new(move |w| w.button(b))),
            Hit::Expand(l, i) => self.expand(l, i),
            Hit::OpenRow(l, i) => {
                let p = self.list(l)[i].path.clone();
                if std::path::Path::new(&p).is_dir() {
                    launcher::open_folder(&p);
                }
            }
            Hit::Remove(l, i) => run(Box::new(move |w| w.remove_row(l, i))),
            Hit::Sub(l, i, j) => self.flip_sub(l, i, j),
            Hit::Field(_) | Hit::ScrollBar => {}
        }
        self.invalidate();
    }

    fn mouse_move(&mut self, x: f32, y: f32) {
        if let Some(drag) = self.bar_drag {
            self.scroll = drag.to(self.bar_view, self.content_h, y);
            self.invalidate();
            return;
        }
        if self.selecting
            && let Some(f) = self.focus
        {
            let style = TextStyle::new(14.0, theme::current().text);
            self.field(f).text.drag(x, &style);
            self.invalidate();
            return;
        }
        let hover = self.hits.at(x, y);
        if hover != self.hover {
            self.hover = hover;
            self.invalidate();
        }
        // SAFETY: asks for WM_MOUSELEAVE.
        unsafe {
            let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
            let _ = TrackMouseEvent(&mut tme);
        }
    }

    fn key_down(&mut self, vk: u16) -> bool {
        let Some(f) = self.focus else { return false };
        // SAFETY: plain calls.
        let (ctrl, shift) = unsafe { (GetKeyState(VK_CONTROL.0 as i32), GetKeyState(VK_SHIFT.0 as i32)) };
        let (ctrl, shift) = ((ctrl as u16 & 0x8000) != 0, (shift as u16 & 0x8000) != 0);
        match VIRTUAL_KEY(vk) {
            VK_RETURN => {
                self.commit(f);
                return true;
            }
            VK_ESCAPE => {
                self.set_focus(None);
                return true;
            }
            VK_TAB => {
                // The next field on this page.
                let on_page: Vec<Field> = FIELDS.iter().copied().filter(|x| self.hits.rect_of(Hit::Field(*x)).is_some()).collect();
                if let Some(i) = on_page.iter().position(|x| *x == f) {
                    let n = on_page.len() as i32;
                    let next = on_page[((i as i32 + if shift { -1 } else { 1 } + n) % n) as usize];
                    self.set_focus(Some(next));
                    self.field(next).text.select_all();
                }
                return true;
            }
            _ => {}
        }
        let r = self.field(f).text.key_down(vk, ctrl, shift);
        self.caret_on = true;
        self.invalidate();
        r != Edit::None
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        self.closing.cancel();
    }
}

impl window::Handler for SettingsWindow {
    fn message(&mut self, hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                // SAFETY: matched BeginPaint/EndPaint.
                unsafe { BeginPaint(hwnd, &mut ps) };
                self.paint();
                // SAFETY: ends the paint begun above.
                unsafe {
                    let _ = EndPaint(hwnd, &ps);
                }
                Some(LRESULT(0))
            }
            WM_SIZE => {
                let (cw, ch) = ((l.0 & 0xFFFF) as u32, ((l.0 >> 16) & 0xFFFF) as u32);
                if let Some(c) = &self.canvas {
                    c.resize(cw, ch);
                }
                self.invalidate();
                None
            }
            WM_GETMINMAXINFO => {
                // SAFETY: lParam is a MINMAXINFO.
                let mm = unsafe { &mut *(l.0 as *mut MINMAXINFO) };
                let s = window::dpi_of(hwnd) / 96.0;
                mm.ptMinTrackSize.x = (760.0 * s) as i32;
                mm.ptMinTrackSize.y = (520.0 * s) as i32;
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                self.dpi = (w.0 & 0xFFFF) as f32;
                if let Some(c) = self.canvas.as_mut() {
                    c.set_dpi(self.dpi);
                }
                // SAFETY: lParam is the suggested rectangle.
                let r = unsafe { &*(l.0 as *const RECT) };
                // SAFETY: moving our own window.
                unsafe {
                    let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                let (x, y) = self.dip(l);
                self.mouse_down(x, y, msg == WM_LBUTTONDBLCLK);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = self.dip(l);
                self.mouse_up(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = self.dip(l);
                self.mouse_move(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                if self.hover.take().is_some() {
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            // The mouse was taken (a menu, a dialog): a held scroll bar lets go.
            WM_CAPTURECHANGED => {
                if self.bar_drag.take().is_some() {
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((w.0 >> 16) & 0xFFFF) as i16;
                let (_, h) = self.size();
                let max = (self.content_h - h).max(0.0);
                self.scroll = (self.scroll - delta as f32 / 120.0 * 48.0).clamp(0.0, max);
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_SETCURSOR if (l.0 & 0xFFFF) as u32 == HTCLIENT => {
                let mut pt = window::cursor();
                // SAFETY: plain calls.
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut pt);
                }
                let s = self.scale();
                let cursor = match self.hits.at(pt.x as f32 / s, pt.y as f32 / s) {
                    Some(Hit::Field(_)) => IDC_IBEAM,
                    Some(Hit::ScrollBar) | None => IDC_ARROW,
                    Some(_) => IDC_HAND,
                };
                // SAFETY: a system cursor.
                unsafe {
                    SetCursor(LoadCursorW(None, cursor).ok());
                }
                Some(LRESULT(1))
            }
            WM_KEYDOWN => self.key_down(w.0 as u16).then_some(LRESULT(0)),
            WM_CHAR => {
                if let Some(f) = self.focus {
                    let ch = w.0 as u16;
                    if ch >= 0x20 && self.field(f).text.char_input(ch) == Edit::Changed {
                        self.caret_on = true;
                        self.invalidate();
                    }
                    return Some(LRESULT(0));
                }
                None
            }
            WM_TIMER if w.0 == TIMER_CARET => {
                if self.focus.is_some() {
                    self.caret_on = !self.caret_on;
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_ACTIVATE => {
                if (w.0 & 0xFFFF) as u32 == WA_INACTIVE
                    && let Some(f) = self.focus
                {
                    self.commit(f);
                }
                None
            }
            WM_CLOSE => {
                // A field still being edited when the window closes is applied too.
                if let Some(f) = self.focus.take() {
                    self.commit(f);
                }
                self.closing.cancel();
                None
            }
            WM_DESTROY => {
                window::later(app::settings_closed);
                None
            }
            _ => None,
        }
    }
}

// ---------- page building blocks ----------

struct PageDraw<'a> {
    c: &'a Canvas,
    t: &'a theme::Theme,
    x: f32,
    w: f32,
    hover: Option<Hit>,
    hits: &'a mut Hits<Hit>,
}

fn page_title(p: &mut PageDraw<'_>, y: f32, title: &str) -> f32 {
    p.c.text(Rect::new(p.x, y, p.w, 40.0), title, &TextStyle::new(28.0, p.t.text).bold().font(Font::Display));
    y + 38.0 + 18.0
}

fn section(p: &mut PageDraw<'_>, y: f32, title: &str, first: bool) -> f32 {
    let y = if first { y } else { y + SECTION_GAP };
    p.c.text(Rect::new(p.x + 1.0, y, p.w, 20.0), title, &TextStyle::new(14.0, p.t.text).bold());
    y + 19.0 + 8.0
}

/// A name and a line of detail under it, centred together in `r` (a folder's row, a subfolder's).
fn two_lines(c: &Canvas, r: Rect, name: &str, detail: &str, t: &theme::Theme) {
    let (ns, ds) = (TextStyle::new(14.0, t.text), TextStyle::new(12.0, t.text2));
    let (nh, dh) = crate::ui::gfx::with(|g| (g.line_height(&ns), g.line_height(&ds)));
    let top = r.y + ((r.h - nh - dh) / 2.0).max(0.0);
    c.text(Rect::new(r.x, top, r.w, nh + 1.0), name, &ns);
    c.text(Rect::new(r.x, top + nh, r.w, dh + 1.0), detail, &ds);
}

/// A card's height and where its control goes (right-aligned, vertically centred).
#[allow(clippy::too_many_arguments)]
fn card_layout(p: &mut PageDraw<'_>, y: f32, icon: Option<char>, title: &str, desc: Option<&str>, control_w: f32, control_h: f32, _frame: bool) -> (f32, Rect) {
    let text_x = p.x + 16.0 + if icon.is_some() { 2.0 + 20.0 + 18.0 } else { 0.0 };
    let text_w = (p.x + p.w - 16.0 - control_w - if control_w > 0.0 { 20.0 } else { 0.0 } - text_x).max(40.0);
    let th = crate::ui::gfx::with(|g| g.measure(title, &TextStyle::new(14.0, p.t.text).wrap(), text_w)).1;
    let dh = desc.map(|d| 1.0 + crate::ui::gfx::with(|g| g.measure(d, &TextStyle::new(12.0, p.t.text2).wrap(), text_w)).1).unwrap_or(0.0);
    // A card of one line is as tall as its control needs; with a second line it gets the room of two.
    let h = (11.0 + th.max(19.0) + dh + 11.0).max(if desc.is_some() { 66.0 } else { 54.0 }).max(control_h + 22.0);
    let ctl = Rect::new(p.x + p.w - 16.0 - control_w, y + (h - control_h) / 2.0, control_w, control_h);
    (h, ctl)
}

/// A card's icon, title and the line under it (`desc`; red when it says what's wrong with the card's field).
#[allow(clippy::too_many_arguments)]
fn draw_card_text(p: &mut PageDraw<'_>, y: f32, h: f32, icon: Option<char>, title: &str, desc: Option<&str>, control_w: f32, error: bool) {
    let text_x = p.x + 16.0 + if icon.is_some() { 2.0 + 20.0 + 18.0 } else { 0.0 };
    let text_w = (p.x + p.w - 16.0 - control_w - if control_w > 0.0 { 20.0 } else { 0.0 } - text_x).max(40.0);
    if let Some(g) = icon {
        p.c.icon(Rect::new(p.x + 18.0, y, 20.0, h), g, 18.0, p.t.text);
    }
    let ts = TextStyle::new(14.0, p.t.text).wrap();
    let th = crate::ui::gfx::with(|g| g.measure(title, &ts, text_w)).1;
    let dh = desc.map(|d| crate::ui::gfx::with(|g| g.measure(d, &TextStyle::new(12.0, p.t.text2).wrap(), text_w)).1).unwrap_or(0.0);
    let block = th + if desc.is_some() { 1.0 + dh } else { 0.0 };
    let ty = y + (h - block) / 2.0;
    p.c.text(Rect::new(text_x, ty, text_w, th + 2.0), title, &ts);
    if let Some(d) = desc {
        let color = if error { p.t.error } else { p.t.text2 };
        p.c.text(Rect::new(text_x, ty + th + 1.0, text_w, dh + 2.0), d, &TextStyle::new(12.0, color).wrap());
    }
}

/// A framed setting card with a control of the given size on the right; returns the next y and the control's place.
fn card(p: &mut PageDraw<'_>, y: f32, icon: Option<char>, title: &str, desc: Option<&str>, control_w: f32, control_h: f32) -> (f32, Rect) {
    card_with(p, y, icon, title, desc, false, control_w, control_h)
}

/// A card for a field: no line under its title unless what's in the field is wrong (`error`, in red).
fn card_error(p: &mut PageDraw<'_>, y: f32, icon: Option<char>, title: &str, error: Option<&str>, control_w: f32, control_h: f32) -> (f32, Rect) {
    card_with(p, y, icon, title, error, true, control_w, control_h)
}

#[allow(clippy::too_many_arguments)]
fn card_with(p: &mut PageDraw<'_>, y: f32, icon: Option<char>, title: &str, desc: Option<&str>, error: bool, control_w: f32, control_h: f32) -> (f32, Rect) {
    let (h, ctl) = card_layout(p, y, icon, title, desc, control_w, control_h, true);
    let r = Rect::new(p.x, y, p.w, h);
    p.c.round(r, widgets::CARD_RADIUS, p.t.panel_bg);
    p.c.stroke_round(r, widgets::CARD_RADIUS, p.t.stroke, 1.0);
    draw_card_text(p, y, h, icon, title, desc, control_w, error);
    (y + h + CARD_GAP, ctl)
}

fn toggle_card(p: &mut PageDraw<'_>, y: f32, icon: char, title: &str, desc: Option<&str>, t: Toggle, on: bool) -> f32 {
    let (ny, ctl) = card(p, y, Some(icon), title, desc, widgets::TOGGLE_WIDTH, widgets::CONTROL_HEIGHT);
    widgets::toggle(p.c, p.t, ctl, on, p.hover == Some(Hit::Toggle(t)), true);
    p.hits.add(ctl, Hit::Toggle(t));
    ny
}

fn buttons_card(p: &mut PageDraw<'_>, y: f32, icon: char, title: &str, desc: Option<&str>, buttons: &[(Btn, &str, bool)]) -> f32 {
    let widths: Vec<f32> = buttons.iter().map(|b| widgets::button_width(b.1)).collect();
    let total = widths.iter().sum::<f32>() + 6.0 * (buttons.len().saturating_sub(1)) as f32;
    let (ny, ctl) = card(p, y, Some(icon), title, desc, total, widgets::BUTTON_HEIGHT);
    let mut x = ctl.x;
    for ((b, label, accent), w) in buttons.iter().zip(widths) {
        let r = Rect::new(x, ctl.y, w, widgets::BUTTON_HEIGHT);
        if *accent {
            widgets::accent_button(p.c, p.t, r, label, p.hover == Some(Hit::Button(*b)));
        } else {
            widgets::subtle_button(p.c, p.t, r, label, p.hover == Some(Hit::Button(*b)), true);
        }
        p.hits.add(r, Hit::Button(*b));
        x += w + 6.0;
    }
    ny
}

#[allow(clippy::too_many_arguments)]
fn combo_card(p: &mut PageDraw<'_>, y: f32, icon: char, title: &str, desc: Option<&str>, combo: Combo, label: &str, width: f32) -> f32 {
    let (ny, ctl) = card(p, y, Some(icon), title, desc, width, widgets::CONTROL_HEIGHT);
    widgets::combo(p.c, p.t, ctl, label, p.hover == Some(Hit::Combo(combo)));
    p.hits.add(ctl, Hit::Combo(combo));
    ny
}

fn text_card(p: &mut PageDraw<'_>, y: f32, icon: char, title: &str, desc: &str) -> f32 {
    card(p, y, Some(icon), title, Some(desc), 0.0, 0.0).0
}

/// On every page until the model is there: laid out like a card (icon, title and note centred, the button on the
/// right), with the download's progress under the note while it runs.
fn draw_download_card(p: &mut PageDraw<'_>, y: f32) -> f32 {
    const ICON: char = '\u{E896}';
    const TITLE: &str = "Download the search model";
    let downloading = core().downloading();
    let (status, _) = core().status_line();
    let note = if downloading { status.clone() } else { format!("{} · until then, search finds words only", installer::download_size_text()) };
    let bw = if downloading { 0.0 } else { widgets::button_width("Download") };
    let (h, ctl) = card_layout(p, y, Some(ICON), TITLE, Some(&note), bw, widgets::BUTTON_HEIGHT, true);
    // The progress bar takes a line of its own under the text.
    let bar_h = if downloading { 3.0 + 10.0 } else { 0.0 };
    let r = Rect::new(p.x, y, p.w, h + bar_h);
    p.c.round(r, widgets::CARD_RADIUS, p.t.panel_bg);
    p.c.stroke_round(r, widgets::CARD_RADIUS, p.t.accent, 1.0);
    draw_card_text(p, y, h, Some(ICON), TITLE, Some(&note), bw, false);
    if downloading {
        let text_x = p.x + 56.0;
        let bar = Rect::new(text_x, y + h - 4.0, p.x + p.w - 16.0 - text_x, 3.0);
        p.c.round(bar, 1.5, p.t.stroke);
        if let Some(f) = core().download_fraction() {
            p.c.round(Rect::new(bar.x, bar.y, bar.w * f as f32, bar.h), 1.5, p.t.accent);
        }
    } else {
        widgets::accent_button(p.c, p.t, ctl, "Download", p.hover == Some(Hit::Button(Btn::Download)));
        p.hits.add(ctl, Hit::Button(Btn::Download));
    }
    y + h + bar_h + SECTION_GAP
}

// ---------- values ----------

fn field_value(s: &Settings, id: Field) -> String {
    match id {
        Field::Hotkey => s.hotkey.clone(),
        Field::ExcludedExt => s.excluded_extensions.join(", "),
        Field::IndexFrom => s.index_from.clone(),
        Field::IndexTo => s.index_to.clone(),
        Field::Idle => s.worker_idle_minutes.to_string(),
        Field::MaxPdfPages => s.max_pdf_pages.to_string(),
        Field::UpdateRepo => s.update_repository.clone(),
    }
}

fn field_valid(id: Field, text: &str) -> bool {
    match id {
        Field::Hotkey => text.is_empty() || hotkeys::parse(text).is_some(),
        Field::ExcludedExt => true,
        Field::IndexFrom | Field::IndexTo => nuiter_core::index::power::parse_time(text).is_some(),
        Field::Idle => text.parse::<i32>().is_ok_and(|m| m >= 0),
        Field::MaxPdfPages => text.parse::<i32>().is_ok_and(|n| (1..=100_000).contains(&n)),
        Field::UpdateRepo => updates::REPOSITORY_PATTERN.is_match(text),
    }
}

fn parse_extensions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for x in text.split([',', ';', ' ']).map(str::trim).filter(|x| !x.is_empty()) {
        let e = if x.starts_with('.') { x.to_lowercase() } else { format!(".{}", x.to_lowercase()) };
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

fn index_bytes() -> f64 {
    let db = Paths::database();
    [db.clone(), db.with_extension("db-wal")].iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len() as f64).sum()
}

/// "3,412 files · ≈ 12 min to index", "3,412 files · indexed", plus the code index's size.
fn describe(e: &Workload, code: bool) -> String {
    if e.files == 0 {
        return if code { "No code to index".into() } else { "Nothing to index".into() };
    }
    let files = format!("{} {}", app::thousands(e.files as i64), if e.files == 1 { "file" } else { "files" });
    let size = if code && e.bytes > 0.0 { format!(" · index ≈ {}", forecast::size_text(e.bytes)) } else { String::new() };
    let left = e.files - e.indexed;
    if left == 0 {
        return format!("{files} · indexed{size}");
    }
    let todo = if left == e.files { String::new() } else { format!(" · {} to go", app::thousands(left as i64)) };
    let later = if e.rest_seconds >= 1.0 { format!(" (+ {} later)", forecast::duration_text(e.rest_seconds)) } else { String::new() };
    let time = if e.seconds < 0.5 { "< 1 s".to_string() } else { format!("≈ {}", forecast::duration_text(e.seconds)) };
    format!("{files}{todo} · {time} to index{later}{size}")
}

