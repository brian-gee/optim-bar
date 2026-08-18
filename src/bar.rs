use std::ffi::c_void;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, FindWindowW,
    GetCursorPos, GetWindowThreadProcessId, PostMessageW,
    GetWindowLongPtrW, LoadCursorW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW, TrackPopupMenu,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HWND_TOPMOST, IDC_ARROW, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, SPIF_SENDCHANGE, SPI_SETWORKAREA, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNA,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_ERASEBKGND, WM_HOTKEY, WM_LBUTTONUP, WM_MBUTTONUP, WM_NCCREATE, WM_PAINT, WM_RBUTTONUP,
    WM_SIZE,
    WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use std::collections::HashMap;

use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Bitmap, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::WM_MOUSEWHEEL;

use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABE_BOTTOM, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS,
    ABM_WINDOWPOSCHANGED, APPBARDATA,
};

/// Appbar-negotiation trace, for when a monitor won't give up its work area.
/// Off unless `%LOCALAPPDATA%\optim-bar\appbar-probe.log` already exists
/// (create it empty to switch on, delete it to switch off), so it costs one
/// `exists()` per re-assert and nothing else. Worth keeping: SHAppBarMessage
/// reports success whether explorer applied the reservation, ignored it, or
/// never saw it, and only this trace tells those three apart.
fn probe_log(line: &str) {
    let Ok(dir) = std::env::var("LOCALAPPDATA") else { return };
    let path = std::path::Path::new(&dir).join("optim-bar").join("appbar-probe.log");
    if !path.exists() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

use crate::config::{self, BarConfig};
use crate::widgets::{self, Role, Segment, Widget};

/// Appbar notification callback (ABN_* in wparam).
pub const WM_APP_APPBAR: u32 = WM_APP + 2;

/// ShellExecute "open" on an exe name or ms-settings: URI.
fn shell_open(what: PCWSTR) {
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            w!("open"),
            what,
            None,
            None,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
}

/// Explorer broadcasts this on (re)start; appbar registrations die with it.
fn taskbar_created_msg() -> u32 {
    use std::sync::OnceLock;
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| unsafe {
        windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(w!("TaskbarCreated"))
    })
}

/// Asks a running bar to shut down the way its menu's Exit does.
///
/// Exit lives in the bar's own context menu, which is exactly what is missing
/// whenever the bar is the thing that needs replacing — after a rebuild, or
/// from the launcher. Registered rather than a `WM_APP` value because the
/// sender is a second process.
fn quit_msg() -> u32 {
    use std::sync::OnceLock;
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| unsafe {
        windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(w!("optim_bar_quit"))
    })
}

/// Posts [`quit_msg`] to a running bar and waits for that process to go.
///
/// The wait is the point: the single-instance mutex outlives the window, so a
/// replacement started the moment the window vanishes exits silently instead
/// of taking over. Waits on the process handle rather than polling for the
/// window, which is released a beat too early.
///
/// False when no bar is running — not an error for a restart, which still has
/// its start half to do.
pub fn request_quit() -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    unsafe {
        let Ok(hwnd) = FindWindowW(WINDOW_CLASS, None) else {
            return false;
        };
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // Opened before the message is posted: afterwards the process may
        // already be gone, leaving nothing to wait on.
        let proc = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok();
        if PostMessageW(Some(hwnd), quit_msg(), WPARAM(0), LPARAM(0)).is_err() {
            if let Some(h) = proc {
                let _ = CloseHandle(h);
            }
            return false;
        }
        match proc {
            Some(h) => {
                // Generous: teardown un-reserves every monitor's work area and
                // hands the tray back, each a broadcast explorer answers.
                let done = WaitForSingleObject(h, 5000) == WAIT_OBJECT_0;
                let _ = CloseHandle(h);
                done
            }
            None => true,
        }
    }
}

/// Icon edge in logical px (matches Brian's YASB icon_size 18).
const ICON_EDGE: f32 = 18.0;
/// Source pixel size widgets provide icons at.
pub const ICON_SRC: usize = 32;

pub const WINDOW_CLASS: PCWSTR = w!("optim_bar_window");
pub const WM_APP_RELOAD: u32 = WM_APP + 1;

/// Set by any bar on WM_DISPLAYCHANGE or by the config watcher; the main
/// loop tears down all bars and recreates them.
pub static REBUILD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct Mon {
    handle: isize,
    rect: windows::Win32::Foundation::RECT,
    primary: bool,
    /// Hardware id, e.g. "MONITOR\SAM78B7\..." — stable across reboots,
    /// unlike \\.\DISPLAYn numbering. Matched by `exclude =`.
    device_id: String,
}

