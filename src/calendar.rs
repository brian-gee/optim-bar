//! Calendar flyout for the clock widget: month grid with today highlighted,
//! ‹ › (or mouse wheel) to page months, Esc / click-away to dismiss.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
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
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_LEFT, VK_RIGHT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowLongPtrW, LoadCursorW,
    RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, CREATESTRUCTW,
    GWLP_USERDATA, IDC_ARROW, SW_SHOW, WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::statspop::Style;

const CLASS: PCWSTR = w!("optim_bar_calendar");
const CELL: f32 = 36.0;
const TITLE_H: f32 = 40.0;
const WEEKDAY_H: f32 = 26.0;
const PAD: f32 = 12.0;
const COLS: f32 = 7.0;

static OPEN: AtomicIsize = AtomicIsize::new(0);
static CLOSED_AT: AtomicIsize = AtomicIsize::new(0);

fn now_ticks() -> isize {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() as isize }
}

pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed) != 0
}

fn close() {
    let h = OPEN.swap(0, Ordering::Relaxed);
    if h != 0 {
        CLOSED_AT.store(now_ticks(), Ordering::Relaxed);
        unsafe {
            let _ = DestroyWindow(HWND(h as *mut _));
        }
    }
}

fn col(v: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((v >> 16) & 0xFF) as f32 / 255.0,
        g: ((v >> 8) & 0xFF) as f32 / 255.0,
        b: (v & 0xFF) as f32 / 255.0,
        a,
    }
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
    }
}

/// Sakamoto: 0 = Sunday.
fn weekday(y: i32, m: u32, d: u32) -> u32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    ((y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d as i32).rem_euclid(7)) as u32
}

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const WEEKDAYS: [&str; 7] = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];

struct Gfx {
    rt: ID2D1HwndRenderTarget,
    fmt: IDWriteTextFormat,      // day numbers, centered
    fmt_title: IDWriteTextFormat, // month-year, centered, bold
    fg: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
}

struct Cal {
    style: Style,
    view_y: i32,
    view_m: u32, // 1-12
    today: (i32, u32, u32),
    scale: f32,
    gfx: Option<Gfx>,
}

impl Cal {
    fn page(&mut self, delta: i32) {
        let mut m = self.view_m as i32 + delta;
        while m < 1 {
            m += 12;
            self.view_y -= 1;
        }
        while m > 12 {
            m -= 12;
            self.view_y += 1;
        }
        self.view_m = m as u32;
    }

    fn size(&self) -> (f32, f32) {
        let w = PAD * 2.0 + COLS * CELL;
        let h = PAD * 2.0 + TITLE_H + WEEKDAY_H + 6.0 * CELL;
        (w, h)
    }

    fn paint(&mut self, hwnd: HWND) {
        unsafe {
            if self.gfx.is_none() {
                self.gfx = self.build_gfx(hwnd).ok();
            }
            let Some(gfx) = &self.gfx else { return };
            let s = |v: f32| v * self.scale;
            gfx.rt.BeginDraw();
            gfx.rt
                .Clear(Some(&col(self.style.bg.0, 1.0f32.min(self.style.bg.1 + 0.1))));

            let (w, _) = self.size();
            let w = s(w);
            let text = |txt: &str, fmt: &IDWriteTextFormat, rect: &D2D_RECT_F, brush: &ID2D1SolidColorBrush| {
                let t16: Vec<u16> = txt.encode_utf16().collect();
                gfx.rt.DrawText(
                    &t16,
                    fmt,
                    rect,
                    brush,
                    Default::default(),
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            };

            // Title with paging chevrons.
            let title_rect = D2D_RECT_F {
                left: s(PAD),
                top: s(PAD),
                right: w - s(PAD),
                bottom: s(PAD + TITLE_H),
            };
            text(
                &format!("{} {}", MONTHS[(self.view_m - 1) as usize], self.view_y),
                &gfx.fmt_title,
                &title_rect,
                &gfx.fg,
            );
            let chev = |txt: &str, left: f32, right: f32| {
                let rect = D2D_RECT_F {
                    left,
                    right,
                    ..title_rect
                };
                text(txt, &gfx.fmt_title, &rect, &gfx.dim);
            };
            chev("\u{2039}", s(PAD), s(PAD + 36.0)); // ‹
            chev("\u{203A}", w - s(PAD + 36.0), w - s(PAD)); // ›

            // Weekday header.
            let grid_top = s(PAD + TITLE_H);
            for (i, wd) in WEEKDAYS.iter().enumerate() {
                let rect = D2D_RECT_F {
                    left: s(PAD) + i as f32 * s(CELL),
                    top: grid_top,
                    right: s(PAD) + (i + 1) as f32 * s(CELL),
                    bottom: grid_top + s(WEEKDAY_H),
                };
                text(wd, &gfx.fmt, &rect, &gfx.dim);
            }

            // Day grid: 6 rows starting at the Sunday on/before the 1st.
            let first_wd = weekday(self.view_y, self.view_m, 1);
            let dim_prev = {
                let (py, pm) = if self.view_m == 1 {
                    (self.view_y - 1, 12)
                } else {
                    (self.view_y, self.view_m - 1)
                };
                days_in_month(py, pm)
            };
            let dcount = days_in_month(self.view_y, self.view_m);
            let cells_top = grid_top + s(WEEKDAY_H);
            for cell in 0..42u32 {
                let (row_i, col_i) = (cell / 7, cell % 7);
                let rect = D2D_RECT_F {
                    left: s(PAD) + col_i as f32 * s(CELL),
                    top: cells_top + row_i as f32 * s(CELL),
                    right: s(PAD) + (col_i + 1) as f32 * s(CELL),
                    bottom: cells_top + (row_i + 1) as f32 * s(CELL),
                };
                // Which day does this cell hold?
                let (day, in_month) = if cell < first_wd {
                    (dim_prev - first_wd + 1 + cell, false)
                } else if cell - first_wd + 1 <= dcount {
                    (cell - first_wd + 1, true)
                } else {
                    (cell - first_wd - dcount + 1, false)
                };
                let is_today = in_month
                    && (self.view_y, self.view_m, day) == self.today;
                if is_today {
                    let inset = s(3.0);
                    gfx.rt.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: rect.left + inset,
                                top: rect.top + inset,
                                right: rect.right - inset,
                                bottom: rect.bottom - inset,
                            },
                            radiusX: s(10.0),
                            radiusY: s(10.0),
                        },
                        &gfx.accent,
                    );
                }
                let brush = if is_today {
                    // Dark text on the accent pill.
                    &gfx.today_text()
                } else if in_month {
                    &gfx.fg
                } else {
                    &gfx.dim
                };
                text(&day.to_string(), &gfx.fmt, &rect, brush);
            }

