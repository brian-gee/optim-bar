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
                hits: Vec::new(),
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
            let w = GetSystemMetrics(SM_CXSCREEN) as f32;
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
            let center_total: f32 = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| matches!(s.side, Side::Center) )
                .map(|(i, _)| slot_widths[i] + gap)
                .sum::<f32>()
                - gap;
            let mut center_x = (w - center_total.max(0.0)) / 2.0;
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