unsafe extern "system" fn mon_cb(
    hmon: windows::Win32::Graphics::Gdi::HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _rc: *mut windows::Win32::Foundation::RECT,
    lparam: LPARAM,
) -> windows::core::BOOL {
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayDevicesW, GetMonitorInfoW, DISPLAY_DEVICEW, MONITORINFO, MONITORINFOEXW,
    };
    let out = &mut *(lparam.0 as *mut Vec<Mon>);
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let _ = GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut MONITORINFO);
    let mut dd = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    let device_id = if EnumDisplayDevicesW(PCWSTR(mi.szDevice.as_ptr()), 0, &mut dd, 0).as_bool()
    {
        let id = &dd.DeviceID;
        let len = id.iter().position(|&c| c == 0).unwrap_or(id.len());
        String::from_utf16_lossy(&id[..len])
    } else {
        String::new()
    };
    out.push(Mon {
        handle: hmon.0 as isize,
        rect: mi.monitorInfo.rcMonitor,
        primary: mi.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
        device_id,
    });
    true.into()
}

fn monitors() -> Vec<Mon> {
    let mut v: Vec<Mon> = Vec::new();
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::EnumDisplayMonitors(
            None,
            None,
            Some(mon_cb),
            LPARAM(&mut v as *mut _ as isize),
        );
    }
    // Primary first so bar index 0 is always the main monitor.
    v.sort_by_key(|m| !m.primary);
    v
}

/// Escape-hatch companion to --restore-tray: gives every monitor its full
/// rect back as work area (the bar process may have died without cleanup).
pub fn restore_work_areas() {
    for m in monitors() {
        let mut rc = m.rect;
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
                SPI_SETWORKAREA,
                0,
                Some(&mut rc as *mut _ as *mut c_void),
                SPIF_SENDCHANGE,
            );
        }
    }
}

/// Creates one bar per monitor (or primary only), per the freshly-read
/// config. Monitors whose device id contains an `exclude =` entry get none.
pub fn create_all() -> Vec<Box<Bar>> {
    let cfg = config::load();
    let mons = monitors();
    let mut bars = Vec::new();
    let mut index = 0;
    for mon in mons.iter() {
        if cfg
            .exclude
            .iter()
            .any(|e| mon.device_id.to_lowercase().contains(&e.to_lowercase()))
        {
            continue;
        }
        if index > 0 && !cfg.all_monitors {
            break;
        }
        match Bar::create(index, mon.handle, mon.rect) {
            Ok(b) => {
                bars.push(b);
                index += 1;
            }
            Err(_) => continue,
        }
    }
    bars
}

const TIMER_TICK: usize = 1;
const GAP: f32 = 8.0; // between widgets, logical px

fn col(v: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((v >> 16) & 0xFF) as f32 / 255.0,
        g: ((v >> 8) & 0xFF) as f32 / 255.0,
        b: (v & 0xFF) as f32 / 255.0,
        a,
    }
}

enum Side {
    Left,
    Center,
    Right,
}

struct Slot {
    widget: Box<dyn Widget>,
    side: Side,
    /// Minimum clickable cell per segment, logical px. Single-glyph segments
    /// (workspace numbers) measure ~8 px, which is a miserable mouse target;
    /// the cell grows around the text without changing the font.
    min_seg: f32,
}

/// Hit-test record from the last layout pass (physical px).
struct HitRect {
    slot: usize,
    seg: usize,
    left: f32,
    right: f32,
}

struct Gfx {
    rt: ID2D1HwndRenderTarget,
    fg: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    custom: ID2D1SolidColorBrush, // recolored per Custom segment
    fmt: IDWriteTextFormat,
    icons: HashMap<u64, ID2D1Bitmap>,
}

pub struct Bar {
    hwnd: HWND,
    d2d_factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    gfx: Option<Gfx>,
    slots: Vec<Slot>,
    hits: Vec<HitRect>,
    cfg: BarConfig,
    scale: f32,
    index: usize,
    monitor: isize,
    mon_rect: RECT,
    /// Bar is hidden because a fullscreen app owns this monitor.
    fs_hidden: bool,
    /// Last work-area re-assertion, throttling the self-heal.
    last_assert: std::time::Instant,
}

