//! Makes the Windows taskbar search box open Nuiter, on every taskbar (primary and secondary monitors).
//!
//! Windows 11 has no supported API for replacing taskbar search, and drawing over the taskbar is unreliable (after
//! Windows Search is used, Explorer moves the taskbar into a z-order band ordinary windows can't cover). So the native
//! box stays as it is, and the click is taken over instead: the box is located with UI Automation (AutomationId
//! "SearchButton", off-thread with a time limit) or by its pixels when Explorer stops exposing it, and a low-level mouse
//! hook on its own thread swallows a left click that lands on it, when the taskbar itself is the window under the
//! cursor. Nothing is injected into Explorer and no system setting is changed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, RwLock, mpsc};
use std::time::{Duration, Instant};

use nuiter_core::Log;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BSTR, w};

/// A taskbar search box: the taskbar it lives on and its screen rectangle (physical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchBox {
    pub taskbar: isize,
    pub rect: RECT,
}

static BOXES: RwLock<Vec<SearchBox>> = RwLock::new(Vec::new());
static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
static SWALLOW_UP: AtomicBool = AtomicBool::new(false);

/// The search boxes currently known.
pub fn boxes() -> Vec<SearchBox> {
    BOXES.read().unwrap().clone()
}

/// The box on the monitor the cursor is on.
pub fn box_on_cursor_monitor() -> Option<SearchBox> {
    let p = crate::ui::window::cursor();
    // SAFETY: plain calls.
    let m = unsafe { MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST) };
    // SAFETY: plain calls.
    boxes().into_iter().find(|b| unsafe { MonitorFromWindow(HWND(b.taskbar as _), MONITOR_DEFAULTTONEAREST) } == m)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
    if !on {
        BOXES.write().unwrap().clear();
    }
}

