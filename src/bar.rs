use std::ffi::c_void;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
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
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetCursorPos,
    GetWindowLongPtrW, LoadCursorW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW, TrackPopupMenu,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HWND_TOPMOST, IDC_ARROW, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, SPIF_SENDCHANGE, SPI_SETWORKAREA, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNA,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_ERASEBKGND, WM_LBUTTONUP, WM_MBUTTONUP, WM_NCCREATE, WM_PAINT, WM_RBUTTONUP, WM_SIZE,
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

use crate::config::{self, BarConfig};
use crate::widgets::{self, Role, Segment, Widget};

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
}

unsafe extern "system" fn mon_cb(
    hmon: windows::Win32::Graphics::Gdi::HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _rc: *mut windows::Win32::Foundation::RECT,
    lparam: LPARAM,
) -> windows::core::BOOL {
    let out = &mut *(lparam.0 as *mut Vec<Mon>);
    let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
        cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(hmon, &mut mi);
    out.push(Mon {
        handle: hmon.0 as isize,
        rect: mi.rcMonitor,
        primary: mi.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
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

/// Creates one bar per monitor (or primary only), per the freshly-read config.
pub fn create_all() -> Vec<Box<Bar>> {
    let mons = monitors();
    let mut bars = Vec::new();
    for (i, mon) in mons.iter().enumerate() {
        if i > 0 {
            let cfg = config::load();
            if !cfg.all_monitors {
                break;
            }
        }
        match Bar::create(i, mon.handle, mon.rect) {
            Ok(b) => bars.push(b),
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
            });
            bar.build_slots();

            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                WINDOW_CLASS,
                w!("optim-bar"),
                WS_POPUP,
                0, 0, 100, 36,
                None,
                None,
                Some(hinstance.into()),
                Some(&*bar as *const Bar as *const c_void),
            )?;
            bar.hwnd = hwnd;
            bar.scale = GetDpiForWindow(hwnd) as f32 / 96.0;
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
                    self.slots.push(Slot {
                        widget,
                        side: match side {
                            0 => Side::Left,
                            1 => Side::Center,
                            _ => Side::Right,
                        },
                    });
                }
            }
        }
    }

    /// Full-width strip on the monitor's top or bottom edge.
    /// With `reserve = true` (default) the monitor's work area is shrunk
    /// past the bar so maximized/tiled windows stop at its edge.
    fn position(&self) {
        unsafe {
            let mr = self.mon_rect;
            let h = (self.cfg.height * self.scale) as i32;
            let y = if self.cfg.position_top { mr.top } else { mr.bottom - h };

            if self.cfg.reserve {
                self.set_work_area(true);
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
        }
    }

    /// Sets this monitor's work area to exclude (or re-include) the bar.
    /// The classic route — SHAppBarMessage — sends the ABM protocol to the
    /// Shell_TrayWnd window, which is *us* since tray.rs took that class
    /// over; we'd be asking ourselves and dropping the request. Write the
    /// work area directly instead: SPI_SETWORKAREA applies to whichever
    /// monitor contains the passed rect, and SPIF_SENDCHANGE broadcasts
    /// WM_SETTINGCHANGE so apps (and komorebi) re-read it.
    fn set_work_area(&self, reserve: bool) {
        unsafe {
            let mut rc = self.mon_rect;
            if reserve {
                let h = (self.cfg.height * self.scale) as i32;
                if self.cfg.position_top {
                    rc.top += h;
                } else {
                    rc.bottom -= h;
                }
            }
            let _ = SystemParametersInfoW(
                SPI_SETWORKAREA,
                0,
                Some(&mut rc as *mut _ as *mut c_void),
                SPIF_SENDCHANGE,
            );
        }
    }

    fn px(&self, v: f32) -> f32 {
        v * self.scale
    }

    /// True when the foreground window fully covers this bar's monitor
    /// (borderless or exclusive fullscreen). Real appbars get told via
    /// ABN_FULLSCREENAPP; we aren't one, so use the same geometric test
    /// optim's game mode uses, polled from the 250 ms timer.
    fn fullscreen_on_monitor(&self) -> bool {
        use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetClassNameW, GetForegroundWindow, GetWindowRect,
        };
        unsafe {
            let fg = GetForegroundWindow();
            if fg == HWND::default() {
                return false;
            }
            if MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST).0 as isize != self.monitor {
                return false;
            }
            // The desktop and shell surfaces are monitor-sized but aren't games.
            let mut class = [0u16; 64];
            let n = GetClassNameW(fg, &mut class) as usize;
            let class = String::from_utf16_lossy(&class[..n]);
            if matches!(class.as_str(), "WorkerW" | "Progman" | "Shell_TrayWnd") {
                return false;
            }
            let mut rect = RECT::default();
            if GetWindowRect(fg, &mut rect).is_err() {
                return false;
            }
            let m = self.mon_rect;
            rect.left <= m.left
                && rect.top <= m.top
                && rect.right >= m.right
                && rect.bottom >= m.bottom
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
                width: f32,
                role: Role,
                icon: Option<(u64, std::sync::Arc<Vec<u8>>)>,
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
                    if gi > 0 {
                        total += seg_gap;
                    }
                    total += width;
                    pieces.push(Piece {
                        slot: si,
                        seg: gi,
                        text16,
                        width,
                        role: seg.role,
                        icon: seg.icon,
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
                let mut draw_x = x;
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
                self.hits.push(HitRect {
                    slot: p.slot,
                    seg: p.seg,
                    left: x,
                    right: x + p.width,
                });
                cursor[p.slot] = x + p.width + seg_gap;
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
            let title: Vec<u16> = concat!("optim-bar ", env!("CARGO_PKG_VERSION"))
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(title.as_ptr()));
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
                WM_DESTROY => {
                    // Main loop owns bar lifetime (rebuilds on display change),
                    // so no PostQuitMessage here.
                    self.set_work_area(false);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
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