impl Bar {
    pub fn create(index: usize, monitor: isize, mon_rect: RECT) -> Result<Box<Bar>> {
        unsafe {
            let hinstance = GetModuleHandleW(None)?;
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: WINDOW_CLASS,
                ..Default::default()
            };
            RegisterClassW(&wc); // fails harmlessly after the first bar

            let d2d_factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let cfg = config::load();
            let mut bar = Box::new(Bar {
                hwnd: HWND::default(),
                d2d_factory,
                dwrite,
                gfx: None,
                slots: Vec::new(),
                hits: Vec::new(),
                cfg,
                scale: 1.0,
                index,
                monitor,
                mon_rect,
                fs_hidden: false,
                last_assert: std::time::Instant::now(),
            });
            bar.build_slots();

            // Born on its own monitor rather than at (0,0): GetDpiForWindow
            // below then reports the DPI of the monitor the bar will live on,
            // instead of the primary's until a WM_DPICHANGED corrects it.
            let h = (bar.cfg.height) as i32;
            let y = if bar.cfg.position_top { mon_rect.top } else { mon_rect.bottom - h };
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                WINDOW_CLASS,
                w!("optim-bar"),
                WS_POPUP,
                mon_rect.left, y, mon_rect.right - mon_rect.left, h,
                None,
                None,
                Some(hinstance.into()),
                Some(&*bar as *const Bar as *const c_void),
            )?;
            bar.hwnd = hwnd;
            bar.scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            bar.register_hotkeys();
            bar.position();

