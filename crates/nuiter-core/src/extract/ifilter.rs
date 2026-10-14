//! Text through the IFilter handlers that Windows Search uses (e.g. Office's filter for .doc and .msg). Runs on its own
//! single-threaded-apartment thread because many filters are apartment-threaded, with a time limit.

use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Storage::IndexServer::{CHUNK_TEXT, IFilter, LoadIFilter};
use windows::core::{HSTRING, IUnknown, Interface, PWSTR};

use crate::paths::Log;

const MAX_CHARS: usize = 4_000_000;

pub fn extract(path: &str, timeout_ms: u64) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    let owned = path.to_string();
    let spawned = std::thread::Builder::new().name("IFilter".into()).stack_size(1 << 20).spawn(move || {
        crate::win::init_com_sta();
        let r = extract_core(&owned);
        let _ = tx.send(r);
    });
    if spawned.is_err() {
        return None;
    }
    // A filter that hangs keeps its thread; the reader helper it runs in is ended by the host before long.
    match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            Log::warn(format!("IFilter failed for {path}: {e}"));
            None
        }
        Err(_) => None,
    }
}

fn extract_core(path: &str) -> windows::core::Result<Option<String>> {
    let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: LoadIFilter returns an owned IUnknown pointer on success.
    unsafe {
        if LoadIFilter(&HSTRING::from(path), None::<&IUnknown>, &mut raw).is_err() || raw.is_null() {
            return Ok(None);
        }
        let unk = IUnknown::from_raw(raw);
        let filter: IFilter = unk.cast()?;
        let mut flags = 0u32;
        // Canonical paragraphs, hard line breaks, canonical hyphens, canonical spaces.
        if filter.Init(1 | 2 | 4 | 8, &[], &mut flags) != 0 {
            return Ok(None);
        }
        let mut text: Vec<u16> = vec![];
        let mut buf = vec![0u16; 8192];
        while text.len() < MAX_CHARS {
            let mut stat = Default::default();
            let hr = filter.GetChunk(&mut stat);
            if hr == 0x80041700u32 as i32 {
                break; // FILTER_E_END_OF_CHUNKS
            }
            if hr < 0 {
                // FILTER_E_EMBEDDING_UNAVAILABLE / FILTER_E_LINK_UNAVAILABLE: an embedded object or a link with nothing
                // to read; the text goes on after it.
                if hr == 0x80041707u32 as i32 || hr == 0x80041708u32 as i32 {
                    continue;
                }
                break;
            }
            if stat.flags.0 & CHUNK_TEXT.0 == 0 {
                continue;
            }
            if stat.breakType.0 != 0 {
                text.push('\n' as u16);
            }
            loop {
                let mut len = buf.len() as u32;
                let hr = filter.GetText(&mut len, PWSTR(buf.as_mut_ptr()));
                if hr < 0 {
                    break; // FILTER_E_NO_MORE_TEXT and errors
                }
                text.extend_from_slice(&buf[..(len as usize).min(buf.len())]);
                if hr == 0x00041709 {
                    break; // FILTER_S_LAST_TEXT
                }
            }
        }
        Ok(Some(String::from_utf16_lossy(&text)))
    }
}
