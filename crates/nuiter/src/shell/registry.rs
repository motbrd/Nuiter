//! Registry settings Nuiter touches: starting with Windows, and the taskbar's search box mode.

use nuiter_core::Log;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_DWORD, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW};
use windows::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
use windows::core::{HSTRING, w};

const RUN_KEY: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

/// Starts Nuiter when the user signs in (in the background), or stops doing so.
pub fn apply_autostart(enabled: bool) {
    // An isolated run (NUITER_HOME: a test instance next to the real one) must not take over the real login item.
    if std::env::var_os("NUITER_HOME").is_some_and(|v| !v.is_empty()) {
        return;
    }
    // SAFETY: plain registry calls with valid strings.
    let r = unsafe {
        if enabled {
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            let value: Vec<u16> = format!("\"{exe}\" --background").encode_utf16().chain(std::iter::once(0)).collect();
            RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Nuiter"), REG_SZ.0, Some(value.as_ptr() as *const _), (value.len() * 2) as u32)
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Nuiter"));
            if r.0 == 2 { windows::Win32::Foundation::WIN32_ERROR(0) } else { r } // not there: nothing to do
        }
    };
    if r.is_err() {
        Log::warn(format!("Startup registration failed: {r:?}"));
    }
}

const SEARCH_KEY: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Search");

/// The taskbar search display mode (0 hidden, 1 icon, 2 box, 3 icon and label).
pub fn taskbar_search_mode() -> u32 {
    let mut v = 2u32;
    let mut size = 4u32;
    // SAFETY: reads a DWORD into a u32.
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, SEARCH_KEY, w!("SearchboxTaskbarMode"), RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut size))
    };
    if r.is_ok() { v } else { 2 }
}

/// Changes the mode and tells Explorer (as Settings › Taskbar › Search does).
pub fn set_taskbar_search_mode(mode: u32) {
    // SAFETY: plain registry and broadcast calls.
    unsafe {
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, SEARCH_KEY, w!("SearchboxTaskbarMode"), REG_DWORD.0, Some(&mode as *const u32 as *const _), 4);
        let area = HSTRING::from("TraySettings");
        let _ = SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, WPARAM(0), LPARAM(area.as_ptr() as isize), SMTO_ABORTIFHUNG, 1000, None);
    }
}
