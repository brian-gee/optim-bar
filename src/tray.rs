//! System-tray host: a window classed "Shell_TrayWnd" that receives the
//! Shell_NotifyIcon WM_COPYDATA protocol, replacing explorer's taskbar as
//! the tray. Created topmost so FindWindow("Shell_TrayWnd") resolves to us;
//! explorer's own taskbar windows are hidden while we run.
//!
//! Safety valve: `optim-bar.exe --restore-tray` (or graceful exit) un-hides
//! explorer's taskbar and broadcasts TaskbarCreated so icons return to it.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetMessageW,
    GetWindowThreadProcessId, PostMessageW, RegisterClassW, RegisterWindowMessageW,
    SendNotifyMessageW, ShowWindow, TranslateMessage, HWND_BROADCAST, MSG, SW_HIDE, SW_SHOW,
    WM_COPYDATA, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::widgets::tasks::icon_pixels;
use windows::Win32::UI::WindowsAndMessaging::HICON;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETVERSION: u32 = 4;
const NIF_MESSAGE: u32 = 0x01;
const NIF_ICON: u32 = 0x02;
const NIS_HIDDEN: u32 = 0x01;

#[derive(Clone)]
pub struct TrayIcon {
    pub owner: u32,     // HWND of the owning app (32-bit in the protocol)
    pub uid: u32,
    pub callback: u32,  // uCallbackMessage
    pub version: u32,
    pub hidden: bool,
    pub pixels: Option<Arc<Vec<u8>>>, // 32x32 premultiplied BGRA
}

pub type TrayState = Arc<Mutex<Vec<TrayIcon>>>;

static STATE: OnceLock<TrayState> = OnceLock::new();
static HOST_HWND: AtomicIsize = AtomicIsize::new(0);
static HOST_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn state() -> TrayState {
    STATE.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone()
}

/// Reads a u32 at `off` from the WM_COPYDATA payload.
fn u32_at(data: &[u8], off: usize) -> u32 {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0)
}

/// Handles one SHELLTRAYDATA message (magic already verified).
/// NOTIFYICONDATA arrives in its 32-bit layout at offset 8:
///   0 cbSize, 4 hWnd, 8 uID, 12 uFlags, 16 uCallbackMessage, 20 hIcon,
///   24 szTip[128], 280 dwState, 284 dwStateMask, ... 800 uVersion/uTimeout
fn on_tray_data(data: &[u8]) -> bool {
    let message = u32_at(data, 4);
    let nid = &data[8..];
    let owner = u32_at(nid, 4);
    let uid = u32_at(nid, 8);
    let flags = u32_at(nid, 12);
    let callback = u32_at(nid, 16);
    let hicon = u32_at(nid, 20);
    let dw_state = u32_at(nid, 280);
    let dw_state_mask = u32_at(nid, 284);
    let version = u32_at(nid, 800);

    let state = state();
    let mut icons = state.lock().unwrap();
    let existing = icons.iter_mut().find(|i| i.owner == owner && i.uid == uid);

    match message {
        NIM_ADD | NIM_MODIFY => {
            let pixels = if flags & NIF_ICON != 0 && hicon != 0 {
                icon_pixels(HICON(hicon as usize as *mut _)).map(Arc::new)
            } else {
                None
            };
            match existing {
                Some(icon) => {
                    if flags & NIF_MESSAGE != 0 {
                        icon.callback = callback;
                    }
                    if pixels.is_some() {
                        icon.pixels = pixels;
                    }
                    if dw_state_mask & NIS_HIDDEN != 0 {
                        icon.hidden = dw_state & NIS_HIDDEN != 0;
                    }
                }
                None => {
                    if message == NIM_ADD {
                        icons.push(TrayIcon {
                            owner,
                            uid,
                            callback: if flags & NIF_MESSAGE != 0 { callback } else { 0 },
                            version: 0,
                            hidden: dw_state_mask & NIS_HIDDEN != 0
                                && dw_state & NIS_HIDDEN != 0,
                            pixels,
                        });
                    } else {
                        return false; // MODIFY for unknown icon
                    }
                }
            }
            true
        }
        NIM_DELETE => {
            let before = icons.len();
            icons.retain(|i| !(i.owner == owner && i.uid == uid));
            icons.len() != before
        }
        NIM_SETVERSION => {
            if let Some(icon) = existing {
                icon.version = version;
                true
            } else {
                false
            }
        }
        _ => true, // NIM_SETFOCUS etc: acknowledge
    }
}

