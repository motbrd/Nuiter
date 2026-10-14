//! The search window: a box to type in, filters, results with thumbnails, and a preview of the selected one (the PDF
//! page with the query's words marked, the video frame at the moment that matched, a player for recordings).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use nuiter_core::extract::protocol::CallBody;
use nuiter_core::file_kinds::{FileCategory, pathx};
use nuiter_core::index::indexer::IndexPhase;
use nuiter_core::runtime::{EmbeddingError, installer};
use nuiter_core::search::engine::{self, MatchKind, SearchQuery, SearchResult};
use nuiter_core::{Cancel, Log};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, MONITOR_DEFAULTTONEAREST, MonitorFromPoint, PAINTSTRUCT};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::Ime::{CFS_POINT, COMPOSITIONFORM, ImmGetContext, ImmReleaseContext, ImmSetCompositionWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::app::{self, core};
use crate::shell::player::Player;
use crate::shell::taskbar::SearchBox;
use crate::shell::{dragdrop, launcher, thumbnails};
use crate::ui::gfx::{self, Canvas, Color, Font, Image, Rect, TextStyle};
use crate::ui::textbox::{Edit, TextBox};
use crate::ui::widgets::{self, Hits};
use crate::ui::{native, theme, window};

const WIDTH: f32 = 820.0;
const HEIGHT: f32 = 620.0;
/// The window's edge to its content (the search box, the filters, the list, the preview).
const MARGIN: f32 = 16.0;
const LIST_W: f32 = 426.0;
const ROW_H: f32 = 48.0;
/// A row's thumbnail (its left padding, its side) and where the row's text starts.
const ROW_PAD: f32 = 8.0;
const THUMB: f32 = 36.0;
const NAME_X: f32 = ROW_PAD + THUMB + 14.0;
const ROW_GAP: f32 = 2.0;
const FOLD_H: f32 = 35.0;
const FOOTER_H: f32 = 36.0;

const TIMER_DEBOUNCE: usize = 1;
const TIMER_CARET: usize = 2;
const TIMER_ANIM: usize = 3;
const TIMER_TRIM: usize = 4;

const FILTERS: [(&str, Option<FileCategory>, bool); 7] = [
    ("All", None, false),
    ("Documents", Some(FileCategory::Document), false),
    ("PDFs", Some(FileCategory::Pdf), false),
    ("Images", Some(FileCategory::Image), false),
    ("Audio", Some(FileCategory::Audio), false),
    ("Video", Some(FileCategory::Video), false),
    ("Code", None, true),
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Query,
    Settings,
    Chip(usize),
    Row(usize),
    List,
    ListBar,
    Preview,
    PreviewBar,
    Open,
    Reveal,
    CopyPath,
    Play,
}

enum Item {
    Result(SearchResult),
    /// "▸ 14 more results": those far below the best one, folded until asked for.
    Fold(Vec<SearchResult>),
}

/// The selected result's preview: the big picture and, for a PDF page, the band to show and where the words are.
#[derive(Default)]
struct Preview {
    image: Option<Rc<Image>>,
    /// The image is the file's icon (it has no thumbnail): shown at an icon's size.
    icon: bool,
    /// (top, height) of the page band to show, in the image's pixels.
    band: Option<(f32, f32)>,
    marks: Vec<[f64; 4]>,
    scroll: f32,
    content_h: f32,
    /// Where the text scrolls (as last drawn).
    area: Rect,
}

pub struct SearchWindow {
    hwnd: HWND,
    canvas: Option<Canvas>,
    dpi: f32,
    query: TextBox,
    caret_on: bool,
    selecting: bool,
    filter: usize,
    items: Vec<Item>,
    selected: Option<usize>,
    list_scroll: f32,
    hits: Hits<Hit>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    empty: Option<(char, String, String)>,
    busy: bool,
    spin_started: Instant,
    shown_at: Instant,
    hidden_at: Instant,
    anim_start: Option<Instant>,
    status: (String, Color),
    status_note: Option<String>,
    search_gen: u64,
    cancel: Option<Cancel>,
    /// Each result's thumbnail, and whether it's only the file's icon.
    thumbs: HashMap<String, (Rc<Image>, bool)>,
    preview: Preview,
    preview_gen: u64,
    player: Option<Player>,
    drag_from: Option<(f32, f32)>,
    dragging: bool,
    /// The list's or the preview's scroll bar, held by the mouse.
    bar_drag: Option<(Hit, widgets::ScrollDrag)>,
    trimmed: bool,
    visible: bool,
}

fn text_style() -> TextStyle {
    TextStyle::new(15.0, theme::current().text)
}

impl SearchWindow {
    pub fn create() -> Rc<RefCell<SearchWindow>> {
        let class = w!("NuiterSearch");
        window::register_class(class, false);
        let state = Rc::new(RefCell::new(SearchWindow {
            hwnd: HWND::default(),
            canvas: None,
            dpi: 96.0,
            query: TextBox::default(),
            caret_on: true,
            selecting: false,
            filter: 0,
            items: vec![],
            selected: None,
            list_scroll: 0.0,
            hits: Hits::default(),
            hover: None,
            pressed: None,
            empty: None,
            busy: false,
            spin_started: Instant::now(),
            shown_at: Instant::now(),
            hidden_at: Instant::now() - Duration::from_secs(10),
            anim_start: None,
            status: (String::new(), app::GREEN),
            status_note: None,
            search_gen: 0,
            cancel: None,
            thumbs: HashMap::new(),
            preview: Preview::default(),
            preview_gen: 0,
            player: None,
            drag_from: None,
            dragging: false,
            bar_drag: None,
            trimmed: false,
            visible: false,
        }));
        let hwnd = window::create(
            class,
            "Nuiter",
            WS_POPUP | WS_THICKFRAME | WS_CLIPCHILDREN,
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            RECT { left: 0, top: 0, right: 820, bottom: 620 },
            state.clone(),
        );
        // Its frame is client area even when Windows asks while the window is busy (`SetWindowPos` in `position`).
        window::set_full_client(hwnd);
        {
            let mut s = state.borrow_mut();
            s.hwnd = hwnd;
            s.show_empty(true);
            s.status = core().status_line();
        }
        theme::apply_backdrop(hwnd, theme::ACRYLIC);
        state
    }

    fn scale(&self) -> f32 {
        self.dpi / 96.0
    }

    fn size(&self) -> (f32, f32) {
        let (w, h) = window::client_size(self.hwnd);
        (w as f32 / self.scale(), h as f32 / self.scale())
    }

    fn invalidate(&self) {
        window::invalidate(self.hwnd);
    }

    // ---------- show / hide ----------

    pub fn toggle(&mut self, anchor: Option<SearchBox>) {
        if self.visible {
            self.hide();
            return;
        }
        // A click on the taskbar box right after the window lost focus to that click means "close".
        if anchor.is_some() && self.hidden_at.elapsed() < Duration::from_millis(300) {
            return;
        }
        self.show_at(anchor);
    }

    pub fn show_at(&mut self, anchor: Option<SearchBox>) {
        // SAFETY: plain timer calls on our window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_TRIM);
        }
        if self.trimmed {
            self.trimmed = false;
            if !self.query.value().trim().is_empty() {
                self.run_search();
            }
        }
        app::warm_up();
        self.shown_at = Instant::now();
        self.position(anchor);
        self.status = core().status_line();
        self.status_note = None;
        self.anim_start = Some(Instant::now());
        self.visible = true;
        let hwnd = self.hwnd;
        // Shown once this message is handled: showing sends messages back to this window.
        window::later(move || {
            native::show_cloaked(hwnd, SW_SHOW);
            native::force_foreground(hwnd);
            // SAFETY: timers and focus on our window.
            unsafe {
                let _ = SetFocus(Some(hwnd));
                SetTimer(Some(hwnd), TIMER_ANIM, 16, None);
                SetTimer(Some(hwnd), TIMER_CARET, 530, None);
            }
        });
        self.query.select_all();
        self.caret_on = true;
        self.invalidate();
    }

    pub fn hide(&mut self) {
        if !self.visible || self.dragging {
            return;
        }
        self.visible = false;
        self.hidden_at = Instant::now();
        self.stop_player();
        let hwnd = self.hwnd;
        window::later(move || {
            // SAFETY: plain calls on our window.
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
                let _ = KillTimer(Some(hwnd), TIMER_CARET);
                // A window hidden for 30 s gives back what showing results took.
                SetTimer(Some(hwnd), TIMER_TRIM, 30_000, None);
            }
        });
    }

    /// The results, thumbnails, preview and player go; the query stays and the next show searches it again.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    fn trim(&mut self) {
        // SAFETY: plain timer call.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_TRIM);
        }
        if self.visible {
            return;
        }
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
        self.items.clear();
        self.selected = None;
        self.thumbs = HashMap::new();
        self.preview = Preview::default();
        self.preview_gen += 1;
        self.player = None;
        self.trimmed = true;
        self.canvas = None; // its bitmaps and layer memory go with it
        if !app::settings_open() {
            crate::ui::gfx::release();
        }
        let before = nuiter_core::win::private_bytes();
        core().store.release_memory();
        nuiter_core::memory::relieve();
        Log::info(format!(
            "Search window hidden: freed its results (private {} → {} MB)",
            before >> 20,
            nuiter_core::win::private_bytes() >> 20
        ));
    }

    /// Places the window above the taskbar like the Windows Search flyout: centred on the clicked box.
    fn position(&mut self, anchor: Option<SearchBox>) {
        let (work, center_x, probe) = match anchor {
            Some(b) => {
                let work = window::work_area_of(HWND(b.taskbar as _));
                (work, b.rect.left + (b.rect.right - b.rect.left) / 2, POINT { x: b.rect.left + 1, y: b.rect.top + 1 })
            }
            None => {
                let p = window::cursor();
                let work = window::work_area_at(p);
                (work, work.left + (work.right - work.left) / 2, p)
            }
        };
        let mut dx = 96u32;
        let mut dy = 96u32;
        // SAFETY: plain calls.
        unsafe {
            let _ = GetDpiForMonitor(MonitorFromPoint(probe, MONITOR_DEFAULTTONEAREST), MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        }
        self.set_dpi(dx as f32);
        let scale = self.scale();
        let w = (WIDTH * scale) as i32;
        let h = (HEIGHT.min((work.bottom - work.top) as f32 / scale - 24.0) * scale) as i32;
        let gap = (12.0 * scale) as i32;
        let x = (center_x - w / 2).clamp(work.left + gap, (work.right - w - gap).max(work.left + gap));
        let y = work.bottom - h - gap;
        // SAFETY: positions our own window.
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        }
        if let Some(c) = &self.canvas {
            c.resize(w as u32, h as u32);
        }
    }

    fn set_dpi(&mut self, dpi: f32) {
        self.dpi = dpi;
        if let Some(c) = self.canvas.as_mut() {
            c.set_dpi(dpi);
        }
    }

    pub fn theme_changed(&mut self) {
        theme::apply_backdrop(self.hwnd, theme::ACRYLIC);
        self.invalidate();
    }

    pub fn status_changed(&mut self) {
        self.status = core().status_line();
        self.status_note = None;
        if self.visible {
            self.invalidate();
        }
    }

    // ---------- search ----------

    fn query_text(&self) -> String {
        let mut text = self.query.value().trim().to_string();
        if FILTERS[self.filter].2 && !text.is_empty() && !nuiter_core::uni::starts_with_ignore_case(&text, "code:") {
            text = format!("code: {text}");
        }
        text
    }

    fn run_search(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
        self.search_gen += 1;
        let generation = self.search_gen;
        let text = self.query_text();
        if text.is_empty() || text.eq_ignore_ascii_case("code:") {
            self.items.clear();
            self.selected = None;
            self.show_empty(true);
            self.busy = false;
            self.invalidate();
            return;
        }
        let cancel = Cancel::new();
        self.cancel = Some(cancel.clone());
        let category = FILTERS[self.filter].1;
        std::thread::spawn(move || {
            nuiter_core::win::init_com_mta();
            let engine = &core().engine;
            // 1) Instant keyword results.
            let quick = SearchQuery { category, semantic: false, prefix_last: true, ..SearchQuery::new(&text) };
            match engine.search(&quick, &cancel) {
                Ok(r) if !cancel.is_cancelled() => {
                    let installed = installer::is_installed();
                    window::invoke(move || app::with_search(move |s| s.show_results(generation, r, !installed, installed)));
                    if !installed {
                        return;
                    }
                }
                Ok(_) => return,
                Err(e) => {
                    Log::warn(format!("Keyword search failed: {e}"));
                    return;
                }
            }
            // 2) Hybrid results.
            let full = SearchQuery { category, ..SearchQuery::new(&text) };
            match engine.search(&full, &cancel) {
                Ok(r) if !cancel.is_cancelled() => window::invoke(move || app::with_search(move |s| s.show_results(generation, r, true, false))),
                Ok(_) => {}
                Err(e) if e.downcast_ref::<EmbeddingError>().is_some() => {
                    Log::warn(format!("Semantic search unavailable: {e}"));
                    let msg = format!("Only exact matches: {e}");
                    window::invoke(move || app::with_search(move |s| s.search_failed(generation, msg)));
                }
                Err(e) if nuiter_core::cancel::is_cancelled(&e) => {}
                Err(e) => {
                    Log::error(format!("Search failed: {e:#}"));
                    window::invoke(move || app::with_search(move |s| s.search_failed(generation, String::new())));
                }
            }
        });
    }

    fn search_failed(&mut self, generation: u64, msg: String) {
        if generation != self.search_gen {
            return;
        }
        self.busy = false;
        if !msg.is_empty() {
            self.status_note = Some(msg);
        }
        if self.items.is_empty() {
            self.show_empty(false);
        }
        self.invalidate();
    }

    fn show_results(&mut self, generation: u64, results: Vec<SearchResult>, last: bool, more_coming: bool) {
        if generation != self.search_gen {
            return;
        }
        self.busy = more_coming;
        if more_coming {
            self.spin_started = Instant::now();
            // SAFETY: plain timer call.
            unsafe { SetTimer(Some(self.hwnd), TIMER_ANIM, 16, None) };
        }
        let selected_path = self.selected_result().map(|r| r.path.clone());
        let selected_before = self.selected_result().map(|r| (r.path.clone(), r.position));
        let thumbs_gen = thumbnails::next_generation(thumbnails::Lane::List);
        // The final results fold what trails far behind the best one; the instant ones (still typing) show everything.
        let strong = if last { engine::shown_count(&results) } else { results.len() };
        let mut results = results;
        let hidden: Vec<SearchResult> = results.split_off(strong.min(results.len()));
        self.items = results.into_iter().map(Item::Result).collect();
        if !hidden.is_empty() {
            self.items.push(Item::Fold(hidden));
        }
        for i in 0..self.items.len() {
            if let Item::Result(r) = &self.items[i] {
                self.request_thumb(&r.path.clone(), thumbs_gen);
            }
        }
        if self.items.is_empty() {
            self.selected = None;
            if last {
                self.show_empty(false);
            }
        } else {
            self.empty = None;
            let keep = self.items.iter().position(|i| matches!(i, Item::Result(r) if Some(&r.path) == selected_path.as_ref()));
            let pick = if last { 0 } else { keep.unwrap_or(0) };
            self.list_scroll = 0.0;
            // The same row with another file or page (the meaning results came after the words' ones): a new preview.
            let now = match &self.items[pick] {
                Item::Result(r) => Some((r.path.clone(), r.position)),
                _ => None,
            };
            if now != selected_before {
                self.selected = None;
            }
            self.select(pick, true);
        }
        self.invalidate();
    }

    fn request_thumb(&mut self, path: &str, generation: u64) {
        if self.thumbs.contains_key(path) {
            return;
        }
        let p = path.to_string();
        thumbnails::request(path, (72.0 * self.scale()) as i32, thumbnails::Lane::List, generation, move |img| {
            if let Some(t) = img {
                window::invoke(move || app::with_search(move |s| s.set_thumb(p, Image::from_bgra(t.pixels, t.width, t.height), t.icon)));
            }
        });
    }

    fn set_thumb(&mut self, path: String, img: Image, icon: bool) {
        if self.trimmed {
            return;
        }
        let img = Rc::new(img);
        // The preview shows the small one until its big picture comes.
        if self.selected_result().is_some_and(|r| r.path == path) && self.preview.image.is_none() {
            self.preview.image = Some(img.clone());
            self.preview.icon = icon;
        }
        self.thumbs.insert(path, (img, icon));
        self.invalidate();
    }

    fn show_empty(&mut self, hint: bool) {
        let code = FILTERS[self.filter].2;
        self.empty = Some(if hint {
            (
                '\u{E721}',
                if code { "Search your code" } else { "Search by meaning" }.into(),
                if code {
                    "Describe what the code does, or type a name.".into()
                } else {
                    "Try “dog on the beach”, “laptop receipt” or INV-2026-0042. Any language. Start with code: to search code.".into()
                },
            )
        } else {
            let indexing = matches!(core().indexer.status().phase, IndexPhase::Indexing | IndexPhase::Scanning);
            ('\u{E11A}', "No matches".into(), if indexing { "Still indexing. Try again soon.".into() } else { "Try other words.".into() })
        });
        self.stop_player();
    }

    fn set_filter(&mut self, i: usize) {
        if self.filter == i {
            return;
        }
        self.filter = i;
        self.run_search();
        self.invalidate();
    }

    fn cycle_filter(&mut self, dir: i32) {
        let n = FILTERS.len() as i32;
        self.set_filter(((self.filter as i32 + dir + n) % n) as usize);
    }

    // ---------- selection / preview ----------

    fn selected_result(&self) -> Option<&SearchResult> {
        match self.selected.and_then(|i| self.items.get(i)) {
            Some(Item::Result(r)) => Some(r),
            _ => None,
        }
    }

    /// Shows the folded results in place of their fold, the first of them selected.
    fn unfold(&mut self, at: usize) {
        let Some(Item::Fold(_)) = self.items.get(at) else { return };
        let Item::Fold(hidden) = self.items.remove(at) else { return };
        let generation = thumbnails::next_generation(thumbnails::Lane::List);
        let n = hidden.len();
        for (k, r) in hidden.into_iter().enumerate() {
            let p = r.path.clone();
            self.items.insert(at + k, Item::Result(r));
            self.request_thumb(&p, generation);
        }
        if n > 0 {
            self.select(at, true);
        }
        self.invalidate();
    }

    fn select(&mut self, i: usize, scroll_into_view: bool) {
        if self.items.is_empty() {
            return;
        }
        let i = i.min(self.items.len() - 1);
        let changed = self.selected != Some(i);
        self.selected = Some(i);
        if scroll_into_view {
            self.scroll_into_view(i);
        }
        if changed {
            self.selection_changed();
        }
        self.invalidate();
    }

    fn row_top(&self, i: usize) -> f32 {
        self.items[..i].iter().map(|it| if matches!(it, Item::Fold(_)) { FOLD_H } else { ROW_H } + ROW_GAP).sum()
    }

    fn list_rect(&self) -> Rect {
        let (_, h) = self.size();
        // Under the filters, and as far above the footer as below the filters (the preview ends there too).
        let top = MARGIN + 44.0 + 12.0 + 28.0 + 8.0;
        Rect::new(MARGIN, top, LIST_W, (h - FOOTER_H - 8.0 - top).max(0.0))
    }

    fn content_height(&self) -> f32 {
        self.row_top(self.items.len())
    }

    fn scroll_into_view(&mut self, i: usize) {
        let view = self.list_rect();
        let top = self.row_top(i);
        let h = if matches!(self.items[i], Item::Fold(_)) { FOLD_H } else { ROW_H };
        if top < self.list_scroll {
            self.list_scroll = top;
        } else if top + h > self.list_scroll + view.h {
            self.list_scroll = top + h - view.h;
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        let max = (self.content_height() - self.list_rect().h).max(0.0);
        self.list_scroll = self.list_scroll.clamp(0.0, max);
    }

    fn selection_changed(&mut self) {
        self.stop_player();
        self.preview_gen += 1;
        let generation = self.preview_gen;
        let Some(r) = self.selected_result().cloned() else { return };
        let (image, icon) = match self.thumbs.get(&r.path) {
            Some((img, icon)) => (Some(img.clone()), *icon),
            None => (None, false),
        };
        self.preview = Preview { image, icon, ..Default::default() };
        let path = r.path.clone();
        if r.category == FileCategory::Pdf
            && let Some(position) = r.position
        {
            // The PDF page that matched, read by the helper when there is one: a broken PDF can't take the window down.
            let page = position as usize;
            let query = self.query.value();
            std::thread::spawn(move || {
                nuiter_core::win::init_com_mta();
                let shot = match core().reader.clone() {
                    Some(reader) => {
                        let mut call = CallBody::new("pdf-page", &path);
                        call.page = page;
                        call.query = Some(query);
                        call.pixels = 1100;
                        reader.call(&call, Duration::from_secs(15)).and_then(|r| r.image.map(|i| (i, r.marks.unwrap_or_default())))
                    }
                    None => nuiter_core::extract::preview::pdf_page(&path, page, &query, 1100),
                };
                let Some((png, marks)) = shot else { return };
                let Some((px, w, h)) = native::decode_image(&png, 4096, 4096) else { return };
                window::invoke(move || app::with_search(move |s| s.set_preview(generation, Image::from_pbgra(px, w, h), marks, false)));
            });
        } else if r.category == FileCategory::Video
            && let Some(seconds) = r.position
        {
            std::thread::spawn(move || {
                nuiter_core::win::init_com_mta();
                let frame = match core().reader.clone() {
                    Some(reader) => {
                        let mut call = CallBody::new("video-frame", &path);
                        call.seconds = seconds;
                        call.pixels = 640;
                        reader.call(&call, Duration::from_secs(15)).and_then(|r| r.image)
                    }
                    None => nuiter_core::extract::preview::video_frame(&path, seconds, 640),
                };
                let Some(jpeg) = frame else { return };
                let Some((px, w, h)) = native::decode_image(&jpeg, 1280, 1280) else { return };
                window::invoke(move || app::with_search(move |s| s.set_preview(generation, Image::from_pbgra(px, w, h), vec![], false)));
            });
        } else {
            let pgen = thumbnails::next_generation(thumbnails::Lane::Preview);
            thumbnails::request(&path, (320.0 * self.scale()) as i32, thumbnails::Lane::Preview, pgen, move |img| {
                if let Some(t) = img {
                    window::invoke(move || {
                        app::with_search(move |s| s.set_preview(generation, Image::from_bgra(t.pixels, t.width, t.height), vec![], t.icon))
                    });
                }
            });
        }
    }

    /// The PDF page that matched with the query's words marked; with marks, the band of the page around the first of
    /// them (as wide as the page, 2:3 as high), so the words can be read in a small preview.
    fn set_preview(&mut self, generation: u64, img: Image, marks: Vec<[f64; 4]>, icon: bool) {
        if generation != self.preview_gen || self.trimmed {
            return;
        }
        let (w, h) = (img.width as f32, img.height as f32);
        let band = if marks.is_empty() {
            None
        } else {
            let band_h = h.min(w * 2.0 / 3.0);
            let m = marks[0];
            let top = ((m[1] + m[3] / 2.0) as f32 * h - band_h / 2.0).clamp(0.0, (h - band_h).max(0.0));
            Some((top, band_h))
        };
        self.preview.image = Some(Rc::new(img));
        self.preview.icon = icon;
        self.preview.band = band;
        self.preview.marks = marks;
        self.invalidate();
    }

    // ---------- the player ----------

    fn play(&mut self) {
        let Some(r) = self.selected_result().cloned() else { return };
        if !matches!(r.category, FileCategory::Audio | FileCategory::Video) {
            return;
        }
        if let Some(p) = self.player.as_mut() {
            p.toggle();
            self.invalidate();
            return;
        }
        let video_rect = (r.category == FileCategory::Video).then(|| {
            let image = self.hits.rect_of(Hit::Preview).map(|p| self.preview_image_rect(p)).unwrap_or_default();
            let s = self.scale();
            RECT { left: (image.x * s) as i32, top: (image.y * s) as i32, right: (image.right() * s) as i32, bottom: (image.bottom() * s) as i32 }
        });
        self.player = Player::open(self.hwnd, &r.path, r.position.unwrap_or(0.0), video_rect);
        self.invalidate();
    }

    pub fn stop_player(&mut self) {
        if self.player.take().is_some() {
            self.invalidate();
        }
    }

    // ---------- actions ----------

    fn open_selected(&mut self) {
        let Some(r) = self.selected_result().cloned() else { return };
        let q = self.query.value();
        self.hide();
        std::thread::spawn(move || {
            nuiter_core::win::init_com_sta();
            launcher::open(&r, &core().settings(), &q);
        });
    }

    fn reveal_selected(&mut self) {
        let Some(r) = self.selected_result().cloned() else { return };
        self.hide();
        launcher::reveal(&r.path);
    }

    fn copy_path(&mut self) {
        if let Some(r) = self.selected_result() {
            crate::ui::clipboard::set_text(&r.path);
            self.status_note = Some("Path copied".into());
            self.invalidate();
        }
    }

    fn copy_file(&mut self) {
        if let Some(r) = self.selected_result() {
            crate::ui::clipboard::set_files(std::slice::from_ref(&r.path));
            self.status_note = Some(format!("Copied {} to the clipboard", r.name));
            self.invalidate();
        }
    }

    fn context_menu(&mut self, at: POINT) {
        let Some(r) = self.selected_result().cloned() else { return };
        let open = open_label(&r);
        let hwnd = self.hwnd;
        window::later(move || {
            let items = [
                native::MenuItem::Bold(1, &open),
                native::MenuItem::Item(2, "Show in folder\tCtrl+Enter"),
                native::MenuItem::Separator,
                native::MenuItem::Item(3, "Copy file\tCtrl+C"),
                native::MenuItem::Item(4, "Copy path"),
            ];
            let cmd = native::popup_menu(hwnd, at, &items);
            app::with_search(move |s| match cmd {
                1 => s.open_selected(),
                2 => s.reveal_selected(),
                3 => s.copy_file(),
                4 => s.copy_path(),
                _ => {}
            });
        });
    }

    fn move_selection(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let cur = self.selected.map(|s| s as i32).unwrap_or(-1);
        let i = (cur + delta).clamp(0, self.items.len() as i32 - 1) as usize;
        self.select(i, true);
    }

    fn key_down(&mut self, vk: u16) -> bool {
        let ctrl = is_down(VK_CONTROL);
        let shift = is_down(VK_SHIFT);
        match VIRTUAL_KEY(vk) {
            VK_DOWN => self.move_selection(1),
            VK_UP => self.move_selection(-1),
            VK_NEXT => self.move_selection(6),
            VK_PRIOR => self.move_selection(-6),
            VK_RETURN => {
                if let Some(i) = self.selected
                    && matches!(self.items.get(i), Some(Item::Fold(_)))
                {
                    self.unfold(i);
                } else if ctrl {
                    self.reveal_selected();
                } else {
                    self.open_selected();
                }
            }
            k if ctrl && k == VIRTUAL_KEY(b'P' as u16) => self.play(),
            k if ctrl && k == VIRTUAL_KEY(b'C' as u16) && !self.query.has_selection() && self.selected_result().is_some() => self.copy_file(),
            VK_ESCAPE => {
                if self.query.is_empty() {
                    self.hide();
                } else {
                    self.query.set("");
                    self.query_changed();
                }
            }
            VK_TAB => self.cycle_filter(if shift { -1 } else { 1 }),
            _ => {
                match self.query.key_down(vk, ctrl, shift) {
                    Edit::Changed => self.query_changed(),
                    Edit::Moved => {}
                    Edit::None => return false,
                }
                self.caret_on = true;
                self.invalidate();
            }
        }
        true
    }

    fn query_changed(&mut self) {
        self.caret_on = true;
        // SAFETY: (re)starts the debounce timer.
        unsafe { SetTimer(Some(self.hwnd), TIMER_DEBOUNCE, 110, None) };
        self.invalidate();
    }

    // ---------- drawing ----------

    fn preview_image_rect(&self, panel: Rect) -> Rect {
        let Some(img) = &self.preview.image else { return Rect::default() };
        let inner = panel.inset(14.0, 14.0, 14.0, 14.0);
        let (iw, ih) = match self.preview.band {
            Some((_, bh)) => (img.width as f32, bh),
            None => (img.width as f32, img.height as f32),
        };
        let s = self.scale();
        // Pixels to DIPs, then fitted into the panel's width and at most 230 high (a file's icon at most 96: an icon,
        // not a big picture of a blank page).
        let (mut w, mut h) = (iw / s, ih / s);
        let max_h = if self.preview.icon { 96.0 } else { 230.0 };
        let fit = (inner.w / w).min(max_h / h).min(1.0);
        w *= fit;
        h *= fit;
        Rect::new(inner.x + (inner.w - w) / 2.0, inner.y - self.preview.scroll, w, h)
    }

    fn paint(&mut self) {
        if self.canvas.is_none() {
            self.canvas = Canvas::new(self.hwnd, self.dpi).ok();
        }
        let Some(c) = self.canvas.take() else { return };
        let t = theme::current();
        self.hits.clear();
        c.begin(t.window_bg);
        // The slide-in: 26 DIPs up over 200 ms, fading in over 140 ms.
        let (offset, opacity) = match self.anim_start {
            Some(s) => {
                let k = (s.elapsed().as_secs_f32() / 0.2).min(1.0);
                let ease = 1.0 - (1.0 - k).powi(3);
                (26.0 * (1.0 - ease), (s.elapsed().as_secs_f32() / 0.14).min(1.0))
            }
            None => (0.0, 1.0),
        };
        if opacity < 1.0 {
            c.push_opacity(opacity);
        }
        c.translate(0.0, offset);
        self.draw(&c, &t);
        c.reset_transform();
        if opacity < 1.0 {
            c.pop_layer();
        }
        if c.end() {
            self.canvas = Some(c);
        }
    }

    fn draw(&mut self, c: &Canvas, t: &theme::Theme) {
        let (w, h) = self.size();
        // The search box.
        let bx = Rect::new(MARGIN, MARGIN, w - 2.0 * MARGIN, 44.0);
        c.round(bx, 22.0, t.input_bg_focus);
        c.stroke_round(bx, 22.0, t.accent, 1.5);
        c.icon(Rect::new(bx.x + 16.0, bx.y, 16.0, bx.h), '\u{E721}', 16.0, t.text2);
        let settings_r = Rect::new(bx.right() - 8.0 - 32.0, bx.y + 6.0, 32.0, 32.0);
        widgets::icon_button(c, t, settings_r, '\u{E713}', self.hover == Some(Hit::Settings), t.text2);
        self.hits.add(settings_r, Hit::Settings);
        let mut text_right = settings_r.x;
        if self.busy {
            let spin = Rect::new(settings_r.x - 8.0 - 18.0, bx.y, 18.0, bx.h);
            let angle = (self.spin_started.elapsed().as_secs_f32() / 0.9).fract() * 360.0;
            c.rotate_around(angle, spin.x + spin.w / 2.0, spin.y + spin.h / 2.0);
            c.icon(spin, '\u{F16A}', 14.0, t.accent_text);
            c.reset_transform();
            text_right = spin.x - 4.0;
        }
        let tr = Rect::new(bx.x + 42.0, bx.y, text_right - bx.x - 42.0, bx.h);
        self.hits.add(tr, Hit::Query);
        let style = text_style();
        self.query.draw(c, tr, &style, "Describe what you're looking for", t.text3, t.accent.with_alpha(0.45), true, self.caret_on && self.visible);

        // Filters.
        let mut x = MARGIN;
        for (i, (label, _, _)) in FILTERS.iter().enumerate() {
            let cw = widgets::chip_width(label);
            let r = Rect::new(x, 72.0, cw, 28.0);
            widgets::chip(c, t, r, label, self.filter == i, self.hover == Some(Hit::Chip(i)));
            self.hits.add(r, Hit::Chip(i));
            x += cw + 6.0;
        }

        // Results.
        let list = self.list_rect();
        self.hits.add(list, Hit::List);
        if let Some((icon, title, text)) = self.empty.clone() {
            let area = Rect::new(MARGIN, list.y, w - 2.0 * MARGIN, list.h);
            let block_h = 34.0 * 1.4 + 12.0 + 28.0 + 6.0 + 60.0;
            let top = area.y + (area.h - block_h) / 2.0 - 40.0;
            c.icon(Rect::new(area.x, top, area.w, 48.0), icon, 34.0, t.text3);
            c.text(Rect::new(area.x, top + 60.0, area.w, 30.0), &title, &TextStyle::new(20.0, t.text).bold().font(Font::Display).center());
            let tw = 460f32.min(area.w);
            c.text(
                Rect::new(area.x + (area.w - tw) / 2.0, top + 96.0, tw, 80.0),
                &text,
                &TextStyle::new(13.0, t.text2).wrap().center().lines(20.0),
            );
        } else {
            c.clip(list);
            // Rows keep clear of the scroll bar, when there is one.
            let row_w = if self.content_height() > list.h + 0.5 { list.w - widgets::SCROLL_BAR_W - widgets::SCROLL_GAP } else { list.w - 8.0 };
            let mut y = list.y - self.list_scroll;
            for i in 0..self.items.len() {
                let fold = matches!(self.items[i], Item::Fold(_));
                let rh = if fold { FOLD_H } else { ROW_H };
                let r = Rect::new(list.x, y, row_w, rh);
                y += rh + ROW_GAP;
                if r.bottom() < list.y || r.y > list.bottom() {
                    continue;
                }
                self.hits.add(r, Hit::Row(i));
                let selected = self.selected == Some(i);
                // The selected row: its background only, no accent bar on its left edge (the user's choice).
                if selected {
                    c.round(r, widgets::RADIUS, t.selected);
                } else if self.hover == Some(Hit::Row(i)) {
                    c.round(r, widgets::RADIUS, t.hover);
                }
                match &self.items[i] {
                    Item::Fold(hidden) => {
                        let label = format!("{} more {}", hidden.len(), if hidden.len() == 1 { "result" } else { "results" });
                        // The chevron where the rows' thumbnails are, the words where their names are.
                        c.icon(Rect::new(r.x + ROW_PAD, r.y, THUMB, rh), '\u{E76C}', 10.0, t.text3);
                        c.text(Rect::new(r.x + NAME_X, r.y, r.w - NAME_X - 10.0, rh), &label, &TextStyle::new(12.0, t.text2).middle());
                    }
                    Item::Result(res) => self.draw_row(c, t, r, res),
                }
            }
            c.unclip();
            if let Some(bar) = widgets::scroll_bar(c, t, list, self.content_height(), self.list_scroll, self.bar_hot(Hit::ListBar)) {
                self.hits.add(bar, Hit::ListBar);
            }

            // The preview.
            if self.selected_result().is_some() {
                let panel = Rect::new(list.right() + 12.0, list.y, w - MARGIN - list.right() - 12.0, list.h);
                self.draw_preview(c, t, panel);
            }
        }

        // The footer.
        let fy = h - FOOTER_H;
        c.fill(Rect::new(0.0, fy, w, 1.0), t.stroke);
        let (status, dot) = match &self.status_note {
            Some(n) => (n.clone(), self.status.1),
            None => self.status.clone(),
        };
        // The keys, as wide as they are (a fixed box cut the last one short); "play" only for something that plays.
        let media = self.selected_result().is_some_and(|r| matches!(r.category, FileCategory::Audio | FileCategory::Video));
        // Keys by name like the rest ("↵" came out as a speck at this size).
        let keys = if media {
            "Enter open   Ctrl+Enter show in folder   Ctrl+C copy file   Ctrl+P play   Esc close"
        } else {
            "Enter open   Ctrl+Enter show in folder   Ctrl+C copy file   Esc close"
        };
        let key_style = TextStyle::new(12.0, t.text3).right().middle();
        let keys_w = gfx::with(|g| g.measure(keys, &key_style, 10_000.0)).0.ceil() + 2.0;
        c.ellipse(MARGIN + 3.5, fy + FOOTER_H / 2.0, 3.5, 3.5, dot);
        let status_x = MARGIN + 15.0;
        let status_w = (w - MARGIN - keys_w - 20.0 - status_x).max(60.0);
        c.text(Rect::new(status_x, fy, status_w, FOOTER_H), &status, &TextStyle::new(12.0, t.text2).middle());
        c.text(Rect::new(w - MARGIN - keys_w, fy, keys_w, FOOTER_H), keys, &key_style);
    }

    fn draw_row(&self, c: &Canvas, t: &theme::Theme, r: Rect, res: &SearchResult) {
        let thumb = Rect::new(r.x + ROW_PAD, r.y + (r.h - THUMB) / 2.0, THUMB, THUMB);
        if let Some((img, _)) = self.thumbs.get(&res.path) {
            // Uniform to fill: the middle square of the picture.
            let (iw, ih) = (img.width as f32, img.height as f32);
            let side = iw.min(ih);
            let src = Rect::new((iw - side) / 2.0, (ih - side) / 2.0, side, side);
            c.image(img, thumb, Some(src), widgets::RADIUS, 1.0);
        } else {
            c.round(thumb, widgets::RADIUS, t.chip_bg);
        }
        // The name over its folder, centred together in the row; the kind beside the name and the place beside the
        // folder, on the right.
        let (name_style, path_style) = (TextStyle::new(13.5, t.text).bold(), TextStyle::new(12.0, t.text3));
        let (nh, ph) = gfx::with(|g| (g.line_height(&name_style), g.line_height(&path_style)));
        let top = r.y + (r.h - nh - ph) / 2.0;
        let right_edge = r.right() - 10.0;
        let kind = TextStyle::new(11.0, t.text2).center().middle();
        let label = res.category.label();
        let (lw, _) = gfx::with(|g| g.measure(label, &kind, 200.0));
        let chip = Rect::new(right_edge - lw - 12.0, top + (nh - 17.0) / 2.0, lw + 12.0, 17.0);
        c.round(chip, widgets::RADIUS, t.chip_bg);
        c.text(chip, label, &kind);
        let mut right = chip.x;
        if let Some(loc) = &res.locator {
            let place = TextStyle::new(11.0, t.accent_text).right().middle();
            // A pixel to spare: a box exactly as wide as the measured text trimmed "0:00" to "0:…".
            let locw = (gfx::with(|g| g.measure(loc, &place, 200.0)).0.ceil() + 1.0).min(90.0);
            let lr = Rect::new(right_edge - locw, top + nh, locw, ph + 1.0);
            c.text(lr, loc, &place);
            right = right.min(lr.x);
        }
        let tx = r.x + NAME_X;
        let tw = (right - 8.0 - tx).max(10.0);
        c.text(Rect::new(tx, top, tw, nh + 1.0), &res.name, &name_style);
        c.text(Rect::new(tx, top + nh, tw, ph + 1.0), pathx::parent(&res.path), &path_style);
    }

    fn draw_preview(&mut self, c: &Canvas, t: &theme::Theme, panel: Rect) {
        let Some(r) = self.selected_result().cloned() else { return };
        c.round(panel, widgets::CARD_RADIUS, t.panel_bg);
        c.stroke_round(panel, widgets::CARD_RADIUS, t.stroke, 1.0);
        self.hits.add(panel, Hit::Preview);
        let inner = panel.inset(14.0, 14.0, 14.0, 14.0);

        // The buttons stay at the bottom; the rest scrolls above them.
        let buttons: Vec<(Hit, String, bool)> =
            vec![(Hit::Open, open_label(&r), true), (Hit::Reveal, "Show in folder".into(), false), (Hit::CopyPath, "Copy path".into(), false)];
        let mut rows: Vec<Vec<(Hit, String, bool, f32)>> = vec![vec![]];
        let mut line_w = 0.0;
        for (hit, label, accent) in buttons {
            let bw = widgets::button_width(&label);
            if line_w > 0.0 && line_w + bw > inner.w {
                rows.push(vec![]);
                line_w = 0.0;
            }
            rows.last_mut().unwrap().push((hit, label, accent, bw));
            line_w += bw + 6.0;
        }
        let buttons_h = rows.len() as f32 * (widgets::BUTTON_HEIGHT + 6.0);
        let mut by = inner.bottom() - buttons_h + 6.0;
        for row in &rows {
            let mut bx = inner.x;
            for (hit, label, accent, bw) in row {
                let br = Rect::new(bx, by, *bw, widgets::BUTTON_HEIGHT);
                if *accent {
                    widgets::accent_button(c, t, br, label, self.hover == Some(*hit));
                } else {
                    widgets::subtle_button(c, t, br, label, self.hover == Some(*hit), true);
                }
                self.hits.add(br, *hit);
                bx += bw + 6.0;
            }
            by += widgets::BUTTON_HEIGHT + 6.0;
        }

        let area = Rect::new(inner.x, inner.y, inner.w, (inner.h - buttons_h - 12.0).max(0.0));
        c.clip(area);
        // More below (as of the last frame): the bottom fades out instead of cutting a line in half.
        let fades = self.preview.content_h - self.preview.scroll > area.h + 1.0;
        if fades {
            c.push_fade_bottom(area, 28.0);
        }
        let mut y = area.y - self.preview.scroll;
        if let Some(img) = self.preview.image.clone() {
            let ir = self.preview_image_rect(panel);
            let ir = Rect::new(ir.x, y, ir.w, ir.h);
            if self.player.as_ref().is_none_or(|p| p.video_window().is_none()) {
                let src = self.preview.band.map(|(top, bh)| Rect::new(0.0, top, img.width as f32, bh));
                c.image(&img, ir, src, if self.preview.icon { 0.0 } else { widgets::RADIUS }, 1.0);
                // The query's words, marked the way a viewer marks what you searched for.
                let (top, bh) = self.preview.band.unwrap_or((0.0, img.height as f32));
                let sx = ir.w / img.width as f32;
                let sy = ir.h / bh;
                c.clip(ir);
                for m in &self.preview.marks {
                    let mx = ir.x + (m[0] as f32 * img.width as f32) * sx - 2.0 * sx;
                    let my = ir.y + (m[1] as f32 * img.height as f32 - top) * sy - sy;
                    let mw = m[2] as f32 * img.width as f32 * sx + 4.0 * sx;
                    let mh = m[3] as f32 * img.height as f32 * sy + 2.0 * sy;
                    c.round(Rect::new(mx, my, mw, mh), 2.0, Color::argb(0x70FFD000));
                }
                c.unclip();
            }
            y += ir.h + 8.0;
        }
        if matches!(r.category, FileCategory::Audio | FileCategory::Video) {
            // Segoe Fluent's Play and Pause glyphs: the text font's ones come out as colour emoji.
            let (glyph, label) = match &self.player {
                Some(p) if p.playing => ('\u{E769}', "Pause".to_string()),
                Some(_) => ('\u{E768}', "Resume".to_string()),
                None => ('\u{E768}', play_label(&r)),
            };
            let bw = widgets::button_width(&label) + 22.0;
            let br = Rect::new(area.x, y, bw, widgets::BUTTON_HEIGHT);
            widgets::subtle_button(c, t, br, "", self.hover == Some(Hit::Play), true);
            c.icon(Rect::new(br.x + 11.0, br.y, 14.0, br.h), glyph, 12.0, t.text);
            c.text(br.inset(33.0, 0.0, 0.0, 0.0), &label, &TextStyle::new(widgets::BUTTON_FONT, t.text).middle());
            self.hits.add(br, Hit::Play);
            y += widgets::BUTTON_HEIGHT + 10.0;
        }
        y += c.text(Rect::new(area.x, y, area.w, 2000.0), &r.name, &TextStyle::new(15.0, t.text).bold().wrap());
        let meta = format!(
            "{} · {} · modified {}\n{}",
            r.category.label(),
            format_size(r.size),
            format_date(r.modified),
            pathx::parent(&r.path)
        );
        y += 4.0 + c.text(Rect::new(area.x, y + 4.0, area.w, 2000.0), &meta, &TextStyle::new(12.0, t.text3).wrap());
        let how = match r.match_kind {
            MatchKind::Both => "Meaning and exact words",
            MatchKind::Keyword => "Exact word match",
            MatchKind::Semantic => "Matched by meaning",
        };
        let place = match (&r.locator, r.category) {
            (None, _) => String::new(),
            (Some(_), FileCategory::Pdf) => format!(" · page {}", r.position.unwrap_or(0.0) as i64),
            (Some(l), FileCategory::Audio | FileCategory::Video) => format!(" · at {l}"),
            (Some(_), FileCategory::Code) => format!(" · line {}", r.position.unwrap_or(0.0) as i64),
            (Some(l), _) => format!(" · {l}"),
        };
        y += 6.0 + c.text(Rect::new(area.x, y + 6.0, area.w, 2000.0), &format!("{how}{place}"), &TextStyle::new(12.0, t.accent_text).wrap());
        if let Some(snippet) = &r.snippet {
            y += 10.0 + c.text(Rect::new(area.x, y + 10.0, area.w, 4000.0), snippet, &TextStyle::new(12.5, t.text2).wrap().lines(19.0));
        }
        if fades {
            c.pop_layer();
        }
        c.unclip();
        self.preview.content_h = y + self.preview.scroll - area.y;
        self.preview.area = area;
        // The bar inside the panel's padding, clear of its border.
        let content_h = self.preview.content_h;
        if let Some(bar) = widgets::scroll_bar(c, t, self.preview_bar_view(), content_h, self.preview.scroll, self.bar_hot(Hit::PreviewBar)) {
            self.hits.add(bar, Hit::PreviewBar);
        }
    }

    fn preview_bar_view(&self) -> Rect {
        let a = self.preview.area;
        Rect::new(a.x, a.y, a.w + 10.0, a.h)
    }

    /// A scroll bar is hovered or held.
    fn bar_hot(&self, bar: Hit) -> bool {
        self.hover == Some(bar) || self.bar_drag.is_some_and(|(b, _)| b == bar)
    }

    /// A scroll bar's view, its content's height and its scroll.
    fn bar_state(&self, bar: Hit) -> (Rect, f32, f32) {
        if bar == Hit::PreviewBar {
            (self.preview_bar_view(), self.preview.content_h, self.preview.scroll)
        } else {
            (self.list_rect(), self.content_height(), self.list_scroll)
        }
    }

    fn set_bar_scroll(&mut self, bar: Hit, scroll: f32) {
        if bar == Hit::PreviewBar {
            self.preview.scroll = scroll;
        } else {
            self.list_scroll = scroll;
            self.clamp_scroll();
        }
    }

    // ---------- input ----------

    fn dip(&self, l: LPARAM) -> (f32, f32) {
        let (x, y) = window::mouse_xy(l);
        let s = self.scale();
        let offset = self.anim_start.map(|_| 0.0).unwrap_or(0.0);
        (x as f32 / s, y as f32 / s - offset)
    }

    fn mouse_down(&mut self, x: f32, y: f32, double: bool) {
        let hit = self.hits.at(x, y);
        self.pressed = hit;
        self.drag_from = None;
        // SAFETY: mouse capture of our own window.
        unsafe { SetCapture(self.hwnd) };
        match hit {
            Some(Hit::Query) => {
                let style = text_style();
                if double {
                    self.query.select_word(x, &style);
                } else {
                    self.query.click(x, &style, is_down(VK_SHIFT));
                    self.selecting = true;
                }
                self.caret_on = true;
            }
            Some(Hit::Row(i)) => match self.items.get(i) {
                Some(Item::Fold(_)) => self.unfold(i),
                Some(Item::Result(_)) => {
                    self.select(i, false);
                    if double {
                        self.open_selected();
                    } else {
                        self.drag_from = Some((x, y));
                    }
                }
                None => {}
            },
            Some(Hit::Chip(i)) => self.set_filter(i),
            Some(bar @ (Hit::ListBar | Hit::PreviewBar)) => {
                let (view, content, scroll) = self.bar_state(bar);
                let (drag, scroll) = widgets::ScrollDrag::press(view, content, scroll, y);
                self.set_bar_scroll(bar, scroll);
                self.bar_drag = Some((bar, drag));
            }
            _ => {}
        }
        self.invalidate();
    }

    fn mouse_up(&mut self, x: f32, y: f32) {
        // SAFETY: releases our capture.
        unsafe {
            let _ = ReleaseCapture();
        }
        self.selecting = false;
        self.drag_from = None;
        self.bar_drag = None;
        let hit = self.hits.at(x, y);
        if hit.is_some() && hit == self.pressed {
            match hit {
                Some(Hit::Settings) => {
                    self.hide();
                    window::later(app::show_settings);
                }
                Some(Hit::Open) => self.open_selected(),
                Some(Hit::Reveal) => self.reveal_selected(),
                Some(Hit::CopyPath) => self.copy_path(),
                Some(Hit::Play) => self.play(),
                _ => {}
            }
        }
        self.pressed = None;
        self.invalidate();
    }

    fn mouse_move(&mut self, x: f32, y: f32) {
        if let Some((bar, drag)) = self.bar_drag {
            let (view, content, _) = self.bar_state(bar);
            self.set_bar_scroll(bar, drag.to(view, content, y));
            self.invalidate();
            return;
        }
        if self.selecting {
            self.query.drag(x, &text_style());
            self.invalidate();
            return;
        }
        if let Some((fx, fy)) = self.drag_from
            && ((x - fx).abs() > 4.0 || (y - fy).abs() > 4.0)
            && let Some(r) = self.selected_result()
        {
            // Drag the file out (into Explorer, a mail, a chat…); the window mustn't hide while it loses focus.
            let path = r.path.clone();
            self.drag_from = None;
            window::later(move || {
                app::with_search(|s| s.dragging = true);
                // SAFETY: releases our capture before the drag takes the mouse.
                unsafe {
                    let _ = ReleaseCapture();
                }
                dragdrop::drag_file(&path);
                app::with_search(|s| s.dragging = false);
            });
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

    fn wheel(&mut self, x: f32, y: f32, delta: i16) {
        let step = -(delta as f32) / 120.0 * 48.0;
        match self.hits.at(x, y) {
            Some(Hit::Preview | Hit::PreviewBar | Hit::Open | Hit::Reveal | Hit::CopyPath | Hit::Play) => {
                let max = (self.preview.content_h - self.preview.area.h).max(0.0);
                self.preview.scroll = (self.preview.scroll + step).clamp(0.0, max);
            }
            _ => {
                self.list_scroll += step;
                self.clamp_scroll();
            }
        }
        self.invalidate();
    }

    fn place_ime(&self) {
        let s = self.scale();
        let r = self.query.caret_rect;
        // SAFETY: the input context of our window.
        unsafe {
            let himc = ImmGetContext(self.hwnd);
            if himc.0.is_null() {
                return;
            }
            let cf = COMPOSITIONFORM {
                dwStyle: CFS_POINT,
                ptCurrentPos: POINT { x: (r.x * s) as i32, y: (r.y * s) as i32 },
                rcArea: RECT::default(),
            };
            let _ = ImmSetCompositionWindow(himc, &cf);
            let _ = ImmReleaseContext(self.hwnd, himc);
        }
    }
}

fn is_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: plain call.
    (unsafe { GetKeyState(vk.0 as i32) } as u16 & 0x8000) != 0
}

/// "Open at 0:36", "Open at page 4", "Open at line 12" when the app can be told where; else "Open".
fn open_label(r: &SearchResult) -> String {
    match (r.category, r.position) {
        (FileCategory::Pdf, Some(p)) if p > 1.0 => format!("Open at page {}", p as i64),
        (FileCategory::Code, Some(line)) => format!("Open at line {}", line as i64),
        (FileCategory::Audio | FileCategory::Video, Some(p)) if p > 0.0 && launcher::opens_at_moment(&r.path) => {
            format!("Open at {}", r.locator.as_deref().unwrap_or(""))
        }
        _ => "Open".into(),
    }
}

fn play_label(r: &SearchResult) -> String {
    match r.position {
        Some(p) if p > 0.0 => format!("Play from {}", r.locator.as_deref().unwrap_or("")),
        _ => "Play".into(),
    }
}

fn format_size(b: i64) -> String {
    let b = b as f64;
    if b < 1024.0 {
        format!("{b} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{} KB", trim1(b / 1024.0))
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{} MB", trim1(b / 1048576.0))
    } else {
        format!("{} GB", trim2(b / 1073741824.0))
    }
}

fn trim1(x: f64) -> String {
    let r = (x * 10.0).round() / 10.0;
    if r.fract() == 0.0 { format!("{}", r as i64) } else { format!("{r:.1}") }
}

fn trim2(x: f64) -> String {
    let r = (x * 100.0).round() / 100.0;
    let s = format!("{r:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn format_date(ticks: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    match nuiter_core::win::ticks_to_local(ticks) {
        Some((y, m, d, _, _)) => format!("{d} {} {y}", MONTHS[(m as usize).clamp(1, 12) - 1]),
        None => String::new(),
    }
}

impl window::Handler for SearchWindow {
    fn message(&mut self, hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
        match msg {
            // The whole window is client area: DWM still draws its shadow and rounded corners. Also when the window is
            // made (wParam 0): left to Windows, the window had the thick frame of WS_THICKFRAME until its first resize.
            WM_NCCALCSIZE => Some(LRESULT(0)),
            WM_NCHITTEST => Some(LRESULT(HTCLIENT as isize)),
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                // SAFETY: matched BeginPaint/EndPaint on our window.
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
                self.clamp_scroll();
                None
            }
            WM_DPICHANGED => {
                self.set_dpi((w.0 & 0xFFFF) as f32);
                // SAFETY: lParam is the suggested rectangle.
                let r = unsafe { &*(l.0 as *const RECT) };
                // SAFETY: moving our own window.
                unsafe {
                    let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
                Some(LRESULT(0))
            }
            WM_ACTIVATE => {
                if (w.0 & 0xFFFF) as u32 == WA_INACTIVE && self.shown_at.elapsed() > Duration::from_millis(400) {
                    self.hide();
                }
                None
            }
            WM_SETCURSOR => {
                let p = window::cursor();
                let mut pt = p;
                // SAFETY: plain calls.
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                }
                let s = self.scale();
                let hit = self.hits.at(pt.x as f32 / s, pt.y as f32 / s);
                let cursor = match hit {
                    Some(Hit::Query) => IDC_IBEAM,
                    Some(Hit::Chip(_) | Hit::Settings | Hit::Open | Hit::Reveal | Hit::CopyPath | Hit::Play) => IDC_HAND,
                    _ => IDC_ARROW,
                };
                // SAFETY: a system cursor.
                unsafe {
                    SetCursor(LoadCursorW(None, cursor).ok());
                }
                Some(LRESULT(1))
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if self.key_down(w.0 as u16) {
                    return Some(LRESULT(0));
                }
                None
            }
            WM_CHAR => {
                let ch = w.0 as u16;
                if ch == 0x09 || ch == 0x0D || ch == 0x1B {
                    return Some(LRESULT(0)); // handled as keys
                }
                if self.query.char_input(ch) == Edit::Changed {
                    self.query_changed();
                }
                Some(LRESULT(0))
            }
            WM_IME_STARTCOMPOSITION => {
                self.place_ime();
                None
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
            WM_RBUTTONUP => {
                let (x, y) = self.dip(l);
                if let Some(Hit::Row(i)) = self.hits.at(x, y)
                    && matches!(self.items.get(i), Some(Item::Result(_)))
                {
                    self.select(i, false);
                    self.context_menu(window::cursor());
                }
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
            // The mouse was taken (a drag, another window): a held scroll bar lets go.
            WM_CAPTURECHANGED => {
                if self.bar_drag.take().is_some() {
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let mut pt = window::cursor();
                // SAFETY: plain call.
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                }
                let s = self.scale();
                self.wheel(pt.x as f32 / s, pt.y as f32 / s, ((w.0 >> 16) & 0xFFFF) as i16);
                Some(LRESULT(0))
            }
            WM_TIMER => {
                match w.0 {
                    TIMER_DEBOUNCE => {
                        // SAFETY: plain timer call.
                        unsafe {
                            let _ = KillTimer(Some(hwnd), TIMER_DEBOUNCE);
                        }
                        self.run_search();
                    }
                    TIMER_CARET => {
                        self.caret_on = !self.caret_on;
                        self.invalidate();
                    }
                    TIMER_ANIM => {
                        let animating = self.anim_start.is_some_and(|s| s.elapsed() < Duration::from_millis(220));
                        if !animating {
                            self.anim_start = None;
                        }
                        if !animating && !self.busy {
                            // SAFETY: plain timer call.
                            unsafe {
                                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                            }
                        }
                        self.invalidate();
                    }
                    TIMER_TRIM => self.trim(),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.hide();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}
