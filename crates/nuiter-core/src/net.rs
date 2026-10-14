//! HTTPS GETs through WinHTTP: Windows' own TLS and certificate store, the system's proxy settings, redirects followed.
//! Used only for the model download and the update check.

use std::ffi::c_void;
use std::io::Read;

use anyhow::{Result, anyhow};
use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR};

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a WinHTTP handle this struct owns.
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

// SAFETY: WinHTTP handles may be used from any thread (one at a time, as here).
unsafe impl Send for Handle {}

/// A response being read: its status, length (when known) and body as a stream.
pub struct Response {
    pub status: u32,
    pub length: Option<u64>,
    request: Handle,
    _connect: Handle,
    _session: Handle,
}

impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = 0u32;
        // SAFETY: the buffer is valid for its length.
        unsafe { WinHttpReadData(self.request.0, buf.as_mut_ptr() as *mut c_void, buf.len().min(u32::MAX as usize) as u32, &mut n) }
            .map_err(std::io::Error::other)?;
        Ok(n as usize)
    }
}

/// GET `url` with extra `headers` ("Name: value" lines).
pub fn get(url: &str, headers: &[String]) -> Result<Response> {
    let agent = HSTRING::from(format!("Nuiter/{}", crate::VERSION));
    let wide: Vec<u16> = url.encode_utf16().collect();
    let mut host = vec![0u16; 512];
    let mut path = vec![0u16; 4096];
    let mut parts = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        lpszHostName: windows::core::PWSTR(host.as_mut_ptr()),
        dwHostNameLength: host.len() as u32,
        lpszUrlPath: windows::core::PWSTR(path.as_mut_ptr()),
        dwUrlPathLength: path.len() as u32,
        ..Default::default()
    };
    // SAFETY: the components point into buffers that live through the calls below.
    unsafe {
        WinHttpCrackUrl(&wide, 0, &mut parts).map_err(|e| anyhow!("bad URL {url}: {e}"))?;
        let host_name = String::from_utf16_lossy(&host[..parts.dwHostNameLength as usize]);
        // The path and the query string together.
        let object = String::from_utf16_lossy(std::slice::from_raw_parts(parts.lpszUrlPath.0, parts.dwUrlPathLength as usize));
        let query = if parts.lpszExtraInfo.0.is_null() {
            String::new()
        } else {
            String::from_utf16_lossy(std::slice::from_raw_parts(parts.lpszExtraInfo.0, parts.dwExtraInfoLength as usize))
        };
        let secure = parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;
        let session = Handle(WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if session.0.is_null() {
            return Err(anyhow!("WinHTTP isn't available: {}", windows::core::Error::from_thread()));
        }
        // Resolve, connect, send: 30 s each; a slow download may pause up to 5 minutes between packets.
        let _ = WinHttpSetTimeouts(session.0, 30_000, 30_000, 30_000, 300_000);
        let decompress = (WINHTTP_DECOMPRESSION_FLAG_GZIP | WINHTTP_DECOMPRESSION_FLAG_DEFLATE).to_le_bytes();
        let _ = WinHttpSetOption(Some(session.0), WINHTTP_OPTION_DECOMPRESSION, Some(&decompress));
        let connect = Handle(WinHttpConnect(session.0, &HSTRING::from(host_name), parts.nPort, 0));
        if connect.0.is_null() {
            return Err(anyhow!("cannot reach {url}: {}", windows::core::Error::from_thread()));
        }
        let flags = if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            &HSTRING::from("GET"),
            &HSTRING::from(format!("{object}{query}")),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        ));
        if request.0.is_null() {
            return Err(anyhow!("cannot ask {url}: {}", windows::core::Error::from_thread()));
        }
        if !headers.is_empty() {
            let h: Vec<u16> = headers.join("\r\n").encode_utf16().collect();
            WinHttpAddRequestHeaders(request.0, &h, WINHTTP_ADDREQ_FLAG_ADD)?;
        }
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|e| anyhow!("{url}: {}", e.message()))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| anyhow!("{url}: {}", e.message()))?;
        let status = query_number(request.0, WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        let length = query_number(request.0, WINHTTP_QUERY_CONTENT_LENGTH).map(|n| n as u64);
        Ok(Response { status, length, request, _connect: connect, _session: session })
    }
}

fn query_number(request: *mut c_void, what: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut size = 4u32;
    let mut index = 0u32;
    // SAFETY: the buffer is a u32, as WINHTTP_QUERY_FLAG_NUMBER asks.
    unsafe {
        WinHttpQueryHeaders(request, what | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut value as *mut u32 as *mut c_void), &mut size, &mut index).ok()?;
    }
    Some(value)
}

/// Whether Windows says the internet connection is paid for by the data (a phone's hotspot, a capped plan, roaming,
/// near or over its limit): a big download should be asked about then. False when it can't tell, or there's none.
pub fn metered() -> bool {
    use windows::Networking::Connectivity::{NetworkCostType, NetworkInformation};
    let cost = || -> windows::core::Result<bool> {
        let c = NetworkInformation::GetInternetConnectionProfile()?.GetConnectionCost()?;
        let kind = c.NetworkCostType()?;
        Ok(kind == NetworkCostType::Fixed || kind == NetworkCostType::Variable || c.Roaming()? || c.ApproachingDataLimit()? || c.OverDataLimit()?)
    };
    cost().unwrap_or(false)
}

/// A whole (small) response body as text.
pub fn get_text(url: &str, headers: &[String]) -> Result<(u32, String)> {
    let mut r = get(url, headers)?;
    let mut body = String::new();
    r.read_to_string(&mut body)?;
    Ok((r.status, body))
}
