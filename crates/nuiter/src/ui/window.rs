//! Win32 windows with their state in Rust: a window's messages go to a handler object (`Handler`) kept in the window's
//! user data. Messages that arrive while the handler is busy (sent back by a call it made) go to DefWindowProc; work
//! that must wait for the handler to finish goes through `later`. Other threads reach the UI thread with `invoke`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Mutex;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, MonitorFromWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

pub trait Handler {
    /// Some(result) when handled; None sends the message on to DefWindowProc.
    fn message(&mut self, hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT>;
}

type Shared = Rc<RefCell<dyn Handler>>;

thread_local! {
    static LATER: RefCell<VecDeque<Box<dyn FnOnce()>>> = RefCell::new(VecDeque::new());
    static MSG_WINDOW: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// Handlers borrowed right now on this thread (`Busy`).
    static BUSY: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Set when WM_NCCREATE took the handler `create` passed: from then on the window frees it.
    static CLAIMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Windows whose whole area is client area (no frame of Windows' own).
    static FULL_CLIENT: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

/// The window has no frame of Windows' own (`frameless`): its frame messages get that answer even when they arrive
/// while the window's handler is busy (a SetWindowPos it made). Given to DefWindowProc then, the window got a thick
/// frame back.
pub fn set_full_client(hwnd: HWND) {
    FULL_CLIENT.with(|f| f.borrow_mut().push(hwnd.0 as isize));
}

fn is_full_client(hwnd: HWND) -> bool {
    FULL_CLIENT.with(|f| f.borrow().contains(&(hwnd.0 as isize)))
}

/// What a window without a frame of Windows' own answers about its frame, whether or not its handler is busy:
/// - WM_NCCALCSIZE: all of the window is client area;
/// - WM_NCPAINT: nothing to paint;
/// - WM_NCACTIVATE: the activation goes on, but DefWindowProc mustn't repaint the frame (lParam -1). It painted
///   WS_THICKFRAME's thick frame, white, over the window's edges whenever the window lost or got the focus: opening
///   Settings from the search window showed it for as long as the search window took to hide.
fn frameless(hwnd: HWND, msg: u32, w: WPARAM) -> Option<LRESULT> {
    match msg {
        WM_NCCALCSIZE | WM_NCPAINT => Some(LRESULT(0)),
        // SAFETY: the default handling of our own window's message, with "don't repaint" for its lParam.
        WM_NCACTIVATE => Some(unsafe { DefWindowProcW(hwnd, msg, w, LPARAM(-1)) }),
        _ => None,
    }
}

/// A handler is borrowed while this lives: `later`'s work waits until none is. A message sent back to a window from
/// inside a handler (SetWindowPos, ShowWindow, SetFocus…) ends in a nested wndproc; work run there would find the
/// handler still borrowed and be lost.
pub struct Busy(());

impl Busy {
    pub fn new() -> Busy {
        BUSY.with(|b| b.set(b.get() + 1));
        Busy(())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.with(|b| b.set(b.get() - 1));
    }
}

/// Runs `f` on the UI thread once the current message is handled (no handler is borrowed then).
pub fn later(f: impl FnOnce() + 'static) {
    LATER.with(|q| q.borrow_mut().push_back(Box::new(f)));
}

fn run_later() {
    if BUSY.with(|b| b.get()) > 0 {
        return;
    }
    loop {
        let next = LATER.with(|q| q.borrow_mut().pop_front());
        match next {
            Some(f) => f(),
            None => break,
        }
    }
}

static INVOKE: Mutex<Vec<Box<dyn FnOnce() + Send>>> = Mutex::new(Vec::new());
static MSG_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
pub const WM_INVOKE: u32 = WM_APP + 0x100;

/// Runs `f` on the UI thread (from any thread).
pub fn invoke(f: impl FnOnce() + Send + 'static) {
    INVOKE.lock().unwrap().push(Box::new(f));
    let h = MSG_HWND.load(std::sync::atomic::Ordering::SeqCst);
    if h != 0 {
        // SAFETY: posting to our own message window.
        unsafe {
            let _ = PostMessageW(Some(HWND(h as _)), WM_INVOKE, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn instance() -> HINSTANCE {
    // SAFETY: the module handle of this executable.
    unsafe { GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default() }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: the user data is the Box<Shared> placed at WM_NCCREATE, freed at WM_NCDESTROY.
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(l.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            CLAIMED.with(|c| c.set(true));
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Shared;
        let mut result = if is_full_client(hwnd) { frameless(hwnd, msg, w) } else { None };
        if result.is_none() && !ptr.is_null() {
            let shared: Shared = (*ptr).clone();
            if let Ok(mut h) = shared.try_borrow_mut() {
                let _busy = Busy::new();
                result = h.message(hwnd, msg, w, l);
            }
        }
        if msg == WM_NCDESTROY && !ptr.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            FULL_CLIENT.with(|f| f.borrow_mut().retain(|&h| h != hwnd.0 as isize));
            drop(Box::from_raw(ptr));
        }
        let r = result.unwrap_or_else(|| DefWindowProcW(hwnd, msg, w, l));
        run_later();
        r
    }
}

type AppMessages = fn(HWND, u32, WPARAM, LPARAM) -> Option<LRESULT>;

thread_local! {
    static APP_MESSAGES: std::cell::Cell<Option<AppMessages>> = const { std::cell::Cell::new(None) };
}

/// Where the app's own messages go (hotkeys, the tray icon, Windows' broadcasts).
pub fn on_app_message(f: AppMessages) {
    APP_MESSAGES.with(|c| c.set(Some(f)));
}

unsafe extern "system" fn msg_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_INVOKE {
        let work: Vec<Box<dyn FnOnce() + Send>> = std::mem::take(&mut *INVOKE.lock().unwrap());
        for f in work {
            f();
            run_later();
        }
        return LRESULT(0);
    }
    if let Some(f) = APP_MESSAGES.with(|c| c.get())
        && let Some(r) = f(hwnd, msg, w, l)
    {
        run_later();
        return r;
    }
    // SAFETY: default handling.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

pub fn register_class(name: PCWSTR, background: bool) {
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(wndproc),
        hInstance: instance(),
        // SAFETY: a system cursor.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        hbrBackground: if background { windows::Win32::Graphics::Gdi::HBRUSH::default() } else { Default::default() },
        lpszClassName: name,
        ..Default::default()
    };
    // SAFETY: registering a class with a static name.
    unsafe {
        RegisterClassExW(&wc);
    }
}

/// The hidden window `invoke` posts to, which also gets the tray icon's and the hotkey's messages and Windows'
/// broadcasts (a message-only window wouldn't get those). Created once, on the UI thread.
pub fn init_message_window() -> HWND {
    let name = w!("NuiterMessages");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(msg_wndproc),
        hInstance: instance(),
        lpszClassName: name,
        ..Default::default()
    };
    // SAFETY: a message-only window owned by this thread.
    let hwnd = unsafe {
        RegisterClassExW(&wc);
        CreateWindowExW(WS_EX_TOOLWINDOW, name, w!("Nuiter"), WS_POPUP, 0, 0, 0, 0, None, None, Some(instance()), None)
            .unwrap_or_default()
    };
    MSG_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::SeqCst);
    MSG_WINDOW.with(|m| m.set(hwnd.0 as isize));
    hwnd
}

/// Creates a window whose messages go to `handler`.
pub fn create(class: PCWSTR, title: &str, style: WINDOW_STYLE, ex: WINDOW_EX_STYLE, r: RECT, handler: Shared) -> HWND {
    let boxed = Box::into_raw(Box::new(handler));
    CLAIMED.with(|c| c.set(false));
    // SAFETY: the box is owned by the window from WM_NCCREATE on (it frees it at WM_NCDESTROY, even when creation fails
    // after that); before, it's still ours.
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            class,
            &HSTRING::from(title),
            style,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            None,
            None,
            Some(instance()),
            Some(boxed as *const _),
        )
        .unwrap_or_default()
    };
    if !CLAIMED.with(|c| c.get()) {
        // SAFETY: no window ever saw the box.
        drop(unsafe { Box::from_raw(boxed) });
    }
    hwnd
}

pub fn dpi_of(hwnd: HWND) -> f32 {
    // SAFETY: plain call.
    let d = unsafe { GetDpiForWindow(hwnd) };
    if d == 0 { 96.0 } else { d as f32 }
}

pub fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    // SAFETY: valid out pointer.
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    (r.right - r.left, r.bottom - r.top)
}

pub fn invalidate(hwnd: HWND) {
    // SAFETY: plain call.
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
    }
}

pub fn cursor() -> POINT {
    let mut p = POINT::default();
    // SAFETY: valid out pointer.
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
}

/// The work area (screen minus taskbars) of the monitor a point or window is on.
pub fn work_area_at(p: POINT) -> RECT {
    // SAFETY: plain calls with a struct carrying its size.
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST), &mut mi);
        mi.rcWork
    }
}

pub fn work_area_of(hwnd: HWND) -> RECT {
    // SAFETY: as above.
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        mi.rcWork
    }
}

pub fn monitor_rect_of(hwnd: HWND) -> RECT {
    // SAFETY: as above.
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        mi.rcMonitor
    }
}

/// Mouse coordinates of a mouse message, in physical pixels of the client area.
pub fn mouse_xy(l: LPARAM) -> (i32, i32) {
    ((l.0 & 0xFFFF) as i16 as i32, ((l.0 >> 16) & 0xFFFF) as i16 as i32)
}

pub fn run_message_loop() {
    let mut msg = MSG::default();
    // SAFETY: the standard loop.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
