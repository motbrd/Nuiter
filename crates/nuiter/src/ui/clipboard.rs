//! The clipboard: text, and files (as Explorer copies them, CF_HDROP).

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GHND, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};

fn open() -> bool {
    // Another program may hold it for a moment.
    for _ in 0..10 {
        // SAFETY: plain call; closed by close().
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

fn close() {
    // SAFETY: matches open().
    unsafe {
        let _ = CloseClipboard();
    }
}

fn global(bytes: &[u8]) -> Option<HGLOBAL> {
    // SAFETY: a movable block of the right size, filled while locked.
    unsafe {
        let h = GlobalAlloc(GHND, bytes.len()).ok()?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

/// Hands the block to the clipboard, which owns it from then on; if it won't take it, the block is freed here.
fn put(format: u32, h: HGLOBAL) {
    // SAFETY: the block is ours until SetClipboardData succeeds.
    unsafe {
        if SetClipboardData(format, Some(HANDLE(h.0))).is_err() {
            let _ = GlobalFree(Some(h));
        }
    }
}

pub fn set_text(text: &str) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_le_bytes()).collect();
    if !open() {
        return;
    }
    // SAFETY: the clipboard owns the block once set.
    unsafe {
        let _ = EmptyClipboard();
        if let Some(h) = global(&bytes) {
            put(CF_UNICODETEXT.0 as u32, h);
        }
    }
    close();
}

pub fn get_text() -> Option<String> {
    if !open() {
        return None;
    }
    // SAFETY: the block is read while locked; the clipboard keeps owning it.
    let r = unsafe {
        (|| {
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if p.is_null() {
                return None;
            }
            let max = GlobalSize(g) / 2;
            let mut n = 0;
            while n < max && *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(g);
            Some(s)
        })()
    };
    close();
    r
}

/// Files on the clipboard, as Explorer's "Copy" puts them: paste into a folder, a mail or a chat.
pub fn set_files(paths: &[String]) {
    // DROPFILES: offset of the list (20), a point, fNC, fWide; then the paths, each null-terminated, and one more null.
    let mut bytes: Vec<u8> = vec![];
    bytes.extend_from_slice(&20u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 8]);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    for p in paths {
        for u in p.encode_utf16().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    if !open() {
        return;
    }
    // SAFETY: as in set_text.
    unsafe {
        let _ = EmptyClipboard();
        if let Some(h) = global(&bytes) {
            put(CF_HDROP.0 as u32, h);
        }
    }
    close();
}
