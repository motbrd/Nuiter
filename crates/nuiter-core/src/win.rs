//! Small Windows helpers used across the engine.

use std::os::windows::fs::MetadataExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::GetFullPathNameW;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::{GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
use windows::core::{HSTRING, PCWSTR};

/// Ticks (.NET's 100 ns since 0001-01-01) at the FILETIME epoch (1601-01-01).
const FILETIME_EPOCH_TICKS: i64 = 504_911_232_000_000_000;

/// "2026-10-13 18:04:05.123", local time.
pub fn local_time_text() -> String {
    // SAFETY: plain Win32 call without arguments.
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// The local time of day as minutes since midnight.
pub fn local_minutes() -> u32 {
    // SAFETY: plain Win32 call without arguments.
    let t = unsafe { GetLocalTime() };
    t.wHour as u32 * 60 + t.wMinute as u32
}

/// The absolute form of a path, as GetFullPathNameW makes it.
pub fn full_path(p: &str) -> Option<String> {
    if p.is_empty() {
        return None;
    }
    let h = HSTRING::from(p);
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { GetFullPathNameW(&h, Some(&mut buf), None) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        buf.resize(n + 1, 0);
    }
}

/// A file's modification time in .NET ticks (what the index stores) and its size and attributes.
pub struct FileStat {
    pub size: i64,
    pub mtime: i64,
    pub attributes: u32,
    pub is_dir: bool,
}

pub fn stat(path: impl AsRef<Path>) -> Option<FileStat> {
    let m = std::fs::metadata(path).ok()?;
    Some(FileStat {
        size: m.file_size() as i64,
        mtime: m.last_write_time() as i64 + FILETIME_EPOCH_TICKS,
        attributes: m.file_attributes(),
        is_dir: m.is_dir(),
    })
}

pub fn stat_of(m: &std::fs::Metadata) -> FileStat {
    FileStat {
        size: m.file_size() as i64,
        mtime: m.last_write_time() as i64 + FILETIME_EPOCH_TICKS,
        attributes: m.file_attributes(),
        is_dir: m.is_dir(),
    }
}

/// .NET ticks (UTC) → (year, month, day, hour, minute) in local time.
pub fn ticks_to_local(ticks: i64) -> Option<(u16, u16, u16, u16, u16)> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ft = (ticks - FILETIME_EPOCH_TICKS).max(0) as u64;
    let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: valid pointers to local structs.
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some((local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute))
}

/// Seconds since 1970 (UTC).
pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Cryptographically random bytes.
pub fn random_bytes(n: usize) -> Vec<u8> {
    use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
    let mut buf = vec![0u8; n];
    // SAFETY: the buffer is valid for its length.
    let status = unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status.is_err() {
        // Never expected; still never a constant key.
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8 ^ (i as u8).wrapping_mul(151);
        }
    }
    buf
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Memory this process has committed for itself (private bytes).
pub fn private_bytes() -> u64 {
    // SAFETY: GetCurrentProcess is a pseudo handle.
    process_private_bytes(unsafe { GetCurrentProcess() })
}

pub fn process_private_bytes(process: HANDLE) -> u64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: the struct is passed with its size.
    let ok = unsafe {
        GetProcessMemoryInfo(
            process,
            &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if ok.is_ok() { c.PrivateUsage as u64 } else { 0 }
}

/// Working set of this process.
pub fn working_set() -> u64 {
    let mut c = PROCESS_MEMORY_COUNTERS::default();
    // SAFETY: the struct is passed with its size.
    let ok = unsafe {
        GetProcessMemoryInfo(GetCurrentProcess(), &mut c, std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32)
    };
    if ok.is_ok() { c.WorkingSetSize as u64 } else { 0 }
}

pub fn process_handle_count(process: HANDLE) -> u32 {
    let mut n = 0u32;
    // SAFETY: valid out pointer.
    let _ = unsafe { GetProcessHandleCount(process, &mut n) };
    n
}

/// The process listening on 127.0.0.1:`port` (TCP over IPv4), if any.
pub fn tcp_listener_pid(port: u16) -> Option<u32> {
    use windows::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER};
    const AF_INET: u32 = 2;
    let mut size = 0u32;
    // The table can grow between the call that sizes it and the one that fills it: a few tries.
    for _ in 0..4 {
        // u32s, so the rows are aligned as the API writes them.
        let mut buf = vec![0u32; (size as usize).div_ceil(4).max(1)];
        // SAFETY: the buffer holds `size` bytes; the call says how many it needs when that's too few.
        let r = unsafe { GetExtendedTcpTable(Some(buf.as_mut_ptr().cast()), &mut size, false, AF_INET, TCP_TABLE_OWNER_PID_LISTENER, 0) };
        if r == 122 {
            continue; // ERROR_INSUFFICIENT_BUFFER: `size` is the size needed now
        }
        if r != 0 {
            return None;
        }
        let count = buf[0] as usize;
        let row_words = std::mem::size_of::<MIB_TCPROW_OWNER_PID>() / 4;
        if 1 + count * row_words > buf.len() {
            return None;
        }
        // SAFETY: `count` rows follow the count, inside the buffer (checked above).
        let rows = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(1).cast::<MIB_TCPROW_OWNER_PID>(), count) };
        // Who answers on 127.0.0.1: a listener on that address or on all of them (0.0.0.0), not one on another address
        // that only shares the port number.
        const LOOPBACK: u32 = u32::from_ne_bytes([127, 0, 0, 1]);
        return rows
            .iter()
            .find(|r| u16::from_be(r.dwLocalPort as u16) == port && (r.dwLocalAddr == LOOPBACK || r.dwLocalAddr == 0))
            .map(|r| r.dwOwningPid);
    }
    None
}