            let _ = ShowWindow(hwnd, SW_SHOWNA);
            SetTimer(Some(hwnd), TIMER_TICK, 250, None);
            Ok(bar)
        }
    }

    pub fn hwnd_val(&self) -> isize {
        self.hwnd.0 as isize
    }

    fn mon_w(&self) -> i32 {
        self.mon_rect.right - self.mon_rect.left
    }

    fn build_slots(&mut self) {
        self.slots.clear();
        let groups = [("left", 0u8), ("center", 1u8), ("right", 2u8)];
        for (side_name, side) in groups {
            for name in self.cfg.side_widgets(side_name, self.index) {
                if let Some(widget) = widgets::build(&name, &self.cfg, self.index, self.monitor) {
                    let section = format!("widget.{name}");
                    // Resolved the same way widgets::build does, so `type =`
                    // aliases still pick up the right default.
                    let kind = self.cfg.values.get_or(&section, "type", &name);
                    let default_min = if kind == "workspaces" { 26.0 } else { 0.0 };
                    self.slots.push(Slot {
                        widget,
                        side: match side {
                            0 => Side::Left,
                            1 => Side::Center,
                            _ => Side::Right,
                        },
                        min_seg: self
                            .cfg
                            .values
                            .get_f32(&section, "min_width", default_min)
                            .clamp(0.0, 200.0),
                    });
                }
            }
        }
    }

    /// Global hotkeys, id = slot index. Primary bar only, so the same
    /// widget on three monitors doesn't fight over one registration.
    /// MUST run after CreateWindowExW: a null-hwnd registration binds to
    /// the thread, and DispatchMessage drops thread-queue WM_HOTKEY.
    fn register_hotkeys(&self) {
        if self.index != 0 {
            return;
        }
        use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, HOT_KEY_MODIFIERS};
        const MOD_NOREPEAT: u32 = 0x4000;
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some((mods, vk)) = slot.widget.hotkey_spec() {
                unsafe {
                    let _ = RegisterHotKey(
                        Some(self.hwnd),
                        i as i32,
                        HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT),
                        vk,
                    );
                }
            }
        }
    }

    /// Full-width strip on the monitor's top or bottom edge.
    /// With `reserve = true` (default) the bar registers as an AppBar.
    /// Raw SPI_SETWORKAREA writes don't survive: explorer recomputes work
    /// areas from its appbar registry on shell events and erases anything
    /// it doesn't know about. SHAppBarMessage still works because tray.rs
    /// relays ABM WM_COPYDATA traffic to explorer's hidden tray window.
    fn position(&self) {
        unsafe {
            let mr = self.mon_rect;
            let h = (self.cfg.height * self.scale) as i32;
            let mut y = if self.cfg.position_top { mr.top } else { mr.bottom - h };

            if self.cfg.reserve {
                let mut abd = APPBARDATA {
                    cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                    hWnd: self.hwnd,
                    uCallbackMessage: WM_APP_APPBAR,
                    uEdge: if self.cfg.position_top { ABE_TOP } else { ABE_BOTTOM },
                    rc: RECT {
                        left: mr.left,
                        top: if self.cfg.position_top { mr.top } else { mr.bottom - h },
                        right: mr.right,
                        bottom: if self.cfg.position_top { mr.top + h } else { mr.bottom },
                    },
                    ..Default::default()
                };
                let asked = abd.rc;
                let host = crate::tray::host_hwnd();
                let mut routed_to = 0;
                let (new_r, query_r, setpos_r) = crate::tray::with_explorer_routed(|| {
                    routed_to = crate::tray::found_tray();
                    let a = SHAppBarMessage(ABM_NEW, &mut abd); // no-op if registered
                    let b = SHAppBarMessage(ABM_QUERYPOS, &mut abd);
                    if self.cfg.position_top {
                        abd.rc.bottom = abd.rc.top + h;
                    } else {
                        abd.rc.top = abd.rc.bottom - h;
                    }
                    let c = SHAppBarMessage(ABM_SETPOS, &mut abd);
                    (a, b, c)
                });
                y = abd.rc.top;
                probe_log(&format!(
                    "bar{} asked=({},{})-({},{}) got=({},{})-({},{}) NEW={} QUERYPOS={} SETPOS={} host={:#x} routed_to={:#x} {}",
                    self.index,
                    asked.left, asked.top, asked.right, asked.bottom,
                    abd.rc.left, abd.rc.top, abd.rc.right, abd.rc.bottom,
                    new_r, query_r, setpos_r,
                    host, routed_to,
                    if routed_to == host && host != 0 { "SELF!" } else { "explorer" }
                ));
            }

            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                mr.left,
                y,
                self.mon_w(),
                h,
                SWP_NOACTIVATE,
            );

            if self.cfg.reserve {
                // The "I moved, recompute work areas" kick. Registration
                // alone can get lost in the shell churn our own startup
                // causes (taskbar hiding + TaskbarCreated broadcast).
                let mut abd = APPBARDATA {
                    cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                    hWnd: self.hwnd,
                    ..Default::default()
                };
                crate::tray::with_explorer_routed(|| {
                    SHAppBarMessage(ABM_WINDOWPOSCHANGED, &mut abd);
                });
                self.claim_work_area();
            }
        }
    }

    /// Reserve the strip on a secondary monitor by writing the work area
    /// directly.
    ///
    /// Explorer applies appbar reservations to the PRIMARY monitor only.
    /// Measured, not assumed: a secondary bar's ABM_NEW/QUERYPOS/SETPOS all
    /// return success with the rect unchanged, and the monitor's work area
    /// never moves — while the identical sequence on the primary reserves
    /// immediately. So the appbar registration stays (it is what works on
    /// the primary, and it keeps explorer aware of the window) and every
    /// other monitor gets SPI_SETWORKAREA.
    ///
    /// Only written when it is actually wrong. SPIF_SENDCHANGE broadcasts
    /// WM_SETTINGCHANGE, explorer answers by recomputing work areas, and the
    /// other bars answer that with ABN_POSCHANGED re-negotiation — an
    /// unconditional write turns that chain into a permanent loop. Explorer
    /// still erases this write whenever it recomputes, which is what the
    /// WM_TIMER self-heal is for.
    fn claim_work_area(&self) {
        if self.is_primary() || self.work_area_ok() {
            return;
        }
        unsafe {
            let h = (self.cfg.height * self.scale) as i32;
            let mut wa = self.mon_rect;
            if self.cfg.position_top {
                wa.top += h;
            } else {
                wa.bottom -= h;
            }
            let _ = SystemParametersInfoW(
                SPI_SETWORKAREA,
                0,
                Some(&mut wa as *mut RECT as *mut std::ffi::c_void),
                SPIF_SENDCHANGE,
            );
        }
    }

    /// True for the monitor explorer treats as primary — the only one whose
    /// appbar reservation it honors.
    fn is_primary(&self) -> bool {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
        const MONITORINFOF_PRIMARY: u32 = 1;
        unsafe {
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            GetMonitorInfoW(HMONITOR(self.monitor as *mut _), &mut mi).as_bool()
                && mi.dwFlags & MONITORINFOF_PRIMARY != 0
        }
    }

    /// Whether the shell currently honors our reservation: the work area
    /// must start below (or end above) the bar strip. `>=` because another
    /// appbar may legitimately reserve more.
    fn work_area_ok(&self) -> bool {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
        unsafe {
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(HMONITOR(self.monitor as *mut _), &mut mi).as_bool() {
                return true; // monitor gone; rebuild will handle it
            }
            let h = (self.cfg.height * self.scale) as i32;
            if self.cfg.position_top {
                mi.rcWork.top >= self.mon_rect.top + h
            } else {
                mi.rcWork.bottom <= self.mon_rect.bottom - h
            }
        }
    }

    fn remove_appbar(&self) {
        unsafe {
            let mut abd = APPBARDATA {
                cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                hWnd: self.hwnd,
                ..Default::default()
            };
            crate::tray::with_explorer_routed(|| {
                SHAppBarMessage(ABM_REMOVE, &mut abd);
            });
        }
    }

    fn px(&self, v: f32) -> f32 {
        v * self.scale
    }

    /// True when ANY visible window fully covers this bar's monitor
    /// (borderless or exclusive fullscreen) — focused or not. A fullscreen
    /// RDP session's hover-activated connection bar lives at the top edge;
    /// it must get the mouse even while the user works on another monitor,
    /// so foreground-only detection isn't enough. Polled from the 250 ms
    /// timer (real appbars get ABN_FULLSCREENAPP, but not reliably for
    /// unfocused windows either).
    fn fullscreen_on_monitor(&self) -> bool {
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetClassNameW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
            IsWindowVisible,
        };

        struct Ctx {
            mon_rect: RECT,
            my_pid: u32,
            found: bool,
        }

        unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
            let ctx = &mut *(lp.0 as *mut Ctx);
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return true.into();
            }
            // Skip our own windows (bars are monitor-sized strips, but be safe).
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == ctx.my_pid {
                return true.into();
            }
            // Cloaked UWP ghosts report full-screen rects while invisible.
            let mut cloaked: u32 = 0;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut _ as *mut _,
                std::mem::size_of::<u32>() as u32,
            );
            if cloaked != 0 {
                return true.into();
            }
            // The desktop and shell surfaces are monitor-sized but aren't games.
            let mut class = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut class) as usize;
            let class = String::from_utf16_lossy(&class[..n]);
            if matches!(
                class.as_str(),
                "WorkerW" | "Progman" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            ) {
                return true.into();
            }
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_err() {
                return true.into();
            }
            let m = ctx.mon_rect;
            if rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom
            {
                ctx.found = true;
                return false.into(); // stop enumerating
            }
            true.into()
        }

        unsafe {
            let mut ctx = Ctx {
                mon_rect: self.mon_rect,
                my_pid: windows::Win32::System::Threading::GetCurrentProcessId(),
                found: false,
            };
            let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
            ctx.found
        }
    }

    fn build_gfx(&self) -> Result<Gfx> {
        unsafe {
            let screen_w = self.mon_w() as u32;
            let h = (self.cfg.height * self.scale) as u32;
            let rt = self.d2d_factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
                    // Software raster: a 36px strip repainting ~1/s doesn't
                    // need a GPU context (the NVIDIA D3D device costs ~40 MB
                    // of private bytes per process).
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    dpiX: 96.0,
                    dpiY: 96.0,
                    ..Default::default()
                },
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd: self.hwnd,
                    pixelSize: D2D_SIZE_U { width: screen_w, height: h },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )?;
            let fg = rt.CreateSolidColorBrush(&col(self.cfg.fg, 1.0), None)?;
            let dim = rt.CreateSolidColorBrush(&col(self.cfg.dim, 1.0), None)?;
            let accent = rt.CreateSolidColorBrush(&col(self.cfg.accent, 1.0), None)?;
            let custom = rt.CreateSolidColorBrush(&col(self.cfg.fg, 1.0), None)?;
            let font16: Vec<u16> = self
                .cfg
                .font
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let fmt = self.dwrite.CreateTextFormat(
                PCWSTR(font16.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                self.px(self.cfg.font_size),
                w!("en-us"),
            )?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            Ok(Gfx {
                rt,
                fg,
                dim,
                accent,
                custom,
                fmt,
                icons: HashMap::new(),
            })
        }
    }

    fn icon_bitmap(gfx: &mut Gfx, key: u64, pixels: &[u8]) -> Option<ID2D1Bitmap> {
        if let Some(b) = gfx.icons.get(&key) {
            return Some(b.clone());
        }
        unsafe {
            let bmp = gfx
                .rt
                .CreateBitmap(
                    D2D_SIZE_U {
                        width: ICON_SRC as u32,
                        height: ICON_SRC as u32,
                    },
                    Some(pixels.as_ptr() as _),
                    (ICON_SRC * 4) as u32,
                    &D2D1_BITMAP_PROPERTIES {
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                        },
                        dpiX: 96.0,
                        dpiY: 96.0,
                    },
                )
                .ok()?;
            gfx.icons.insert(key, bmp.clone());
            Some(bmp)
        }
    }

    fn measure(&self, gfx: &Gfx, text: &[u16]) -> f32 {
        unsafe {
            let Ok(layout) =
                self.dwrite
                    .CreateTextLayout(text, &gfx.fmt, f32::MAX, self.px(self.cfg.height))
            else {
                return 0.0;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            m.widthIncludingTrailingWhitespace
        }
    }

    fn render(&mut self) {
        if self.gfx.is_none() {
            self.gfx = self.build_gfx().ok();
        }
        let Some(mut gfx) = self.gfx.take() else { return };

        unsafe {
            let w = self.mon_w() as f32;
            let h = self.px(self.cfg.height);
            let pad = self.px(self.cfg.pad);
            let gap = self.px(GAP);
            let seg_gap = self.px(4.0);
            let icon_edge = self.px(ICON_EDGE);
            let (bg_rgb, bg_a) = self.cfg.bg;

            gfx.rt.BeginDraw();
            gfx.rt.Clear(Some(&col(bg_rgb, bg_a.max(0.99)))); // solid by design

            // Collect segments + measure each: (slot, seg, utf16, width, role, icon)
            struct Piece {
                slot: usize,
                seg: usize,
                text16: Vec<u16>,
                /// Ink width of icon + text.
                width: f32,
                /// Laid-out/clickable width; >= width, content centered in it.
                cell: f32,
                role: Role,
                icon: Option<(u64, std::sync::Arc<Vec<u8>>)>,
                fill: Option<u32>,
            }
            let mut pieces: Vec<Piece> = Vec::new();
            let mut slot_widths: Vec<f32> = vec![0.0; self.slots.len()];
            for (si, slot) in self.slots.iter().enumerate() {
                let segs: Vec<Segment> = slot.widget.segments();
                let mut total = 0.0;
                for (gi, seg) in segs.into_iter().enumerate() {
                    let text16: Vec<u16> = seg.text.encode_utf16().collect();
                    let tw = if text16.is_empty() {
                        0.0
                    } else {
                        self.measure(&gfx, &text16)
                    };
                    let iw = if seg.icon.is_some() {
                        icon_edge + if tw > 0.0 { seg_gap } else { 0.0 }
                    } else {
                        0.0
                    };
                    let width = tw + iw;
                    let cell = width.max(self.px(slot.min_seg));
                    if gi > 0 {
                        total += seg_gap;
                    }
                    total += cell;
                    pieces.push(Piece {
                        slot: si,
                        seg: gi,
                        text16,
                        width,
                        cell,
                        role: seg.role,
                        icon: seg.icon,
                        fill: seg.fill,
                    });
                }
                slot_widths[si] = total;
            }

            // Slot x positions: left run, centered center run, right run.
            let mut slot_x: Vec<f32> = vec![0.0; self.slots.len()];
            let mut left_x = pad;
            let mut right_x = w - pad;
            let side_total = |want: fn(&Side) -> bool| -> f32 {
                self.slots
                    .iter()
                    .enumerate()
                    .filter(|(i, s)| want(&s.side) && slot_widths[*i] > 0.0)
                    .map(|(i, _)| slot_widths[i] + gap)
                    .sum::<f32>()
                    - gap
            };
            let center_total = side_total(|s| matches!(s, Side::Center));
            // Center on the full width, but never overlap the side runs
            // (the portrait monitor is only 1080px wide).
            let left_end = pad + side_total(|s| matches!(s, Side::Left)).max(0.0) + gap;
            let right_start = w - pad - side_total(|s| matches!(s, Side::Right)).max(0.0) - gap;
            let ideal = (w - center_total.max(0.0)) / 2.0;
            let hi = (right_start - center_total.max(0.0)).max(left_end);
            let mut center_x = ideal.clamp(left_end.min(hi), hi);
            for (i, slot) in self.slots.iter().enumerate() {
                if slot_widths[i] == 0.0 {
                    slot_x[i] = -1.0; // hidden
                    continue;
                }
                slot_x[i] = match slot.side {
                    Side::Left => {
                        let x = left_x;
                        left_x += slot_widths[i] + gap;
                        x
                    }
                    Side::Center => {
                        let x = center_x;
                        center_x += slot_widths[i] + gap;
                        x
                    }
                    Side::Right => {
                        right_x -= slot_widths[i];
                        let x = right_x;
                        right_x -= gap;
                        x
                    }
                };
            }

            // Draw pieces and rebuild hit rects.
            self.hits.clear();
            let mut cursor: Vec<f32> = slot_x.clone();
            for p in &pieces {
                if slot_x[p.slot] < 0.0 {
                    continue;
                }
                let x = cursor[p.slot];
                // Selection pill, drawn under the content across the whole cell.
                if let Some(rgb) = p.fill {
                    let inset = self.px(4.0);
                    let r = self.px(4.0);
                    gfx.custom.SetColor(&col(rgb, 1.0));
                    gfx.rt.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: x,
                                top: inset,
                                right: x + p.cell,
                                bottom: h - inset,
                            },
                            radiusX: r,
                            radiusY: r,
                        },
                        &gfx.custom,
                    );
                }
                // Content sits centered in the cell; for most widgets cell ==
                // width and this is a no-op.
                let mut draw_x = x + (p.cell - p.width) / 2.0;
                if let Some((key, pixels)) = &p.icon {
                    if let Some(bmp) = Self::icon_bitmap(&mut gfx, *key, pixels) {
                        gfx.rt.DrawBitmap(
                            &bmp,
                            Some(&D2D_RECT_F {
                                left: draw_x,
                                top: (h - icon_edge) / 2.0,
                                right: draw_x + icon_edge,
                                bottom: (h + icon_edge) / 2.0,
                            }),
                            1.0,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        );
                    }
                    draw_x += icon_edge + if p.text16.is_empty() { 0.0 } else { seg_gap };
                }
                if !p.text16.is_empty() {
                    let brush: &ID2D1SolidColorBrush = match p.role {
                        Role::Fg => &gfx.fg,
                        Role::Dim => &gfx.dim,
                        Role::Accent => &gfx.accent,
                        Role::Custom(c) => {
                            gfx.custom.SetColor(&col(c, 1.0));
                            &gfx.custom
                        }
                    };
                    gfx.rt.DrawText(
                        &p.text16,
                        &gfx.fmt,
                        &D2D_RECT_F {
                            left: draw_x,
                            top: 0.0,
                            right: draw_x + p.width,
                            bottom: h,
                        },
                        brush,
                        Default::default(),
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
                // Claim half the inter-segment gap on each side so there is no
                // dead strip between adjacent targets.
                self.hits.push(HitRect {
                    slot: p.slot,
                    seg: p.seg,
                    left: x - seg_gap / 2.0,
                    right: x + p.cell + seg_gap / 2.0,
                });
                cursor[p.slot] = x + p.cell + seg_gap;
            }

            if gfx.rt.EndDraw(None, None).is_ok() {
                self.gfx = Some(gfx);
            }
        }
    }

    fn hit(&self, x: f32) -> Option<(usize, usize)> {
        self.hits
            .iter()
            .find(|h| x >= h.left && x <= h.right)
            .map(|h| (h.slot, h.seg))
    }

    fn on_click(&mut self, button: u8, x: f32) {
        if let Some((slot, seg)) = self.hit(x) {
            self.slots[slot].widget.on_click(seg, button);
            self.invalidate();
        } else if button == 2 {
            self.context_menu();
        }
    }

    /// Right-click on empty bar area: YASB-style menu.
    fn context_menu(&self) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            const ID_CONFIG: usize = 1;
            const ID_RELOAD: usize = 2;
            const ID_EXIT: usize = 3;
            const ID_TASKMGR: usize = 4;
            const ID_DISPLAY: usize = 5;
            const ID_SOUND: usize = 6;
            const ID_PERSONALIZE: usize = 7;
            let title: Vec<u16> = concat!("optim-bar ", env!("CARGO_PKG_VERSION"))
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(title.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, ID_TASKMGR, w!("Task Manager"));
            let _ = AppendMenuW(menu, MF_STRING, ID_DISPLAY, w!("Display settings"));
            let _ = AppendMenuW(menu, MF_STRING, ID_SOUND, w!("Sound settings"));
            let _ = AppendMenuW(menu, MF_STRING, ID_PERSONALIZE, w!("Personalization"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, ID_CONFIG, w!("Edit config"));
            let _ = AppendMenuW(menu, MF_STRING, ID_RELOAD, w!("Reload"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit (restore taskbar)"));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Menus dismiss on outside-click only for the foreground window.
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                Some(0),
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            match cmd.0 as usize {
                ID_CONFIG => {
                    let path16: Vec<u16> = config::path()
                        .to_string_lossy()
                        .encode_utf16()
                        .chain(std::iter::once(0))
                        .collect();
                    windows::Win32::UI::Shell::ShellExecuteW(
                        None,
                        w!("open"),
                        PCWSTR(path16.as_ptr()),
                        None,
                        None,
                        windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
                    );
                }
                ID_RELOAD => {
                    // Main loop rebuilds all bars right after this message.
                    REBUILD.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                ID_EXIT => PostQuitMessage(0),
                ID_TASKMGR => shell_open(w!("taskmgr.exe")),
                ID_DISPLAY => shell_open(w!("ms-settings:display")),
                ID_SOUND => shell_open(w!("ms-settings:sound")),
                ID_PERSONALIZE => shell_open(w!("ms-settings:personalization")),
                _ => {}
            }
        }
    }

    fn on_wheel(&mut self, delta: i32, x: f32) {
        if let Some((slot, _)) = self.hit(x) {
            self.slots[slot].widget.on_wheel(delta);
            self.invalidate();
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn handle(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_TIMER => {
                    let fs = self.fullscreen_on_monitor();
                    if fs != self.fs_hidden {
                        self.fs_hidden = fs;
                        let _ = ShowWindow(self.hwnd, if fs { SW_HIDE } else { SW_SHOWNA });
                    }
                    // Self-heal the reservation: explorer recomputes work
                    // areas from its appbar registry on shell events and can
                    // briefly lose ours. Throttled so a genuinely refused
                    // reservation doesn't turn into a SETPOS storm.
                    if self.cfg.reserve
                        && !self.fs_hidden
                        && !self.work_area_ok()
                        && self.last_assert.elapsed().as_secs() >= 2
                    {
                        self.last_assert = std::time::Instant::now();
                        self.position();
                    }
                    let mut dirty = false;
                    for slot in &mut self.slots {
                        dirty |= slot.widget.tick();
                    }
                    if dirty && !self.fs_hidden {
                        self.invalidate();
                    }
                    LRESULT(0)
                }
                WM_APP_RELOAD | WM_DISPLAYCHANGE => {
                    // Config or display topology changed: main loop rebuilds
                    // every bar from scratch.
                    REBUILD.store(true, std::sync::atomic::Ordering::Relaxed);
                    LRESULT(0)
                }
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    let _ = BeginPaint(hwnd, &mut ps);
                    self.render();
                    let _ = EndPaint(hwnd, &ps);
                    LRESULT(0)
                }
                WM_ERASEBKGND => LRESULT(1),
                WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP => {
                    let x = (lparam.0 as u32 & 0xFFFF) as u16 as f32;
                    let button = match msg {
                        WM_LBUTTONUP => 0,
                        WM_MBUTTONUP => 1,
                        _ => 2,
                    };
                    self.on_click(button, x);
                    LRESULT(0)
                }
                WM_MOUSEWHEEL => {
                    // Wheel lparam is screen coords; convert to client x.
                    let mut pt = POINT {
                        x: (lparam.0 as u32 & 0xFFFF) as u16 as i16 as i32,
                        y: ((lparam.0 as u32 >> 16) & 0xFFFF) as u16 as i16 as i32,
                    };
                    let _ = ScreenToClient(hwnd, &mut pt);
                    let delta = ((wparam.0 as u32 >> 16) & 0xFFFF) as u16 as i16 as i32;
                    self.on_wheel(delta, pt.x as f32);
                    LRESULT(0)
                }
                WM_SIZE => {
                    if let Some(gfx) = &self.gfx {
                        let w = (lparam.0 as u32) & 0xFFFF;
                        let h = ((lparam.0 as u32) >> 16) & 0xFFFF;
                        let _ = gfx.rt.Resize(&D2D_SIZE_U { width: w, height: h });
                    }
                    LRESULT(0)
                }
                windows::Win32::UI::WindowsAndMessaging::WM_DPICHANGED => {
                    // Bar moved to a monitor with different DPI: rescale.
                    self.scale = ((wparam.0 as u32) & 0xFFFF) as f32 / 96.0;
                    self.gfx = None;
                    self.position();
                    self.invalidate();
                    LRESULT(0)
                }
                WM_HOTKEY => {
                    if let Some(slot) = self.slots.get_mut(wparam.0) {
                        slot.widget.on_hotkey();
                        self.invalidate();
                    }
                    LRESULT(0)
                }
                WM_APP_APPBAR => {
                    // ABN_POSCHANGED: another appbar moved; re-negotiate.
                    // (Fullscreen shows/hides run off the timer poll instead.)
                    if wparam.0 == 1 {
                        self.position();
                    }
                    LRESULT(0)
                }
                WM_DESTROY => {
                    // Main loop owns bar lifetime (rebuilds on display change),
                    // so no PostQuitMessage here.
                    self.remove_appbar();
                    LRESULT(0)
                }
                m if m == taskbar_created_msg() => {
                    // Explorer restarted: its appbar registry is empty now.
                    self.position();
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == quit_msg() {
            // Bars live on the main thread, so this quits the loop that owns
            // the cleanup — same path as the menu's Exit. Handled ahead of the
            // per-bar dispatch below so it lands even on a half-built window.
            PostQuitMessage(0);
            return LRESULT(0);
        }
        if msg == WM_NCCREATE {
            let cs = lparam.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let bar = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Bar;
        if bar.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        (*bar).handle(hwnd, msg, wparam, lparam)
    }
}
