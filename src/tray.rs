//! System-tray host: a window classed "Shell_TrayWnd" that receives the
//! Shell_NotifyIcon WM_COPYDATA protocol, replacing explorer's taskbar as
//! the tray. Created topmost so FindWindow("Shell_TrayWnd") resolves to us;
//! explorer's own taskbar windows are hidden while we run.
//!
//! Safety valve: `optim-bar.exe --restore-tray` (or graceful exit) un-hides
//! explorer's taskbar and broadcasts TaskbarCreated so icons return to it.

use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetMessageW,
    GetWindowThreadProcessId, PostMessageW, RegisterClassW, RegisterWindowMessageW, SendMessageW,
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
const NIF_STATE: u32 = 0x08;
const NIS_HIDDEN: u32 = 0x01;

#[derive(Clone)]
pub struct TrayIcon {
    pub owner: u32,     // HWND of the owning app (32-bit in the protocol)
    pub uid: u32,
    pub callback: u32,  // uCallbackMessage
    pub version: u32,
    pub hidden: bool,
    /// uFlags from the last ADD/MODIFY, for `--list-tray`.
    pub flags: u32,
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

/// Reads the hidden bit out of an ADD/MODIFY: `(said anything, is hidden)`.
///
/// `dwState` and `dwStateMask` are meaningful **only** when `NIF_STATE` is
/// set in `uFlags`; otherwise they hold whatever the caller left in the
/// struct, and trusting them marks live icons hidden at random. The first
/// return value distinguishes "explicitly visible" from "didn't say", so a
/// MODIFY that omits NIF_STATE leaves an existing icon's state alone.
fn hidden_state(flags: u32, dw_state: u32, dw_state_mask: u32) -> (bool, bool) {
    let given = flags & NIF_STATE != 0 && dw_state_mask & NIS_HIDDEN != 0;
    (given, given && dw_state & NIS_HIDDEN != 0)
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
            let (state_given, hidden_now) = hidden_state(flags, dw_state, dw_state_mask);
            match existing {
                Some(icon) => {
                    if flags & NIF_MESSAGE != 0 {
                        icon.callback = callback;
                    }
                    if pixels.is_some() {
                        icon.pixels = pixels;
                    }
                    if state_given {
                        icon.hidden = hidden_now;
                    }
                    icon.flags = flags;
                }
                None => {
                    if message == NIM_ADD {
                        icons.push(TrayIcon {
                            owner,
                            uid,
                            callback: if flags & NIF_MESSAGE != 0 { callback } else { 0 },
                            version: 0,
                            hidden: hidden_now,
                            flags,
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
            // Prune icons whose owning window died without NIM_DELETE
            // (crashed or force-killed apps leave ghosts otherwise) —
            // explorer does the same validation.
            if let Ok(mut icons) = state().lock() {
                icons.retain(|i| {
                    windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(
                        i.owner as isize as *mut _,
                    )))
                    .as_bool()
                });
            }
            // Did we lose the top Shell_TrayWnd slot since the last tick?
            // Sample it first: hide_explorer_trays() below goes through
            // with_explorer_routed(), which demotes us on purpose, so any
            // later reading would report a loss we caused ourselves.
            let lost_top = found_tray() != hwnd.0 as isize;
            // Explorer re-shows its taskbars on some shell events; re-assert.
            // Going through hide_explorer_trays also drops the work-area
            // reservation it re-registers along with them.
            let explorer_reappeared = explorer_trays().into_iter().any(|h| {
                windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(HWND(h as *mut _)).as_bool()
            });
            if explorer_reappeared {
                hide_explorer_trays();
            }
            // Keep us first in the topmost band: FindWindow("Shell_TrayWnd")
            // walks z-order, and whichever tray it finds gets the icon and
            // appbar traffic. Explorer periodically re-tops its own.
            use windows::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            };
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            if explorer_reappeared || lost_top {
                // Two ways an icon goes missing, both ending the same way:
                // it registered with a Shell_TrayWnd that isn't ours, and
                // nothing ever asks for it again.
                //
                // 1. Explorer coming back makes every app re-register, and
                //    for the second or two before the re-top above, the tray
                //    FindWindow reaches is explorer's brand-new one.
                // 2. We hand the top slot to explorer deliberately whenever
                //    with_explorer_routed() runs an appbar message. Anything
                //    calling Shell_NotifyIcon in that window lands there too.
                //
                // Now that we're back on top, ask everyone to register again.
                broadcast_throttled(15_000);
            }
            return LRESULT(0);
        }
        if msg == dump_message() {
            write_dump();
            return LRESULT(1);
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
            // Anything else — the appbar (ABM) protocol especially, dwData 0
            // — is shell business we intercepted by owning the class. Relay
            // to explorer's hidden tray: its appbar registry still runs, and
            // explorer recomputes work areas from that registry, stomping
            // anything written with raw SPI_SETWORKAREA. Being a real appbar
            // (via this relay) is the only reservation explorer preserves.
            if let Some(h) = explorer_main_tray() {
                return SendMessageW(HWND(h as *mut _), WM_COPYDATA, Some(wparam), Some(lparam));
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

/// The taskbar state we found before taking the shell over, to hand back.
static PREV_TASKBAR_STATE: AtomicIsize = AtomicIsize::new(-1);

/// Hide explorer's taskbars *and* give back the screen space they reserve.
///
/// `ShowWindow(SW_HIDE)` only takes the pixels off screen. The taskbar stays
/// registered as an appbar, so its 48 px stays carved out of every monitor's
/// work area and tiled windows stop short of a taskbar nobody can see — most
/// visibly right after explorer restarts and re-registers.
///
/// Only `ABM_SETSTATE(ABS_AUTOHIDE)` releases it. Measured, in this order:
/// `SPI_SETWORKAREA` returns success and changes nothing on the primary
/// (explorer owns that monitor's work area and recomputes from its appbar
/// registry); `ABM_REMOVE` on explorer's own tray windows is ignored, being
/// another process's registration; moving those windows off-screen changes
/// nothing either, because the registered rect is independent of where the
/// window actually is. Auto-hide drops the reservation on every monitor at
/// once, and the taskbar is hidden anyway so nothing can slide into view.
fn hide_explorer_trays() {
    use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETSTATE, ABM_SETSTATE, APPBARDATA};
    const ABS_AUTOHIDE: isize = 0x01;
    unsafe {
        for h in explorer_trays() {
            let _ = ShowWindow(HWND(h as *mut _), SW_HIDE);
        }
        with_explorer_routed(|| {
            let mut abd = APPBARDATA {
                cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                ..Default::default()
            };
            if PREV_TASKBAR_STATE.load(Ordering::Relaxed) < 0 {
                let state = SHAppBarMessage(ABM_GETSTATE, &mut abd) as isize;
                PREV_TASKBAR_STATE.store(state, Ordering::Relaxed);
            }
            abd.lParam = LPARAM(ABS_AUTOHIDE);
            SHAppBarMessage(ABM_SETSTATE, &mut abd);
        });
    }
}

/// Put the taskbar's auto-hide state back the way we found it.
fn restore_taskbar_state() {
    use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_SETSTATE, APPBARDATA};
    let prev = PREV_TASKBAR_STATE.swap(-1, Ordering::Relaxed);
    if prev < 0 {
        return; // never took it over
    }
    unsafe {
        with_explorer_routed(|| {
            let mut abd = APPBARDATA {
                cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                lParam: LPARAM(prev),
                ..Default::default()
            };
            SHAppBarMessage(ABM_SETSTATE, &mut abd);
        });
    }
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

unsafe extern "system" fn find_main_tray(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let out = &mut *(lparam.0 as *mut isize);
    let mut class = [0u16; 64];
    let n = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut class);
    if String::from_utf16_lossy(&class[..n.max(0) as usize]) == "Shell_TrayWnd" {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != GetCurrentProcessId() {
            *out = hwnd.0 as isize;
            return false.into(); // stop enumeration
        }
    }
    true.into()
}

/// Explorer's (hidden) primary taskbar window — the ABM relay target.
fn explorer_main_tray() -> Option<isize> {
    let mut found: isize = 0;
    unsafe {
        let _ = EnumWindows(Some(find_main_tray), LPARAM(&mut found as *mut _ as isize));
    }
    (found != 0).then_some(found)
}

/// Ask every app to register its tray icons again, from a separate process.
///
/// Diagnostic *and* repair for the one failure this design has: an icon that
/// registered with the wrong Shell_TrayWnd is simply absent, with nothing to
/// distinguish it from an app that never had an icon.
pub fn rebroadcast() {
    broadcast_taskbar_created();
}

fn broadcast_taskbar_created() {
    unsafe {
        let msg = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = SendNotifyMessageW(HWND_BROADCAST, msg, WPARAM(0), LPARAM(0));
    }
}

/// Where the running host writes its icon table for `--list-tray`.
pub fn dump_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
        .join("optim-bar")
        .join("tray-dump.txt")
}

fn dump_message() -> u32 {
    unsafe { RegisterWindowMessageW(w!("optim_bar_dump_tray")) }
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Serialises the icon table to [`dump_path`].
///
/// Through a file rather than a return value because the table lives in
/// *this* process's memory. A second optim-bar run with `--list-tray` has an
/// empty one, and that is exactly what makes a misrouted icon indis-
/// tinguishable from an app that never registered one.
fn write_dump() {
    use std::fmt::Write as _;
    let st = state(); // bind: locking a temporary Arc borrows it past its life
    let Ok(icons) = st.lock() else { return };
    let mut s = String::new();
    let _ = writeln!(s, "{} icon(s) held by the optim-bar tray host", icons.len());
    let _ = writeln!(s);
    for i in icons.iter() {
        let owner = HWND(i.owner as usize as *mut _);
        let exe = crate::switcher::exe_of(owner);
        let _ = writeln!(
            s,
            "{:<26} hwnd={:#010x} uid={:<5} callback={:#06x} v={} flags={:#06x} pixels={:<3} {}",
            if exe.is_empty() { "<unknown>".into() } else { exe },
            i.owner,
            i.uid,
            i.callback,
            i.version,
            i.flags,
            if i.pixels.is_some() { "yes" } else { "no" },
            if i.hidden {
                "HIDDEN — shown only in the flyout, not inline"
            } else {
                "visible"
            },
        );
        let _ = writeln!(s, "{:<26} owner class: {}", "", class_of(owner));
    }
    let p = dump_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, s);
}

pub enum DumpError {
    /// No optim-bar is hosting the tray.
    NoHost,
    /// One is, but it ignored the message — almost always an older build
    /// still running while a newer binary sits on disk.
    NoAnswer,
}

/// Asks the running host to dump its table, and returns what it wrote.
pub fn request_dump() -> Result<String, DumpError> {
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_NORMAL};
    let mut host: isize = 0;
    unsafe {
        let _ = EnumWindows(
            Some(find_optim_tray),
            LPARAM(&mut host as *mut _ as isize),
        );
    }
    if host == 0 {
        return Err(DumpError::NoHost);
    }
    let path = dump_path();
    let _ = std::fs::remove_file(&path);
    unsafe {
        let mut result = 0usize;
        // Timeout rather than a bare SendMessage: the host could be wedged,
        // and a diagnostic that hangs is no diagnostic.
        let _ = SendMessageTimeoutW(
            HWND(host as *mut _),
            dump_message(),
            WPARAM(0),
            LPARAM(0),
            SMTO_NORMAL,
            3000,
            Some(&mut result),
        );
    }
    std::fs::read_to_string(&path).map_err(|_| DumpError::NoAnswer)
}

unsafe extern "system" fn find_optim_tray(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let out = &mut *(lparam.0 as *mut isize);
    if class_of(hwnd) == "Shell_TrayWnd" {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != GetCurrentProcessId() && crate::switcher::exe_of(hwnd) == "optim-bar.exe" {
            *out = hwnd.0 as isize;
            return false.into(); // stop enumeration
        }
    }
    true.into()
}

/// Tick of the last TaskbarCreated broadcast, for throttling.
static LAST_BROADCAST: AtomicU64 = AtomicU64::new(0);

/// Broadcast at most once per `min_gap_ms`.
///
/// Every app re-registers its icons on TaskbarCreated, so firing this on a
/// 2 s cadence would have every tray app rebuilding its icon constantly.
/// The recovery cases are rare and a few seconds of latency is invisible.
fn broadcast_throttled(min_gap_ms: u64) {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    let now = unsafe { GetTickCount64() };
    let last = LAST_BROADCAST.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < min_gap_ms {
        return;
    }
    LAST_BROADCAST.store(now, Ordering::Relaxed);
    broadcast_taskbar_created();
}

/// Runs `f` with our Shell_TrayWnd dropped to the bottom of the z-order,
/// so FindWindow("Shell_TrayWnd") — which walks z-order — resolves to
/// explorer's real tray for the duration. SHAppBarMessage needs this:
/// explorer services state-changing ABM messages (NEW/SETPOS/REMOVE) only
/// when they arrive directly, not via our WM_COPYDATA relay.
/// Our tray host window, or 0 before it exists. While it's 0, FindWindow
/// already resolves to explorer and no routing dance is needed.
pub fn host_hwnd() -> isize {
    HOST_HWND.load(Ordering::Relaxed)
}

/// The first `Shell_TrayWnd` FindWindow resolves to right now — the window
/// SHAppBarMessage will actually talk to.
pub fn found_tray() -> isize {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe { FindWindowW(w!("Shell_TrayWnd"), None).map(|h| h.0 as isize).unwrap_or(0) }
}

pub fn with_explorer_routed<R>(f: impl FnOnce() -> R) -> R {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_BOTTOM, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    let host = HOST_HWND.load(Ordering::Relaxed);
    if host == 0 {
        return f(); // host not up yet: FindWindow already resolves to explorer
    }
    unsafe {
        let hw = HWND(host as *mut _);
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
        // One demotion isn't always enough: FindWindow has been observed
        // still resolving to us right after, and a state-changing ABM that
        // lands on our own tray window is relayed onward and quietly does
        // nothing (while still returning success). Confirm before proceeding.
        for _ in 0..5 {
            let _ = SetWindowPos(hw, Some(HWND_BOTTOM), 0, 0, 0, 0, flags);
            if found_tray() != host {
                break;
            }
        }
        let r = f();
        let _ = SetWindowPos(hw, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        r
    }
}

/// Un-hides explorer's taskbar windows and tells apps to re-register there.
pub fn restore_explorer_tray() {
    restore_taskbar_state(); // before our host goes away, while routing still works
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

        hide_explorer_trays();
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

#[cfg(test)]
mod tests {
    use super::{hidden_state, NIF_ICON, NIF_MESSAGE, NIF_STATE, NIS_HIDDEN};

    /// Proton VPN registers with flags 0x00AF — NIF_STATE set and NIS_HIDDEN
    /// asserted — which is how a Windows 11 overflow icon announces itself.
    #[test]
    fn an_explicit_hidden_icon_is_recognised() {
        let (given, hidden) = hidden_state(0x00AF, NIS_HIDDEN, NIS_HIDDEN);
        assert!(given);
        assert!(hidden);
    }

    /// Without NIF_STATE the state fields are uninitialised caller memory.
    /// Reading them anyway is how a perfectly visible icon disappears.
    #[test]
    fn state_fields_are_ignored_without_nif_state() {
        let flags = NIF_MESSAGE | NIF_ICON; // no NIF_STATE
        let (given, hidden) = hidden_state(flags, NIS_HIDDEN, NIS_HIDDEN);
        assert!(!given, "must not treat leftover bytes as a state claim");
        assert!(!hidden);
    }

    /// NIF_STATE set but the mask not covering NIS_HIDDEN says nothing about
    /// hidden-ness, so an existing icon must keep whatever it had.
    #[test]
    fn a_modify_that_omits_the_hidden_bit_says_nothing() {
        let (given, _) = hidden_state(NIF_STATE, 0, 0);
        assert!(!given);
    }

    #[test]
    fn explicitly_unhiding_is_distinguishable_from_silence() {
        let (given, hidden) = hidden_state(NIF_STATE, 0, NIS_HIDDEN);
        assert!(given, "mask covers the bit, so this is a real claim");
        assert!(!hidden, "state bit clear means show it");
    }
}