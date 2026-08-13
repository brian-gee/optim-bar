//! Tray overflow flyout: a popup grid of the tray icons, dropped under the
//! bar's chevron (Windows 11 style). One at a time; dismisses on outside
//! click (WM_KILLFOCUS), Esc, or after forwarding a click to an icon.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, MonitorFromPoint, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowLongPtrW,
    GetWindowRect, LoadCursorW, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW,
    ShowWindow, WindowFromPoint, CREATESTRUCTW, GWLP_USERDATA, IDC_ARROW, SW_SHOW,
    WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_MBUTTONUP, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::bar::ICON_SRC;
use crate::tray::{self, TrayIcon};

const CLASS: PCWSTR = w!("optim_bar_flyout");
const COLS: usize = 4;
const CELL: f32 = 40.0; // logical cell edge
const ICON: f32 = 20.0; // logical icon edge inside a cell
const PAD: f32 = 8.0;

static OPEN: AtomicIsize = AtomicIsize::new(0);

pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed) != 0
}

fn close() {
    let h = OPEN.swap(0, Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = DestroyWindow(HWND(h as *mut _));
        }
    }
}

struct Gfx {
    rt: ID2D1HwndRenderTarget,
    dim: ID2D1SolidColorBrush,
    /// One per icon, aligned with `icons`; None = no pixels (dot fallback).
    bitmaps: Vec<Option<ID2D1Bitmap>>,
}

struct Flyout {
    icons: Vec<TrayIcon>,
    bg: (u32, f32),
    dim: u32,
    surface: u32,
    cols: usize,
    rows: usize,
    scale: f32,
    gfx: Option<Gfx>,
}

fn col(v: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((v >> 16) & 0xFF) as f32 / 255.0,
        g: ((v >> 8) & 0xFF) as f32 / 255.0,
        b: (v & 0xFF) as f32 / 255.0,
        a,
    }
}

/// Opens the flyout under the cursor (or closes an already-open one).
/// `icons` is the visible tray-icon snapshot; colors come from the bar cfg.
pub fn toggle(icons: Vec<TrayIcon>, bg: (u32, f32), dim: u32, surface: u32) {
    if is_open() {
        close();
        return;
    }
    if icons.is_empty() {
        return;
    }
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else { return };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc); // fails harmlessly after the first open

        // Anchor: the chevron click position. The bar window under the
        // cursor gives us the bar's edge and the right DPI.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let bar_hwnd = WindowFromPoint(pt);
        let scale = GetDpiForWindow(bar_hwnd).max(96) as f32 / 96.0;
        let mut bar_rc = RECT::default();
        let _ = GetWindowRect(bar_hwnd, &mut bar_rc);

        let n = icons.len();
        let cols = n.min(COLS);
        let rows = n.div_ceil(cols);
        let w = (2.0 * PAD + cols as f32 * CELL) * scale;
        let h = (2.0 * PAD + rows as f32 * CELL) * scale;
        let margin = (4.0 * scale) as i32;

        // Below a top bar, above a bottom bar; clamped to the monitor.
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let m = mi.rcMonitor;
        let top_bar = bar_rc.top <= m.top + (m.bottom - m.top) / 2;
        let y = if top_bar {
            bar_rc.bottom + margin
        } else {
            bar_rc.top - margin - h as i32
        };
        let x = (pt.x - w as i32 / 2)
            .min(m.right - w as i32 - margin)
            .max(m.left + margin);

        let fly = Box::new(Flyout {
            icons,
            bg,
            dim,
            surface,
            cols,
            rows,
            scale,
            gfx: None,
        });
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("optim-bar tray"),
            WS_POPUP,
            x,
            y,
            w as i32,
            h as i32,
            None,
            None,
            Some(hinstance.into()),
            Some(Box::into_raw(fly) as *const c_void),
        ) else {
            return;
        };
        OPEN.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOW);
        // Take focus so an outside click lands as WM_KILLFOCUS and dismisses.
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
}

