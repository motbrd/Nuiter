//! Global shortcuts: Win+S (Windows Search) is taken over with a low-level keyboard hook on a thread of its own (a busy
//! UI thread can then never delay keyboard input system-wide), and an extra, user-chosen hotkey is registered with
//! RegisterHotKey.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use nuiter_core::Log;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const HOTKEY_ID: i32 = 0x4E55;

static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
static WIN_DOWN: AtomicBool = AtomicBool::new(false);

fn is_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: plain call.
    (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000) != 0
}

/// Whether Win is held now. The flag alone isn't enough: a release this hook never sees (Win+L locks the PC and the
/// release happens on the lock screen; a window of an elevated app has the focus) left it set, and every plain "s"
/// typed after was swallowed. The keyboard's own state has the last word, and clears the flag.
fn win_held() -> bool {
    if !WIN_DOWN.load(Ordering::SeqCst) {
        return false;
    }
    if is_down(VK_LWIN) || is_down(VK_RWIN) {
        return true;
    }
    WIN_DOWN.store(false, Ordering::SeqCst);
    false
}

unsafe extern "system" fn keyboard_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL, lParam is a KBDLLHOOKSTRUCT.
        let k = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
        let down = w.0 as u32 == WM_KEYDOWN || w.0 as u32 == WM_SYSKEYDOWN;
        if k.vkCode == VK_LWIN.0 as u32 || k.vkCode == VK_RWIN.0 as u32 {
            WIN_DOWN.store(down, Ordering::SeqCst);
        } else if down
            && win_held()
            && k.vkCode == b'S' as u32
            && !is_down(VK_SHIFT)
            && !is_down(VK_CONTROL)
            && !is_down(VK_MENU)
        {
            // Win+S: swallowed, and the shell is told a "combo" happened so releasing Win won't open Start.
            std::thread::spawn(send_dummy_key);
            crate::ui::window::invoke(crate::app::on_hotkey);
            return LRESULT(1);
        }
    }
    // SAFETY: passing the event on.
    unsafe { CallNextHookEx(None, code, w, l) }
}

/// A harmless key event, so that releasing Win after an intercepted Win+key doesn't open Start.
fn send_dummy_key() {
    const VK_NONAME: u16 = 0xE8; // unassigned
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(VK_NONAME), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: two well-formed inputs.
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

pub fn set_win_s(on: bool) {
    let running = HOOK_THREAD.load(Ordering::SeqCst);
    if on && running == 0 {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = std::thread::Builder::new().name("KeyboardHook".into()).stack_size(256 << 10).spawn(move || {
            // SAFETY: a hook with its own message loop on this thread.
            unsafe {
                HOOK_THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
                let module = GetModuleHandleW(None).ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
                let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
                if let Err(e) = &hook {
                    Log::warn(format!("Keyboard hook failed: {e}"));
                }
                let _ = tx.send(());
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
                if let Ok(h) = hook {
                    let _ = UnhookWindowsHookEx(h);
                }
                HOOK_THREAD.store(0, Ordering::SeqCst);
            }
        });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
    } else if !on && running != 0 {
        // SAFETY: asks the hook thread's loop to end.
        unsafe {
            let _ = PostThreadMessageW(running, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        for _ in 0..20 {
            if HOOK_THREAD.load(Ordering::SeqCst) == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Parses "Win+Alt+S", "Ctrl+Shift+F", "Ctrl+F12": the modifiers (MOD_*) and the key.
pub fn parse(spec: &str) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let mut vk = 0u32;
    for raw in spec.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match raw.to_lowercase().as_str() {
            "ctrl" | "control" => mods |= 0x2,
            "alt" => mods |= 0x1,
            "shift" => mods |= 0x4,
            "win" | "windows" => mods |= 0x8,
            k => vk = key_code(k)?,
        }
    }
    (vk != 0).then_some((mods, vk))
}

fn key_code(k: &str) -> Option<u32> {
    let b = k.as_bytes();
    if b.len() == 1 && (b[0].is_ascii_alphanumeric()) {
        return Some(b[0].to_ascii_uppercase() as u32);
    }
    if b.len() == 2 && b[0] == b'd' && b[1].is_ascii_digit() {
        return Some(b[1] as u32);
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=24).contains(&n)
    {
        return Some(VK_F1.0 as u32 + n - 1);
    }
    Some(match k {
        "space" => VK_SPACE.0,
        "enter" | "return" => VK_RETURN.0,
        "tab" => VK_TAB.0,
        "esc" | "escape" => VK_ESCAPE.0,
        "insert" | "ins" => VK_INSERT.0,
        "delete" | "del" => VK_DELETE.0,
        "home" => VK_HOME.0,
        "end" => VK_END.0,
        "pageup" | "prior" => VK_PRIOR.0,
        "pagedown" | "next" => VK_NEXT.0,
        "up" => VK_UP.0,
        "down" => VK_DOWN.0,
        "left" => VK_LEFT.0,
        "right" => VK_RIGHT.0,
        "oemplus" | "plus" => VK_OEM_PLUS.0,
        "oemminus" | "minus" => VK_OEM_MINUS.0,
        "oemperiod" | "period" => VK_OEM_PERIOD.0,
        "oemcomma" | "comma" => VK_OEM_COMMA.0,
        "oem3" | "oemtilde" | "backtick" => VK_OEM_3.0,
        _ => return None,
    } as u32)
}

pub fn set_hotkey(window: HWND, spec: &str) {
    // SAFETY: (un)registering a hotkey of our own window.
    unsafe {
        let _ = UnregisterHotKey(Some(window), HOTKEY_ID);
        if spec.trim().is_empty() {
            return;
        }
        let Some((mods, vk)) = parse(spec) else {
            Log::warn(format!("Cannot parse hotkey '{spec}'"));
            return;
        };
        if RegisterHotKey(Some(window), HOTKEY_ID, HOT_KEY_MODIFIERS(mods | 0x4000 /* MOD_NOREPEAT */), vk).is_err() {
            Log::warn(format!("Hotkey '{spec}' is already used by another application"));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shortcuts_are_parsed() {
        assert_eq!(super::parse("Win+Alt+S"), Some((0x8 | 0x1, b'S' as u32)));
        assert_eq!(super::parse("Ctrl+Shift+F"), Some((0x2 | 0x4, b'F' as u32)));
        assert_eq!(super::parse("Ctrl+1"), Some((0x2, b'1' as u32)));
        assert!(super::parse("Ctrl+").is_none());
        assert!(super::parse("Win+Banana").is_none());
    }
}
