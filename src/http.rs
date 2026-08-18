//! Minimal blocking HTTP(S) GET over WinHTTP.
//!
//! Shared by the weather fetcher (HTTPS to open-meteo.com) and the indoor
//! air monitor (plain HTTP to a box on the LAN). Both callers own a polling
//! thread and are happy to block, so there is no async machinery here — and
//! no HTTP crate, which keeps the binary at its ~19 MB.

use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
};

/// GET `path` from `host:port`; returns the response body.
///
/// Timeouts are deliberately short: an unplugged air monitor should turn
/// into a "no reading" row within seconds, not sit on WinHTTP's 60 s
/// connect default while the popup shows stale numbers.
pub fn get(host: &str, port: u16, path: &str, secure: bool) -> Option<Vec<u8>> {
    unsafe {
        let host16: Vec<u16> = host.encode_utf16().chain(std::iter::once(0)).collect();
        let path16: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let session = WinHttpOpen(
            w!("optim-bar"),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            return None;
        }
        let _ = WinHttpSetTimeouts(session, 5_000, 5_000, 10_000, 10_000);
        let mut out = None;
        let conn = WinHttpConnect(session, PCWSTR(host16.as_ptr()), port, 0);
        if !conn.is_null() {
            let req = WinHttpOpenRequest(
                conn,
                w!("GET"),
                PCWSTR(path16.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null_mut(),
                WINHTTP_OPEN_REQUEST_FLAGS(if secure { WINHTTP_FLAG_SECURE.0 } else { 0 }),
            );
            if !req.is_null() {
                if WinHttpSendRequest(req, None, None, 0, 0, 0).is_ok()
                    && WinHttpReceiveResponse(req, std::ptr::null_mut()).is_ok()
                {
                    let mut body = Vec::new();
                    loop {
                        let mut avail = 0u32;
                        if WinHttpQueryDataAvailable(req, &mut avail).is_err() || avail == 0 {
                            break;
                        }
                        let start = body.len();
                        body.resize(start + avail as usize, 0);
                        let mut read = 0u32;
                        if WinHttpReadData(req, body[start..].as_mut_ptr() as _, avail, &mut read)
                            .is_err()
                        {
                            break;
                        }
                        body.truncate(start + read as usize);
                        if read == 0 {
                            break;
                        }
                    }
                    if !body.is_empty() {
                        out = Some(body);
                    }
                }
                let _ = WinHttpCloseHandle(req);
            }
            let _ = WinHttpCloseHandle(conn);
        }
        let _ = WinHttpCloseHandle(session);
        out
    }
}