impl Flyout {
    fn build_gfx(&self, hwnd: HWND) -> Option<Gfx> {
        unsafe {
            let factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
            let w = (2.0 * PAD + self.cols as f32 * CELL) * self.scale;
            let h = (2.0 * PAD + self.rows as f32 * CELL) * self.scale;
            let rt = factory
                .CreateHwndRenderTarget(
                    &D2D1_RENDER_TARGET_PROPERTIES {
                        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                        dpiX: 96.0,
                        dpiY: 96.0,
                        ..Default::default()
                    },
                    &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                        hwnd,
                        pixelSize: D2D_SIZE_U {
                            width: w as u32,
                            height: h as u32,
                        },
                        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                    },
                )
                .ok()?;
            let dim = rt.CreateSolidColorBrush(&col(self.dim, 1.0), None).ok()?;
            let bitmaps = self
                .icons
                .iter()
                .map(|i| {
                    let px = i.pixels.as_ref()?;
                    rt.CreateBitmap(
                        D2D_SIZE_U {
                            width: ICON_SRC as u32,
                            height: ICON_SRC as u32,
                        },
                        Some(px.as_ptr() as _),
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
                    .ok()
                })
                .collect();
            Some(Gfx { rt, dim, bitmaps })
        }
    }

    fn render(&mut self, hwnd: HWND) {
        if self.gfx.is_none() {
            self.gfx = self.build_gfx(hwnd);
        }
        let Some(gfx) = &self.gfx else { return };
        unsafe {
            let pad = PAD * self.scale;
            let cell = CELL * self.scale;
            let icon = ICON * self.scale;
            gfx.rt.BeginDraw();
            gfx.rt.Clear(Some(&col(self.bg.0, 1.0)));
            // Hairline border so the popup reads against dark windows.
            let w = 2.0 * pad + self.cols as f32 * cell;
            let h = 2.0 * pad + self.rows as f32 * cell;
            gfx.dim.SetColor(&col(self.surface, 1.0));
            gfx.rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: 0.5,
                        top: 0.5,
                        right: w - 0.5,
                        bottom: h - 0.5,
                    },
                    radiusX: 4.0,
                    radiusY: 4.0,
                },
                &gfx.dim,
                1.0,
                None,
            );
            gfx.dim.SetColor(&col(self.dim, 1.0));
            for (i, bmp) in gfx.bitmaps.iter().enumerate() {
                let cx = pad + (i % self.cols) as f32 * cell + cell / 2.0;
                let cy = pad + (i / self.cols) as f32 * cell + cell / 2.0;
                match bmp {
                    Some(b) => {
                        gfx.rt.DrawBitmap(
                            b,
                            Some(&D2D_RECT_F {
                                left: cx - icon / 2.0,
                                top: cy - icon / 2.0,
                                right: cx + icon / 2.0,
                                bottom: cy + icon / 2.0,
                            }),
                            1.0,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        );
                    }
                    None => {
                        let r = icon / 5.0;
                        gfx.rt.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: D2D_RECT_F {
                                    left: cx - r,
                                    top: cy - r,
                                    right: cx + r,
                                    bottom: cy + r,
                                },
                                radiusX: r,
                                radiusY: r,
                            },
                            &gfx.dim,
                        );
                    }
                }
            }
            let _ = gfx.rt.EndDraw(None, None);
        }
    }

    /// Cell index under a client-space point, if it maps to an icon.
    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let pad = PAD * self.scale;
        let cell = CELL * self.scale;
        if x < pad || y < pad {
            return None;
        }
        let c = ((x - pad) / cell) as usize;
        let r = ((y - pad) / cell) as usize;
        if c >= self.cols || r >= self.rows {
            return None;
        }
        let idx = r * self.cols + c;
        (idx < self.icons.len()).then_some(idx)
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = lparam.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Flyout;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                (*ptr).render(hwnd);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP => {
                let x = (lparam.0 as u32 & 0xFFFF) as u16 as f32;
                let y = ((lparam.0 as u32 >> 16) & 0xFFFF) as u16 as f32;
                if let Some(idx) = (*ptr).hit(x, y) {
                    let button = match msg {
                        WM_LBUTTONUP => 0,
                        WM_MBUTTONUP => 1,
                        _ => 2,
                    };
                    let icon = (&(*ptr).icons)[idx].clone();
                    // send_button foregrounds the owner, which also costs us
                    // focus — close first so KILLFOCUS doesn't double-destroy.
                    close();
                    tray::send_button(&icon, button);
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
                close();
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                close();
                LRESULT(0)
            }
            WM_NCDESTROY => {
                OPEN.compare_exchange(hwnd.0 as isize, 0, Ordering::Relaxed, Ordering::Relaxed)
                    .ok();
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
