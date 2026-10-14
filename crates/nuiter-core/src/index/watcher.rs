//! A folder tree's changes, from ReadDirectoryChangesW on a thread of its own (overlapped, so it stops at once).

use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::HSTRING;

use crate::paths::Log;

pub enum WatchEvent {
    /// Created, changed or deleted.
    Touched(String),
    Renamed { from: String, to: String },
    /// Changes were lost (the buffer overflowed): everything must be looked at again.
    Overflow,
}

pub struct Watcher {
    stop: HANDLE,
    thread: Option<JoinHandle<()>>,
}

// SAFETY: the event handle is only signalled and closed, from any thread.
unsafe impl Send for Watcher {}
// SAFETY: as for Send: `&Watcher` allows nothing but dropping through the owner.
unsafe impl Sync for Watcher {}

impl Watcher {
    pub fn start(root: &str, on_event: impl Fn(WatchEvent) + Send + 'static) -> anyhow::Result<Watcher> {
        // SAFETY: plain Win32 calls; handles are closed by the thread or by Drop.
        let (dir, stop, event) = unsafe {
            let dir = CreateFileW(
                &HSTRING::from(root),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )?;
            let stop = CreateEventW(None, true, false, None)?;
            let event = CreateEventW(None, true, false, None)?;
            (dir, stop, event)
        };
        let root = root.trim_end_matches('\\').to_string();
        let (dir_raw, stop_raw, event_raw) = (dir.0 as usize, stop.0 as usize, event.0 as usize);
        let spawned = std::thread::Builder::new().name("FolderWatcher".into()).stack_size(256 << 10).spawn(move || {
            let (dir, stop, event) = (HANDLE(dir_raw as _), HANDLE(stop_raw as _), HANDLE(event_raw as _));
            // DWORD-aligned, as the API requires: 64 KB.
            let mut buf = vec![0u32; 16 * 1024];
            loop {
                let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
                // SAFETY: the buffer and OVERLAPPED outlive the operation (it's waited for or cancelled below).
                let started = unsafe {
                    ReadDirectoryChangesW(
                        dir,
                        buf.as_mut_ptr() as *mut _,
                        (buf.len() * 4) as u32,
                        true,
                        FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE,
                        None,
                        Some(&mut ov),
                        None,
                    )
                };
                if let Err(e) = started {
                    Log::warn(format!("Watching {root} stopped: {e}"));
                    break;
                }
                // SAFETY: waits on two live events.
                let w = unsafe { WaitForMultipleObjects(&[event, stop], false, INFINITE) };
                let mut bytes = 0u32;
                if w != WAIT_OBJECT_0 {
                    // SAFETY: cancels and waits for our own pending operation before its buffer goes.
                    unsafe {
                        let _ = CancelIoEx(dir, Some(&ov));
                        let _ = GetOverlappedResult(dir, &ov, &mut bytes, true);
                    }
                    break;
                }
                // SAFETY: the operation completed.
                let done = unsafe { GetOverlappedResult(dir, &ov, &mut bytes, false) };
                // SAFETY: resetting our own event.
                unsafe {
                    let _ = windows::Win32::System::Threading::ResetEvent(event);
                }
                if done.is_err() || bytes == 0 {
                    on_event(WatchEvent::Overflow);
                    if done.is_err() {
                        // The folder went away (deleted, drive unplugged): stop; the indexer notices the drive.
                        break;
                    }
                    continue;
                }
                let base = buf.as_ptr() as *const u8;
                let mut offset = 0usize;
                let mut renamed_from: Option<String> = None;
                loop {
                    // SAFETY: records lie within the bytes the call reported; each carries its length and next offset.
                    let info = unsafe { &*(base.add(offset) as *const FILE_NOTIFY_INFORMATION) };
                    let name_len = info.FileNameLength as usize / 2;
                    // SAFETY: the name follows the record, FileNameLength bytes long.
                    let name = unsafe { std::slice::from_raw_parts(info.FileName.as_ptr(), name_len) };
                    let path = format!("{root}\\{}", String::from_utf16_lossy(name));
                    match info.Action {
                        FILE_ACTION_RENAMED_OLD_NAME => renamed_from = Some(path),
                        FILE_ACTION_RENAMED_NEW_NAME => match renamed_from.take() {
                            Some(from) => on_event(WatchEvent::Renamed { from, to: path }),
                            None => on_event(WatchEvent::Touched(path)),
                        },
                        FILE_ACTION_ADDED | FILE_ACTION_REMOVED | FILE_ACTION_MODIFIED => on_event(WatchEvent::Touched(path)),
                        _ => {}
                    }
                    if info.NextEntryOffset == 0 {
                        break;
                    }
                    offset += info.NextEntryOffset as usize;
                    if offset >= bytes as usize {
                        break;
                    }
                }
                if let Some(from) = renamed_from {
                    on_event(WatchEvent::Touched(from));
                }
            }
            // SAFETY: closing the handles this thread owns.
            unsafe {
                let _ = CloseHandle(dir);
                let _ = CloseHandle(event);
            }
        });
        match spawned {
            Ok(thread) => Ok(Watcher { stop, thread: Some(thread) }),
            Err(e) => {
                // SAFETY: no thread took the handles: they're closed here.
                unsafe {
                    let _ = CloseHandle(dir);
                    let _ = CloseHandle(event);
                    let _ = CloseHandle(stop);
                }
                Err(e.into())
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: signalling and closing our own event after the thread is done with it.
        unsafe {
            let _ = SetEvent(self.stop);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // SAFETY: the thread that waited on it is gone.
        unsafe {
            let _ = CloseHandle(self.stop);
        }
    }
}
