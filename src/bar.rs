use std::ffi::c_void;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
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
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetSystemMetrics, GetWindowLongPtrW, LoadCursorW,
    PostQuitMessage, RegisterClassW, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HWND_TOPMOST, IDC_ARROW,
    SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SW_SHOWNA, WM_APP, WM_DESTROY, WM_ERASEBKGND,
    WM_LBUTTONUP, WM_MBUTTONUP, WM_NCCREATE, WM_PAINT, WM_RBUTTONUP, WM_SIZE, WM_TIMER,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::config::{self, BarConfig};
use crate::widgets::{self, Widget};

pub const WINDOW_CLASS: PCWSTR = w!("optim_bar_window");
pub const WM_APP_RELOAD: u32 = WM_APP + 1;

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
    /// Hit-test rect from the last layout pass (physical px).
    rect: D2D_RECT_F,
}

struct Gfx {
    rt: ID2D1HwndRenderTarget,
    fg: ID2D1SolidColorBrush,
    fmt: IDWriteTextFormat,
}

pub struct Bar {
    hwnd: HWND,
    d2d_factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    gfx: Option<Gfx>,
    slots: Vec<Slot>,
    cfg: BarConfig,
    scale: f32,
}

impl Bar {
    pub fn create() -> Result<Box<Bar>> {
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
            RegisterClassW(&wc);

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
                cfg,
                scale: 1.0,
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

    fn build_slots(&mut self) {
        self.slots.clear();
        let groups = [
            (self.cfg.left.clone(), 0u8),
            (self.cfg.center.clone(), 1u8),
            (self.cfg.right.clone(), 2u8),
        ];
        for (names, side) in groups {
            for name in names {
                if let Some(widget) = widgets::build(&name, &self.cfg) {
                    self.slots.push(Slot {
                        widget,
                        side: match side {
                            0 => Side::Left,
                            1 => Side::Center,
                            _ => Side::Right,
                        },
                        rect: D2D_RECT_F::default(),
                    });
                }
            }
        }
    }

    /// Full-width strip on the primary monitor, top or bottom edge.
    fn position(&self) {
        unsafe {
            let screen_w = GetSystemMetrics(SM_CXSCREEN);
            let screen_h = GetSystemMetrics(SM_CYSCREEN);
            let h = (self.cfg.height * self.scale) as i32;
            let y = if self.cfg.position_top { 0 } else { screen_h - h };
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                y,
                screen_w,
                h,
                SWP_NOACTIVATE,
            );
        }
    }

    fn px(&self, v: f32) -> f32 {
        v * self.scale
    }

    fn build_gfx(&self) -> Result<Gfx> {
        unsafe {
            let screen_w = GetSystemMetrics(SM_CXSCREEN) as u32;
            let h = (self.cfg.height * self.scale) as u32;
            let rt = self.d2d_factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
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
            Ok(Gfx { rt, fg, fmt })
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
        let Some(gfx) = self.gfx.take() else { return };

        unsafe {
            let w = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(SM_CXSCREEN) as f32;
            let h = self.px(self.cfg.height);
            let pad = self.px(self.cfg.pad);
            let gap = self.px(GAP);
            let (bg_rgb, bg_a) = self.cfg.bg;

            gfx.rt.BeginDraw();
            gfx.rt.Clear(Some(&col(bg_rgb, bg_a.max(0.99)))); // solid for now; blur milestone later

            // Measure all texts first.
            let texts: Vec<Vec<u16>> = self
                .slots
                .iter()
                .map(|s| s.widget.text().encode_utf16().collect())
                .collect();
            let widths: Vec<f32> = texts.iter().map(|t| self.measure(&gfx, t)).collect();

            // Lay out: left run, right run (right-to-left), centered center run.
            let mut left_x = pad;
            let mut right_x = w - pad;
            let center_total: f32 = self
                .slots
                .iter()
                .zip(&widths)
                .filter(|(s, _)| matches!(s.side, Side::Center))
                .map(|(_, w)| w + gap)
                .sum::<f32>()
                - gap;
            let mut center_x = (w - center_total.max(0.0)) / 2.0;

            for (i, slot) in self.slots.iter_mut().enumerate() {
                let width = widths[i];
                let x = match slot.side {
                    Side::Left => {
                        let x = left_x;
                        left_x += width + gap;
                        x
                    }
                    Side::Center => {
                        let x = center_x;
                        center_x += width + gap;
                        x
                    }
                    Side::Right => {
                        right_x -= width;
                        let x = right_x;
                        right_x -= gap;
                        x
                    }
                };
                slot.rect = D2D_RECT_F {
                    left: x,
                    top: 0.0,
                    right: x + width,
                    bottom: h,
                };
                gfx.rt.DrawText(
                    &texts[i],
                    &gfx.fmt,
                    &slot.rect,
                    &gfx.fg,
                    Default::default(),
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }

            if gfx.rt.EndDraw(None, None).is_ok() {
                self.gfx = Some(gfx);
            }
        }
    }

    fn on_click(&mut self, button: u8, x: f32) {
        for slot in &mut self.slots {
            if x >= slot.rect.left && x <= slot.rect.right {
                slot.widget.on_click(button);
                self.invalidate();
                return;
            }
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn reload(&mut self) {
        self.cfg = config::load();
        self.build_slots();
        self.gfx = None;
        self.position();
        self.invalidate();
    }

    fn handle(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_TIMER => {
                    let mut dirty = false;
                    for slot in &mut self.slots {
                        dirty |= slot.widget.tick();
                    }
                    if dirty {
                        self.invalidate();
                    }
                    LRESULT(0)
                }
                WM_APP_RELOAD => {
                    self.reload();
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
                WM_SIZE => {
                    if let Some(gfx) = &self.gfx {
                        let w = (lparam.0 as u32) & 0xFFFF;
                        let h = ((lparam.0 as u32) >> 16) & 0xFFFF;
                        let _ = gfx.rt.Resize(&D2D_SIZE_U { width: w, height: h });
                    }
                    LRESULT(0)
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
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