pub fn start(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("TaskbarTracker".into()).stack_size(512 << 10).spawn(track_loop);
    // The hook lives on its own thread with its own message loop so that nothing else can delay mouse input.
    let _ = std::thread::Builder::new().name("MouseHook".into()).stack_size(256 << 10).spawn(|| {
        // SAFETY: a hook with its own message loop on this thread.
        unsafe {
            HOOK_THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
            let module = GetModuleHandleW(None).ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
            let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
            if let Err(e) = &hook {
                Log::warn(format!("Mouse hook failed: {e}"));
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
            if let Ok(h) = hook {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    });
}

pub fn stop() {
    BOXES.write().unwrap().clear();
    let t = HOOK_THREAD.load(Ordering::SeqCst);
    if t != 0 {
        // SAFETY: asks the hook thread's loop to end.
        unsafe {
            let _ = PostThreadMessageW(t, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = w.0 as u32;
        if msg == WM_LBUTTONDOWN {
            // SAFETY: for WH_MOUSE_LL, lParam is an MSLLHOOKSTRUCT.
            let m = unsafe { &*(l.0 as *const MSLLHOOKSTRUCT) };
            let boxes = BOXES.read().unwrap();
            for b in boxes.iter() {
                let r = b.rect;
                if m.pt.x < r.left || m.pt.x >= r.right || m.pt.y < r.top || m.pt.y >= r.bottom {
                    continue;
                }
                // Only when the taskbar is really what's under the cursor (not a menu, flyout or fullscreen app).
                // SAFETY: plain window-manager calls.
                let root = unsafe { GetAncestor(WindowFromPoint(m.pt), GA_ROOT) };
                if root.0 as isize != b.taskbar {
                    continue;
                }
                SWALLOW_UP.store(true, Ordering::SeqCst);
                let clicked = *b;
                crate::ui::window::invoke(move || crate::app::on_taskbar_click(clicked));
                return LRESULT(1);
            }
        } else if msg == WM_LBUTTONUP && SWALLOW_UP.swap(false, Ordering::SeqCst) {
            return LRESULT(1);
        }
    }
    // SAFETY: passing the event on.
    unsafe { CallNextHookEx(None, code, w, l) }
}

// ---------- locating the boxes ----------

#[derive(Default)]
struct Tracked {
    uia_rect: Option<(Instant, RECT)>,
    uia_hung: bool,
    pixel_rect: Option<RECT>,
    last_pixel_scan: Option<Instant>,
    rect: Option<RECT>,
}

/// Each taskbar's search box as UI Automation last saw it, and when.
type UiaResults = std::sync::Arc<Mutex<HashMap<isize, (Instant, Option<RECT>)>>>;

/// UI Automation runs on a thread of its own: its calls into Explorer can hang when the taskbar's provider misbehaves.
struct Uia {
    requests: mpsc::Sender<isize>,
    results: UiaResults,
    asked: HashMap<isize, Instant>,
}

impl Uia {
    fn start() -> Uia {
        let (tx, rx) = mpsc::channel::<isize>();
        let results: UiaResults = Default::default();
        let out = results.clone();
        let _ = std::thread::Builder::new().name("TaskbarUia".into()).stack_size(512 << 10).spawn(move || {
            nuiter_core::win::init_com_mta();
            // SAFETY: the UI Automation client object.
            let automation: Option<IUIAutomation> = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok() };
            let mut buttons: HashMap<isize, (Option<IUIAutomationElement>, Instant)> = HashMap::new();
            let mut missing: HashMap<isize, bool> = HashMap::new();
            while let Ok(tray) = rx.recv() {
                let Some(a) = &automation else {
                    out.lock().unwrap().insert(tray, (Instant::now(), None));
                    continue;
                };
                let entry = buttons.entry(tray).or_insert((None, Instant::now() - Duration::from_secs(10)));
                if entry.0.is_none() && entry.1.elapsed() > Duration::from_secs(3) {
                    entry.1 = Instant::now();
                    entry.0 = find_search_button(a, tray);
                    let now_missing = entry.0.is_none();
                    if now_missing && !missing.get(&tray).copied().unwrap_or(false) {
                        Log::info(format!("Taskbar {tray:#x}: search box not exposed via UI Automation"));
                    }
                    missing.insert(tray, now_missing);
                }
                let rect = entry.0.as_ref().and_then(|b| {
                    // SAFETY: a COM call on a live element.
                    match unsafe { b.CurrentBoundingRectangle() } {
                        Ok(r) if r.right - r.left > 8 => Some(r),
                        Ok(_) => None,
                        Err(_) => None,
                    }
                });
                if rect.is_none() {
                    entry.0 = None;
                }
                out.lock().unwrap().insert(tray, (Instant::now(), rect));
            }
        });
        Uia { requests: tx, results, asked: HashMap::new() }
    }
}

fn find_search_button(a: &IUIAutomation, tray: isize) -> Option<IUIAutomationElement> {
    // SAFETY: UI Automation calls on owned interfaces.
    unsafe {
        let root = a.ElementFromHandle(HWND(tray as _)).ok()?;
        let value = windows::Win32::System::Variant::VARIANT::from(BSTR::from("SearchButton"));
        let cond = a.CreatePropertyCondition(UIA_AutomationIdPropertyId, &value).ok()?;
        root.FindFirst(TreeScope_Descendants, &cond).ok()
    }
}

/// How often the boxes are looked for: often while the cursor is on a taskbar (a click may come), seldom otherwise
/// (each look asks Explorer through UI Automation; every 300 ms it kept the PC from ever idling).
const LOOK_NEAR: Duration = Duration::from_millis(300);
const LOOK_AWAY: Duration = Duration::from_millis(2000);

fn cursor_on_taskbar(trays: &[isize]) -> bool {
    let p = crate::ui::window::cursor();
    trays.iter().any(|&t| {
        let mut r = RECT::default();
        // SAFETY: valid out pointer.
        unsafe { GetWindowRect(HWND(t as _), &mut r) }.is_ok() && p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
    })
}

fn track_loop() {
    let mut taskbars: HashMap<isize, Tracked> = HashMap::new();
    let mut uia = Uia::start();
    let mut interval = LOOK_NEAR;
    loop {
        // Short steps, so the cursor reaching a taskbar is noticed soon even while looking seldom.
        let slept = Instant::now();
        while slept.elapsed() < interval {
            std::thread::sleep(LOOK_NEAR);
            if interval == LOOK_AWAY && ENABLED.load(Ordering::SeqCst) && cursor_on_taskbar(&find_taskbars()) {
                break;
            }
        }
        if !ENABLED.load(Ordering::SeqCst) {
            BOXES.write().unwrap().clear();
            interval = LOOK_AWAY;
            continue;
        }
        let trays = find_taskbars();
        interval = if cursor_on_taskbar(&trays) { LOOK_NEAR } else { LOOK_AWAY };
        taskbars.retain(|h, _| trays.contains(h));
        let mut found = vec![];
        for tray in trays {
            let t = taskbars.entry(tray).or_default();
            // Auto-hidden or sliding: the box can't be clicked anyway.
            // SAFETY: plain call.
            if !unsafe { IsWindowVisible(HWND(tray as _)) }.as_bool() || !tray_on_screen(tray) {
                continue;
            }
            // Ask UI Automation again when its last answer came back; give up on it while it hangs.
            let answered = uia.results.lock().unwrap().get(&tray).copied();
            let asked = uia.asked.get(&tray).copied();
            match (asked, answered) {
                (Some(a), Some((at, r))) if at >= a => {
                    t.uia_rect = r.map(|r| (at, r));
                    t.uia_hung = false;
                    uia.asked.insert(tray, Instant::now());
                    let _ = uia.requests.send(tray);
                }
                (None, _) => {
                    uia.asked.insert(tray, Instant::now());
                    let _ = uia.requests.send(tray);
                }
                (Some(a), _) if a.elapsed() > Duration::from_secs(2) => {
                    if !t.uia_hung {
                        Log::warn(format!("Taskbar {tray:#x}: UI Automation is not responding; using the pixel locator"));
                    }
                    t.uia_hung = true;
                    t.uia_rect = None;
                }
                _ => {}
            }
            let mut via = "UI Automation";
            let rect = match t.uia_rect {
                // Fresh enough: from the last look, however far apart looks are now.
                Some((at, r)) if at.elapsed() < LOOK_AWAY + Duration::from_secs(2) => Some(r),
                _ => {
                    if t.last_pixel_scan.is_none_or(|s| s.elapsed() > Duration::from_secs(1)) {
                        t.last_pixel_scan = Some(Instant::now());
                        t.pixel_rect = locate_by_pixels(tray);
                    }
                    via = "pixels";
                    t.pixel_rect
                }
            };
            if rect != t.rect {
                match rect {
                    Some(r) => Log::info(format!(
                        "Taskbar {tray:#x}: search box at {},{} {}x{} (via {via})",
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top
                    )),
                    None => Log::info(format!("Taskbar {tray:#x}: search box not found")),
                }
            }
            t.rect = rect;
            if let Some(r) = rect {
                found.push(SearchBox { taskbar: tray, rect: r });
            }
        }
        *BOXES.write().unwrap() = found;
    }
}

fn tray_on_screen(tray: isize) -> bool {
    let mut r = RECT::default();
    // SAFETY: valid out pointer.
    if unsafe { GetWindowRect(HWND(tray as _), &mut r) }.is_err() {
        return false;
    }
    let m = crate::ui::window::monitor_rect_of(HWND(tray as _));
    r.left >= m.left - 1 && r.right <= m.right + 1 && r.top >= m.top - 1 && r.bottom <= m.bottom + 1
}

/// Primary and secondary taskbars. FindWindowEx rather than EnumWindows: Explorer sometimes moves the taskbar into a
/// z-order band that EnumWindows no longer lists.
fn find_taskbars() -> Vec<isize> {
    let mut list = vec![];
    // SAFETY: plain window lookups.
    unsafe {
        if let Ok(p) = FindWindowW(w!("Shell_TrayWnd"), None)
            && !p.0.is_null()
        {
            list.push(p.0 as isize);
        }
        let mut h = HWND::default();
        while list.len() < 16 {
            match FindWindowExW(None, Some(h), w!("Shell_SecondaryTrayWnd"), None) {
                Ok(n) if !n.0.is_null() => {
                    list.push(n.0 as isize);
                    h = n;
                }
                _ => break,
            }
        }
    }
    list
}

/// The search "pill" found in the taskbar's pixels, for when UI Automation doesn't expose it: several rows that hold one
/// long run of a uniform colour that differs from the taskbar background.
fn locate_by_pixels(tray: isize) -> Option<RECT> {
    let mut tr = RECT::default();
    // SAFETY: valid out pointer.
    unsafe { GetWindowRect(HWND(tray as _), &mut tr) }.ok()?;
    let (w, h) = (tr.right - tr.left, tr.bottom - tr.top);
    if w <= 0 || h <= 0 || h > w {
        return None; // vertical taskbars are not supported by Windows 11 anyway
    }
    let px = capture(tr.left, tr.top, w, h)?;
    let (wu, hu) = (w as usize, h as usize);
    let at = |x: usize, y: usize| px[y * wu + x];
    let bg = background(&px, wu, hu);
    let (min_run, max_run) = (hu * 3, hu * 12);
    let mut rows: Vec<(usize, usize, usize)> = vec![];
    for y in 2..hu.saturating_sub(2) {
        // Longest run of near-identical colour in this row that is clearly not the background.
        let (mut best_l, mut best_r, mut run_start) = (0usize, 0usize, 0usize);
        let mut best_len = 0usize;
        for x in 1..=wu {
            let end = x == wu || dist(at(x, y), at(x - 1, y)) > 4;
            if !end {
                continue;
            }
            let len = x - run_start;
            if len > best_len && dist(at(run_start, y), bg) >= 8 {
                best_l = run_start;
                best_r = x - 1;
                best_len = len;
            }
            run_start = x;
        }
        if best_len >= min_run && best_len <= max_run {
            rows.push((y, best_l, best_r));
        }
    }
    if rows.len() < 3 {
        return None;
    }
    // Rows that overlap horizontally and are vertically close form a group; the tallest group wins.
    let mut groups: Vec<Vec<(usize, usize, usize)>> = vec![];
    for r in rows {
        let g = groups.iter_mut().find(|g| {
            let last = g[g.len() - 1];
            r.0 - last.0 <= hu / 2 && r.1 < last.2 && r.2 > last.1
        });
        match g {
            Some(g) => g.push(r),
            None => groups.push(vec![r]),
        }
    }
    let best = groups.iter().max_by_key(|g| g[g.len() - 1].0 - g[0].0)?;
    let (top, bottom) = (best[0].0, best[best.len() - 1].0);
    if bottom - top < hu / 3 {
        return None;
    }
    // Corner rounding shortens the uniform run by a couple of pixels at the measured rows.
    let left = best.iter().map(|r| r.1).min()?.saturating_sub(2);
    let right = (best.iter().map(|r| r.2).max()? + 2).min(wu - 1);
    Some(RECT { left: tr.left + left as i32, top: tr.top + top as i32 - 1, right: tr.left + right as i32 + 1, bottom: tr.top + bottom as i32 + 2 })
}

fn dist(a: u32, b: u32) -> i32 {
    let c = |v: u32, s: u32| ((v >> s) & 0xFF) as i32;
    (c(a, 16) - c(b, 16)).abs().max((c(a, 8) - c(b, 8)).abs()).max((c(a, 0) - c(b, 0)).abs())
}

/// The most common colour along the top and bottom edges of the taskbar.
fn background(px: &[u32], w: usize, h: usize) -> u32 {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for y in [1, h - 2] {
        for x in (0..w).step_by(3) {
            *counts.entry(px[y * w + x] & 0xF8F8F8).or_insert(0) += 1;
        }
    }
    counts.into_iter().max_by_key(|e| e.1).map(|e| e.0).unwrap_or(0)
}

/// The screen's pixels in a rectangle, 0x00RRGGBB.
fn capture(x: i32, y: i32, w: i32, h: i32) -> Option<Vec<u32>> {
    // SAFETY: GDI objects created and released here; the DIB's bits are read before it's deleted.
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok();
        let mut out = None;
        if let Some(dib) = dib {
            let old = SelectObject(mem, dib.into());
            if BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok() && !bits.is_null() {
                out = Some(std::slice::from_raw_parts(bits as *const u32, (w * h) as usize).iter().map(|p| p & 0xFFFFFF).collect());
            }
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        out
    }
}