/// The PC's physical memory.
pub fn total_memory() -> u64 {
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: the struct carries its size.
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullTotalPhys } else { 8 << 30 }
}

/// COM for this thread: multithreaded (decoders, WinRT) or single-threaded (shell, IFilter).
pub fn init_com_mta() {
    // SAFETY: initialising COM on the calling thread; a second call is harmless.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

pub fn init_com_sta() {
    // SAFETY: as above.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// A null-terminated UTF-16 copy of a string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(w: &[u16]) -> PCWSTR {
    PCWSTR(w.as_ptr())
}

/// A handle closed when dropped.
pub struct OwnedHandle(pub HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            // SAFETY: the handle is owned.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as for Send: a shared reference only lends the handle to Win32 calls, which take it from any thread.
unsafe impl Sync for OwnedHandle {}

/// The ANSI code page's text for bytes that aren't UTF-8 (MultiByteToWideChar), or Latin-1 when that fails.
pub fn decode_code_page(bytes: &[u8], code_page: u32) -> String {
    use windows::Win32::Globalization::{MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
    if bytes.is_empty() {
        return String::new();
    }
    // SAFETY: buffers are sized by the first call.
    unsafe {
        let n = MultiByteToWideChar(code_page, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
        if n > 0 {
            let mut buf = vec![0u16; n as usize];
            let m = MultiByteToWideChar(code_page, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, Some(&mut buf));
            if m > 0 {
                return String::from_utf16_lossy(&buf[..m as usize]);
            }
        }
    }
    bytes.iter().map(|&b| b as char).collect()
}

/// The system's ANSI code page.
pub fn ansi_code_page() -> u32 {
    // SAFETY: plain Win32 call.
    unsafe { windows::Win32::Globalization::GetACP() }
}

/// SHA-256 of some bytes, as lowercase hex (names of kernel objects, keys).
pub fn hash_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex(&sha2::Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_process_listening_on_a_port_is_known() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert_eq!(super::tcp_listener_pid(port), Some(std::process::id()));
        drop(l);
        assert_eq!(super::tcp_listener_pid(port), None);
    }
}
