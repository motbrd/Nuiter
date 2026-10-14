//! The notification-area icon: its tooltip, its notifications, and the messages its clicks send.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{HICON, RegisterWindowMessageW, WM_APP};
use windows::core::w;

pub const WM_TRAY: u32 = WM_APP + 1;
const ID: u32 = 1;

fn copy(dst: &mut [u16], text: &str) {
    let w: Vec<u16> = text.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: ID, ..Default::default() }
}

/// The message Explorer broadcasts when it (re)starts: the icon has to be added again.
pub fn taskbar_created_message() -> u32 {
    // SAFETY: plain call.
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

pub fn add(hwnd: HWND, icon: HICON, tip: &str) {
    let mut d = data(hwnd);
    d.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE | NIF_SHOWTIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = icon;
    copy(&mut d.szTip, tip);
    // SAFETY: a well-formed NOTIFYICONDATA.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &d);
        d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
    }
}

pub fn set_tip(hwnd: HWND, tip: &str) {
    let mut d = data(hwnd);
    d.uFlags = NIF_TIP | NIF_SHOWTIP;
    copy(&mut d.szTip, tip);
    // SAFETY: as above.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

pub fn set_icon(hwnd: HWND, icon: HICON) {
    let mut d = data(hwnd);
    d.uFlags = NIF_ICON;
    d.hIcon = icon;
    // SAFETY: as above.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

/// A notification from the icon; `warning` shows the warning symbol.
pub fn notify(hwnd: HWND, text: &str, warning: bool) {
    let mut d = data(hwnd);
    d.uFlags = NIF_INFO;
    copy(&mut d.szInfoTitle, "Nuiter");
    copy(&mut d.szInfo, text);
    d.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
    // SAFETY: as above.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

pub fn remove(hwnd: HWND) {
    let d = data(hwnd);
    // SAFETY: as above.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &d);
    }
}