            if gfx.rt.EndDraw(None, None).is_err() {
                self.gfx = None;
            }
        }
    }

    fn build_gfx(&self, hwnd: HWND) -> windows::core::Result<Gfx> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut rect = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
            let rt = factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    dpiX: 96.0,
                    dpiY: 96.0,
                    ..Default::default()
                },
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd,
                    pixelSize: D2D_SIZE_U {
                        width: (rect.right - rect.left) as u32,
                        height: (rect.bottom - rect.top) as u32,
                    },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )?;
            let font16: Vec<u16> = self
                .style
                .font
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let font = PCWSTR(font16.as_ptr());
            let mk = |sz: f32, weight| -> windows::core::Result<IDWriteTextFormat> {
                let f = dwrite.CreateTextFormat(
                    font,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    sz,
                    w!("en-us"),
                )?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                Ok(f)
            };
            let size = self.style.font_size * self.scale;
            Ok(Gfx {
                fg: rt.CreateSolidColorBrush(&col(self.style.fg, 1.0), None)?,
                dim: rt.CreateSolidColorBrush(&col(self.style.dim, 1.0), None)?,
                accent: rt.CreateSolidColorBrush(&col(self.style.accent, 1.0), None)?,
                fmt: mk(size, DWRITE_FONT_WEIGHT_NORMAL)?,
                fmt_title: mk(size * 1.15, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                rt,
            })
        }
    }
}

impl Gfx {
    /// Dark text brush for the today pill (created per paint; cheap).
    fn today_text(&self) -> ID2D1SolidColorBrush {
        unsafe {
            self.rt
                .CreateSolidColorBrush(&col(0x11111B, 1.0), None)
                .unwrap_or_else(|_| self.fg.clone())
        }
    }
}

/// Toggle the calendar under the cursor (i.e. under the clock).
pub fn toggle(style: Style) {
    if is_open() {
        close();
        return;
    }
    if now_ticks() - CLOSED_AT.load(Ordering::Relaxed) < 400 {
        return;
    }
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };
        let wc = WNDCLASSW {
            style: Default::default(),
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let t = GetLocalTime();
        let cal = Box::new(Cal {
            style,
            view_y: t.wYear as i32,
            view_m: t.wMonth as u32,
            today: (t.wYear as i32, t.wMonth as u32, t.wDay as u32),
            scale: 1.0,
            gfx: None,
        });
        let (w_l, h_l) = cal.size();

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (w_px, h_px) = (w_l as i32, h_l as i32);
        let x = (pt.x - w_px / 2)
            .max(mi.rcWork.left + 4)
            .min(mi.rcWork.right - w_px - 4);
        let y = (mi.rcWork.top + 4).max(pt.y + 8).min(mi.rcWork.bottom - h_px - 4);

        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("calendar"),
            WS_POPUP,
            x,
            y,
            w_px,
            h_px,
            None,
            None,
            Some(hinstance.into()),
            Some(&*cal as *const Cal as *const c_void),
        ) else {
            return;
        };
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            std::mem::size_of_val(&corner) as u32,
        );
        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        if (scale - 1.0).abs() > 0.01 {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Cal;
            if !p.is_null() {
                (*p).scale = scale;
                let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                    hwnd,
                    None,
                    x,
                    y,
                    (w_l * scale) as i32,
                    (h_l * scale) as i32,
                    windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                        | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                );
            }
        }
        std::mem::forget(cal); // owned by the window; freed in WM_NCDESTROY

        OPEN.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = lparam.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let cal = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Cal;
        if cal.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                (*cal).paint(hwnd);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONUP => {
                // Chevron zones page the month; anywhere else is inert.
                let x = (lparam.0 as u32 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 as u32 >> 16) & 0xFFFF) as i16 as f32;
                let s = (*cal).scale;
                if y <= (PAD + TITLE_H) * s {
                    let (w_l, _) = (*cal).size();
                    if x <= (PAD + 40.0) * s {
                        (*cal).page(-1);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    } else if x >= (w_l - PAD - 40.0) * s {
                        (*cal).page(1);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                (*cal).page(if delta > 0 { -1 } else { 1 });
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                match wparam.0 as u16 {
                    v if v == VK_ESCAPE.0 => close(),
                    v if v == VK_LEFT.0 => {
                        (*cal).page(-1);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    v if v == VK_RIGHT.0 => {
                        (*cal).page(1);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                close();
                LRESULT(0)
            }
            WM_NCDESTROY => {
                drop(Box::from_raw(cal));
                OPEN.store(0, Ordering::Relaxed);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