extern "system" fn tray_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == windows::Win32::UI::WindowsAndMessaging::WM_TIMER {
            // Explorer re-shows its taskbars on some shell events; re-assert.
            for h in explorer_trays() {
                let hw = HWND(h as *mut _);
                if windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hw).as_bool() {
                    let _ = ShowWindow(hw, SW_HIDE);
                }
            }
            return LRESULT(0);
        }
        if msg == WM_COPYDATA {
            #[repr(C)]
            struct Cds {
                dw_data: usize,
                cb_data: u32,
                lp_data: *const u8,
            }
            let cds = &*(lparam.0 as *const Cds);
            if !cds.lp_data.is_null() && cds.cb_data >= 8 {
                let data = std::slice::from_raw_parts(cds.lp_data, cds.cb_data as usize);
                // dwData 1 = tray data; magic 0x34753423 at offset 0
                if cds.dw_data == 1 && u32_at(data, 0) == 0x34753423 {
                    return LRESULT(on_tray_data(data) as i32 as isize);
                }
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

struct ExplorerTrays(Vec<isize>);

unsafe extern "system" fn find_explorer_trays(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let out = &mut *(lparam.0 as *mut ExplorerTrays);
    let mut class = [0u16; 64];
    let n = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut class);
    let name = String::from_utf16_lossy(&class[..n.max(0) as usize]);
    if name == "Shell_TrayWnd" || name == "Shell_SecondaryTrayWnd" {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != GetCurrentProcessId() {
            out.0.push(hwnd.0 as isize);
        }
    }
    true.into()
}

fn explorer_trays() -> Vec<isize> {
    let mut found = ExplorerTrays(Vec::new());
    unsafe {
        let _ = EnumWindows(
            Some(find_explorer_trays),
            LPARAM(&mut found as *mut _ as isize),
        );
    }
    found.0
}

fn broadcast_taskbar_created() {
    unsafe {
        let msg = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = SendNotifyMessageW(HWND_BROADCAST, msg, WPARAM(0), LPARAM(0));
    }
}

/// Un-hides explorer's taskbar windows and tells apps to re-register there.
pub fn restore_explorer_tray() {
    unsafe {
        for h in explorer_trays() {
            let _ = ShowWindow(HWND(h as *mut _), SW_SHOW);
        }
        let host = HOST_HWND.load(Ordering::Relaxed);
        if host != 0 {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(HWND(host as *mut _));
            HOST_HWND.store(0, Ordering::Relaxed);
        }
        broadcast_taskbar_created();
    }
}

/// Starts the tray host thread once: creates our Shell_TrayWnd, hides
/// explorer's, and broadcasts TaskbarCreated so running apps migrate.
pub fn ensure_host() {
    if HOST_STARTED.swap(true, Ordering::SeqCst) {
        return; // already started (or starting)
    }
    std::thread::spawn(|| unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else { return };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wndproc),
            hInstance: hinstance.into(),
            lpszClassName: w!("Shell_TrayWnd"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("Shell_TrayWnd"),
            PCWSTR::null(),
            WS_POPUP,
            0, 0, 0, 0,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) else {
            return;
        };
        HOST_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

        for h in explorer_trays() {
            let _ = ShowWindow(HWND(h as *mut _), SW_HIDE);
        }
        broadcast_taskbar_created();
        windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), 1, 2000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

/// Full click gesture for a tray icon: foregrounds the owner (so its menus
/// can dismiss), then sends the down/up pair — plus WM_CONTEXTMENU for v4
/// right-clicks. Button 0 = left, 1 = middle, 2 = right.
pub fn send_button(icon: &TrayIcon, button: u8) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, SetForegroundWindow, WM_CONTEXTMENU, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP,
    };
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(HWND(icon.owner as usize as *mut _));
    }
    let cursor = (pt.x, pt.y);
    match button {
        0 => {
            forward_click(icon, WM_LBUTTONDOWN, cursor);
            forward_click(icon, WM_LBUTTONUP, cursor);
        }
        2 => {
            forward_click(icon, WM_RBUTTONDOWN, cursor);
            forward_click(icon, WM_RBUTTONUP, cursor);
            if icon.version >= 4 {
                forward_click(icon, WM_CONTEXTMENU, cursor);
            }
        }
        _ => {
            forward_click(icon, WM_MBUTTONUP, cursor);
        }
    }
}

/// Forwards a mouse event to an icon's owner, honoring NOTIFYICON_VERSION_4.
pub fn forward_click(icon: &TrayIcon, mouse_msg: u32, cursor: (i32, i32)) {
    if icon.callback == 0 {
        return;
    }
    unsafe {
        let owner = HWND(icon.owner as usize as *mut _);
        if icon.version >= 4 {
            let wparam = WPARAM(((cursor.1 as u16 as usize) << 16) | cursor.0 as u16 as usize);
            let lparam = LPARAM((((icon.uid as u16 as isize) << 16) | mouse_msg as u16 as isize) as isize);
            let _ = PostMessageW(Some(owner), icon.callback, wparam, lparam);
        } else {
            let _ = PostMessageW(
                Some(owner),
                icon.callback,
                WPARAM(icon.uid as usize),
                LPARAM(mouse_msg as isize),
            );
        }
    }
}
