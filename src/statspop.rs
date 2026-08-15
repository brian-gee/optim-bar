//! System + weather dropdown: opened by clicking any of the stat widgets
//! (cpu / mem / gpu_temp / cpu_temp). Live-refreshes once a second while
//! open. Weather rows come from the shared weather state; the airing
//! section shows the next stretches worth opening the windows for.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_TRAILING,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::{GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowLongPtrW, KillTimer,
    LoadCursorW, RegisterClassW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow,
    CREATESTRUCTW, GWLP_USERDATA, IDC_ARROW, SW_SHOW, WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::weather::{self, compass, feeds_windows};

const CLASS: PCWSTR = w!("optim_bar_statspop");
const WIDTH: f32 = 300.0;
const ROW_H: f32 = 24.0;
const BAR_H: f32 = 26.0;
const WIND_H: f32 = 58.0;
const HEADER_H: f32 = 32.0;
const BIG_H: f32 = 40.0;
const PAD: f32 = 14.0;

static OPEN: AtomicIsize = AtomicIsize::new(0);
/// Tick of the last close, so the widget click that *caused* a focus-loss
/// close doesn't immediately reopen the popup (kill-focus fires first).
static CLOSED_AT: AtomicIsize = AtomicIsize::new(0);

pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed) != 0
}

fn now_ticks() -> isize {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() as isize }
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

/// Red -> yellow -> green for a 0-100 score (Catppuccin endpoints).
fn score_color(score: u32) -> u32 {
    const RED: (u32, u32, u32) = (0xF3, 0x8B, 0xA8);
    const YEL: (u32, u32, u32) = (0xF9, 0xE2, 0xAF);
    const GRN: (u32, u32, u32) = (0xA6, 0xE3, 0xA1);
    let lerp = |a: (u32, u32, u32), b: (u32, u32, u32), t: f32| -> u32 {
        let c = |x: u32, y: u32| (x as f32 + (y as f32 - x as f32) * t) as u32;
        (c(a.0, b.0) << 16) | (c(a.1, b.1) << 8) | c(a.2, b.2)
    };
    let s = score.min(100) as f32;
    if s < 50.0 {
        lerp(RED, YEL, s / 50.0)
    } else {
        lerp(YEL, GRN, (s - 50.0) / 50.0)
    }
}

/// Visual + data parameters, captured from BarConfig by the widgets.
#[derive(Clone)]
pub struct Style {
    pub bg: (u32, f32),
    pub fg: u32,
    pub dim: u32,
    pub accent: u32,
    pub surface: u32,
    pub font: String,
    pub font_size: f32,
    pub lhm_sensor: String, // empty = skip CPU temp row
}

impl Style {
    pub fn from_cfg(cfg: &crate::config::BarConfig) -> Style {
        // The CPU-temp sensor lives in whatever section is the lhm widget;
        // scan for the first `type = lhm` widget section.
        let sensor = ["cpu_temp", "lhm"]
            .iter()
            .filter_map(|n| cfg.values.get(&format!("widget.{n}"), "sensor_id"))
            .next()
            .unwrap_or_default();
        Style {
            bg: cfg.bg,
            fg: cfg.fg,
            dim: cfg.dim,
            accent: cfg.accent,
            surface: cfg.surface,
            font: cfg.font.clone(),
            font_size: cfg.font_size,
            lhm_sensor: sensor,
        }
    }
}

enum Row {
    Header(String),
    /// label, value, value color override (None = fg)
    KV(String, String, Option<u32>),
    /// Labeled slim progress bar with a right-aligned value.
    Bar {
        label: String,
        value: String,
        frac: f32,
    },
    /// Wind block: compass rose + two text lines.
    Wind {
        kmh: f64,
        dir: f64,
        temp_c: f64,
        humidity: f64,
        feeds: bool,
        bearings: Vec<f64>,
    },
    /// Emphasized row: tinted pill, label + big bold colored value.
    Big(String, String, u32),
}

#[repr(C)]
struct NvmlUtil {
    gpu: u32,
    memory: u32,
}

struct Nvml {
    device: *mut c_void,
    get_temperature: unsafe extern "C" fn(*mut c_void, i32, *mut u32) -> i32,
    get_utilization: unsafe extern "C" fn(*mut c_void, *mut NvmlUtil) -> i32,
}
unsafe impl Send for Nvml {}

fn nvml_load() -> Option<Nvml> {
    unsafe {
        let lib = LoadLibraryW(w!("nvml.dll")).ok()?;
        let init: unsafe extern "C" fn() -> i32 =
            std::mem::transmute(GetProcAddress(lib, PCSTR(b"nvmlInit_v2\0".as_ptr()))?);
        let by_index: unsafe extern "C" fn(u32, *mut *mut c_void) -> i32 = std::mem::transmute(
            GetProcAddress(lib, PCSTR(b"nvmlDeviceGetHandleByIndex_v2\0".as_ptr()))?,
        );
        let get_temperature = std::mem::transmute(GetProcAddress(
            lib,
            PCSTR(b"nvmlDeviceGetTemperature\0".as_ptr()),
        )?);
        let get_utilization = std::mem::transmute(GetProcAddress(
            lib,
            PCSTR(b"nvmlDeviceGetUtilizationRates\0".as_ptr()),
        )?);
        if init() != 0 {
            return None;
        }
        let mut device: *mut c_void = std::ptr::null_mut();
        if by_index(0, &mut device) != 0 {
            return None;
        }
        Some(Nvml {
            device,
            get_temperature,
            get_utilization,
        })
    }
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

struct Gfx {
    rt: ID2D1HwndRenderTarget,
    fmt: IDWriteTextFormat,
    fmt_bold: IDWriteTextFormat,
    fmt_right: IDWriteTextFormat,
    fmt_big: IDWriteTextFormat,
    fg: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
}

struct Pop {
    style: Style,
    rows: Vec<Row>,
    scale: f32,
    gfx: Option<Gfx>,
    nvml: Option<Nvml>,
    cpu_last: (u64, u64), // idle, busy
    cpu_pct: Option<u64>,
    /// First timer tick fires fast (150 ms) purely to get the second CPU
    /// sample early; after it we settle into the 1 s cadence.
    fast_tick: bool,
    /// Row index of the "set location" hint, when shown; clicking it opens
    /// the config file.
    hint_row: Option<usize>,
    lhm_temp: Arc<Mutex<Option<f64>>>,
    lhm_alive: Arc<AtomicBool>,
}

fn col(v: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((v >> 16) & 0xFF) as f32 / 255.0,
        g: ((v >> 8) & 0xFF) as f32 / 255.0,
        b: (v & 0xFF) as f32 / 255.0,
        a,
    }
}

/// Sakamoto's weekday algorithm; 0 = Sunday.
fn weekday(y: i32, m: i32, d: i32) -> usize {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    ((y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d) % 7) as usize
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// "2026-08-14T07:00" -> ("Fri", 7). Falls back to ("?", 0).
fn day_hour(iso: &str) -> (&'static str, u32) {
    let parse = || -> Option<(&'static str, u32)> {
        let (date, time) = iso.split_once('T')?;
        let mut it = date.split('-');
        let (y, m, d) = (
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        );
        let hour = time.get(0..2)?.parse().ok()?;
        Some((DAYS[weekday(y, m, d)], hour))
    };
    parse().unwrap_or(("?", 0))
}

/// Current local time as an ISO-ish sortable "YYYY-MM-DDTHH:MM".
fn now_key() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute
    )
}

impl Pop {
    fn build_rows(&mut self) {
        let mut rows = Vec::new();
        rows.push(Row::Header("System".into()));

        // CPU % — needs two samples; first tick shows a placeholder.
        unsafe {
            let (mut idle, mut kernel, mut user) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default());
            if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
                let idle_v = ft(idle);
                let busy_v = ft(kernel) - idle_v + ft(user);
                let (li, lb) = self.cpu_last;
                let (di, db) = (idle_v.saturating_sub(li), busy_v.saturating_sub(lb));
                if li != 0 && di + db > 0 {
                    self.cpu_pct = Some((db * 100 / (di + db)).min(100));
                }
                self.cpu_last = (idle_v, busy_v);
            }
        }
        let cpu_temp = self.lhm_temp.lock().ok().and_then(|t| *t);
        let cpu_val = match (self.cpu_pct, cpu_temp) {
            (Some(p), Some(t)) => format!("{p}%  ·  {}\u{b0}", t.round() as i64),
            (Some(p), None) => format!("{p}%"),
            (None, Some(t)) => format!("\u{2026}  ·  {}\u{b0}", t.round() as i64),
            (None, None) => "\u{2026}".into(),
        };
        rows.push(Row::Bar {
            label: "CPU".into(),
            value: cpu_val,
            frac: self.cpu_pct.unwrap_or(0) as f32 / 100.0,
        });

        unsafe {
            let mut ms = MEMORYSTATUSEX {
                dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
                ..Default::default()
            };
            if GlobalMemoryStatusEx(&mut ms).is_ok() {
                let used = (ms.ullTotalPhys - ms.ullAvailPhys) as f64 / (1 << 30) as f64;
                let total = ms.ullTotalPhys as f64 / (1 << 30) as f64;
                rows.push(Row::Bar {
                    label: "RAM".into(),
                    value: format!("{used:.1}/{total:.0} GB"),
                    frac: ms.dwMemoryLoad as f32 / 100.0,
                });
            }
        }

        if let Some(nvml) = &self.nvml {
            unsafe {
                let mut util = NvmlUtil { gpu: 0, memory: 0 };
                let mut temp = 0u32;
                let u_ok = (nvml.get_utilization)(nvml.device, &mut util) == 0;
                let t_ok = (nvml.get_temperature)(nvml.device, 0, &mut temp) == 0;
                if u_ok || t_ok {
                    let value = match (u_ok, t_ok) {
                        (true, true) => format!("{}%  ·  {temp}\u{b0}", util.gpu),
                        (true, false) => format!("{}%", util.gpu),
                        (false, true) => format!("{temp}\u{b0}"),
                        _ => unreachable!(),
                    };
                    rows.push(Row::Bar {
                        label: "GPU".into(),
                        value,
                        frac: if u_ok { util.gpu as f32 / 100.0 } else { 0.0 },
                    });
                }
            }
        }

        // Weather section from the shared state.
        self.hint_row = None;
        let state = weather::state().lock();
        if let Ok(state) = state {
            rows.push(Row::Header("Outside".into()));
            if !state.configured {
                // No location set — coordinates never ship as defaults, so
                // this is the first-run state. Click opens the config.
                self.hint_row = Some(rows.len());
                rows.push(Row::KV(
                    "Weather off \u{2014} click to set location".into(),
                    String::new(),
                    Some(self.style.accent),
                ));
                self.rows = rows;
                return;
            }
            match &state.current {
                Some(c) => {
                    rows.push(Row::Wind {
                        kmh: c.wind_kmh,
                        dir: c.wind_dir,
                        temp_c: c.temp_c,
                        humidity: c.humidity,
                        feeds: feeds_windows(&state.bearings, c.wind_dir),
                        bearings: state.bearings.clone(),
                    });
                    rows.push(Row::Big(
                        "Airing score".into(),
                        format!("{}", c.score),
                        score_color(c.score),
                    ));
                }
                None => rows.push(Row::KV("Waiting for forecast\u{2026}".into(), String::new(), None)),
            }

            rows.push(Row::Header("Good times to air out".into()));
            let now = now_key();
            let mut ranges: Vec<(String, String, u32)> = Vec::new();
            let mut i = 0;
            let hours = &state.hours;
            while i < hours.len() && ranges.len() < 3 {
                if hours[i].time.as_str() >= now.as_str() && hours[i].score >= state.threshold {
                    let start = i;
                    let mut peak = hours[i].score;
                    while i + 1 < hours.len()
                        && hours[i + 1].score >= state.threshold
                        && hours[i + 1].time.as_str() >= now.as_str()
                    {
                        i += 1;
                        peak = peak.max(hours[i].score);
                    }
                    let (day, h0) = day_hour(&hours[start].time);
                    let (_, h1) = day_hour(&hours[i].time);
                    ranges.push((
                        format!("{day} {h0:02}:00\u{2013}{:02}:00", (h1 + 1) % 24),
                        format!("score {peak}"),
                        peak,
                    ));
                }
                i += 1;
            }
            if ranges.is_empty() {
                rows.push(Row::KV("Nothing good in the next 7 days".into(), String::new(), None));
            }
            for (label, value, peak) in ranges {
                rows.push(Row::KV(label, value, Some(score_color(peak))));
            }
        }

        self.rows = rows;
    }

    fn row_height(r: &Row) -> f32 {
        match r {
            Row::Header(_) => HEADER_H,
            Row::KV(..) => ROW_H,
            Row::Bar { .. } => BAR_H,
            Row::Wind { .. } => WIND_H,
            Row::Big(..) => BIG_H,
        }
    }

    fn desired_height(&self) -> f32 {
        PAD * 2.0 + self.rows.iter().map(Self::row_height).sum::<f32>()
    }

    fn paint(&mut self, hwnd: HWND) {
        unsafe {
            if self.gfx.is_none() {
                self.gfx = self.build_gfx(hwnd).ok();
            }
            let Some(gfx) = &self.gfx else { return };
            let scale = self.scale;
            let s = |v: f32| v * scale;
            gfx.rt.BeginDraw();
            gfx.rt.Clear(Some(&col(self.style.bg.0, 1.0f32.min(self.style.bg.1 + 0.1))));

            let w = WIDTH * scale;

            // Pass 1: layout — (top, height) per row, in draw order.
            let mut tops = Vec::with_capacity(self.rows.len());
            let mut y = PAD * scale;
            for row in &self.rows {
                let h = Self::row_height(row) * scale;
                tops.push((y, h));
                y += h;
            }

            // Pass 2: cards behind each run of non-header rows.
            let surface = col(self.style.surface, 0.45);
            if let Ok(card_brush) = gfx.rt.CreateSolidColorBrush(&surface, None) {
                let mut i = 0;
                while i < self.rows.len() {
                    if matches!(self.rows[i], Row::Header(_)) {
                        i += 1;
                        continue;
                    }
                    let start = i;
                    while i < self.rows.len() && !matches!(self.rows[i], Row::Header(_)) {
                        i += 1;
                    }
                    let top = tops[start].0;
                    let bottom = tops[i - 1].0 + tops[i - 1].1;
                    gfx.rt.FillRoundedRectangle(
                        &windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: s(PAD * 0.5),
                                top: top - s(2.0),
                                right: w - s(PAD * 0.5),
                                bottom: bottom + s(2.0),
                            },
                            radiusX: s(8.0),
                            radiusY: s(8.0),
                        },
                        &card_brush,
                    );
                }
            }

            // Pass 3: rows.
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
            let tint = |c: u32, a: f32| gfx.rt.CreateSolidColorBrush(&col(c, a), None);

            for (row, &(top, h)) in self.rows.iter().zip(&tops) {
                let inner = D2D_RECT_F {
                    left: s(PAD),
                    top,
                    right: w - s(PAD),
                    bottom: top + h,
                };
                match row {
                    Row::Header(title) => {
                        let rect = D2D_RECT_F { top: top + s(8.0), ..inner };
                        text(title, &gfx.fmt_bold, &rect, &gfx.dim);
                    }
                    Row::KV(label, value, color) => {
                        text(label, &gfx.fmt, &inner, &gfx.fg);
                        if !value.is_empty() {
                            match color.and_then(|c| tint(c, 1.0).ok()) {
                                Some(b) => text(value, &gfx.fmt_right, &inner, &b),
                                None => text(value, &gfx.fmt_right, &inner, &gfx.fg),
                            }
                        }
                    }
                    Row::Bar { label, value, frac } => {
                        text(label, &gfx.fmt, &inner, &gfx.fg);
                        text(value, &gfx.fmt_right, &inner, &gfx.fg);
                        // Slim track between label and value columns.
                        let (bx0, bx1) = (inner.left + s(46.0), inner.right - s(112.0));
                        if bx1 > bx0 + s(20.0) {
                            let cy = top + h / 2.0;
                            let track = D2D_RECT_F {
                                left: bx0,
                                top: cy - s(2.5),
                                right: bx1,
                                bottom: cy + s(2.5),
                            };
                            let rr = |rect: D2D_RECT_F| {
                                windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT {
                                    rect,
                                    radiusX: s(2.5),
                                    radiusY: s(2.5),
                                }
                            };
                            if let Ok(b) = tint(self.style.dim, 0.30) {
                                gfx.rt.FillRoundedRectangle(&rr(track), &b);
                            }
                            let fill_w = (bx1 - bx0) * frac.clamp(0.0, 1.0);
                            if fill_w > s(2.0) {
                                gfx.rt.FillRoundedRectangle(
                                    &rr(D2D_RECT_F {
                                        right: bx0 + fill_w,
                                        ..track
                                    }),
                                    &gfx.accent,
                                );
                            }
                        }
                    }
                    Row::Wind {
                        kmh,
                        dir,
                        temp_c,
                        humidity,
                        feeds,
                        bearings,
                    } => {
                        // Compass rose, left.
                        let r = s(20.0);
                        let (cx, cy) = (inner.left + r + s(2.0), top + h / 2.0);
                        let pt = |ang_deg: f64, radius: f32| {
                            let a = ang_deg.to_radians();
                            windows_numerics::Vector2 { X: cx + radius * a.sin() as f32, Y: cy - radius * a.cos() as f32 }
                        };
                        let ellipse = windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                            point: windows_numerics::Vector2 { X: cx, Y: cy },
                            radiusX: r,
                            radiusY: r,
                        };
                        if let Ok(b) = tint(self.style.dim, 0.55) {
                            gfx.rt.DrawEllipse(&ellipse, &b, s(1.2), None);
                            // Cardinal ticks.
                            for c in [0.0, 90.0, 180.0, 270.0] {
                                gfx.rt.DrawLine(pt(c, r - s(3.5)), pt(c, r), &b, s(1.2), None);
                            }
                        }
                        // Window bearings, accented.
                        for bearing in bearings {
                            gfx.rt.DrawLine(
                                pt(*bearing, r - s(6.0)),
                                pt(*bearing, r + s(1.0)),
                                &gfx.accent,
                                s(2.2),
                                None,
                            );
                        }
                        // Wind ray: where the wind comes FROM.
                        let ray = if *feeds { &gfx.accent } else { &gfx.fg };
                        gfx.rt.DrawLine(
                            windows_numerics::Vector2 { X: cx, Y: cy },
                            pt(*dir, r - s(4.0)),
                            ray,
                            s(2.2),
                            None,
                        );
                        let dot = windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                            point: pt(*dir, r - s(4.0)),
                            radiusX: s(3.0),
                            radiusY: s(3.0),
                        };
                        gfx.rt.FillEllipse(&dot, ray);

                        // Text block to the right of the rose.
                        let tx = inner.left + r * 2.0 + s(14.0);
                        let mark = if *feeds { "  \u{2713} windows" } else { "" };
                        let line1 = D2D_RECT_F {
                            left: tx,
                            top: top + s(6.0),
                            right: inner.right,
                            bottom: top + h / 2.0,
                        };
                        let line2 = D2D_RECT_F {
                            top: top + h / 2.0,
                            bottom: top + h - s(6.0),
                            ..line1
                        };
                        let wind_brush = if *feeds { &gfx.accent } else { &gfx.fg };
                        text(
                            &format!("{kmh:.0} km/h from {}{mark}", compass(*dir)),
                            &gfx.fmt,
                            &line1,
                            wind_brush,
                        );
                        text(
                            &format!(
                                "{temp_c:.0}\u{b0}C / {:.0}\u{b0}F  \u{b7}  {humidity:.0}% RH",
                                temp_c * 9.0 / 5.0 + 32.0
                            ),
                            &gfx.fmt,
                            &line2,
                            &gfx.dim,
                        );
                    }
                    Row::Big(label, value, color) => {
                        // Tinted pill matching the score color.
                        if let Ok(b) = tint(*color, 0.16) {
                            gfx.rt.FillRoundedRectangle(
                                &windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT {
                                    rect: D2D_RECT_F {
                                        left: inner.left - s(4.0),
                                        top: top + s(3.0),
                                        right: inner.right + s(4.0),
                                        bottom: top + h - s(3.0),
                                    },
                                    radiusX: s(8.0),
                                    radiusY: s(8.0),
                                },
                                &b,
                            );
                        }
                        let pad_rect = D2D_RECT_F {
                            left: inner.left + s(6.0),
                            right: inner.right - s(6.0),
                            ..inner
                        };
                        text(label, &gfx.fmt, &pad_rect, &gfx.fg);
                        if let Ok(b) = tint(*color, 1.0) {
                            text(value, &gfx.fmt_big, &pad_rect, &b);
                        }
                    }
                }
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
            let size = self.style.font_size * self.scale;
            let mk = |sz: f32, weight, align_right: bool| -> windows::core::Result<IDWriteTextFormat> {
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
                if align_right {
                    f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
                }
                Ok(f)
            };
            Ok(Gfx {
                fg: rt.CreateSolidColorBrush(&col(self.style.fg, 1.0), None)?,
                dim: rt.CreateSolidColorBrush(&col(self.style.dim, 1.0), None)?,
                accent: rt.CreateSolidColorBrush(&col(self.style.accent, 1.0), None)?,
                fmt: mk(size, DWRITE_FONT_WEIGHT_NORMAL, false)?,
                fmt_bold: mk(size, DWRITE_FONT_WEIGHT_SEMI_BOLD, false)?,
                fmt_right: mk(size, DWRITE_FONT_WEIGHT_NORMAL, true)?,
                fmt_big: mk(size * 1.5, DWRITE_FONT_WEIGHT_SEMI_BOLD, true)?,
                rt,
            })
        }
    }
}

/// Toggle the popup near the cursor (i.e. under the clicked widget).
pub fn toggle(style: Style) {
    if is_open() {
        close();
        return;
    }
    // The click that opens us may have just closed us via WM_KILLFOCUS
    // (focus loss fires before the bar's click handler). Treat a click
    // arriving within 400 ms of that close as "toggle off", not "reopen".
    if now_ticks() - CLOSED_AT.load(Ordering::Relaxed) < 400 {
        return;
    }
    unsafe {
        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(_) => return,
        };
        let wc = WNDCLASSW {
            style: Default::default(),
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc); // idempotent; fails harmlessly when re-registered

        // LHM reader thread (stops when the popup closes).
        let lhm_temp: Arc<Mutex<Option<f64>>> = Arc::new(Mutex::new(None));
        let lhm_alive = Arc::new(AtomicBool::new(true));
        if !style.lhm_sensor.is_empty() {
            let (temp, alive, sensor) =
                (lhm_temp.clone(), lhm_alive.clone(), style.lhm_sensor.clone());
            std::thread::spawn(move || {
                while alive.load(Ordering::Relaxed) {
                    let v = crate::widgets::lhm::read_sensor("127.0.0.1", 8085, &sensor);
                    if let Ok(mut t) = temp.lock() {
                        *t = v;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2000));
                }
            });
        }

        let mut pop = Box::new(Pop {
            style,
            rows: Vec::new(),
            scale: 1.0,
            gfx: None,
            nvml: nvml_load(),
            cpu_last: (0, 0),
            cpu_pct: None,
            fast_tick: true,
            hint_row: None,
            lhm_temp,
            lhm_alive,
        });
        pop.build_rows();

        // Position: below the cursor, clamped to the work area.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        // DPI of the popup window isn't known until it exists; approximate
        // with the monitor's via a probe window position first — the bar is
        // per-monitor DPI aware, so use the cursor monitor's scale by
        // creating at nominal size and fixing up after GetDpiForWindow.
        let w_px = WIDTH as i32;
        let h_px = pop.desired_height() as i32;
        let x = (pt.x - w_px / 2)
            .max(mi.rcWork.left + 4)
            .min(mi.rcWork.right - w_px - 4);
        let y = (mi.rcWork.top + 4).max(pt.y + 8).min(mi.rcWork.bottom - h_px - 4);

        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("stats"),
            WS_POPUP,
            x,
            y,
            w_px,
            h_px,
            None,
            None,
            Some(hinstance.into()),
            Some(&*pop as *const Pop as *const c_void),
        ) {
            Ok(h) => h,
            Err(_) => return,
        };
        // Rounded corners, matching the Win11 flyout look.
        let corner = windows::Win32::Graphics::Dwm::DWMWCP_ROUND;
        let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            std::mem::size_of_val(&corner) as u32,
        );

        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        if (scale - 1.0).abs() > 0.01 {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Pop;
            if !p.is_null() {
                (*p).scale = scale;
                let h_px = ((*p).desired_height() * scale) as i32;
                let w_px = (WIDTH * scale) as i32;
                let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                    hwnd,
                    None,
                    x,
                    y,
                    w_px,
                    h_px,
                    windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                        | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                );
            }
        }
        std::mem::forget(pop); // owned by the window; freed in WM_NCDESTROY

        OPEN.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        SetTimer(Some(hwnd), 1, 150, None);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = lparam.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let pop = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Pop;
        if pop.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                (*pop).paint(hwnd);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_TIMER => {
                if (*pop).fast_tick {
                    (*pop).fast_tick = false;
                    SetTimer(Some(hwnd), 1, 1000, None); // replaces the fast timer
                }
                (*pop).build_rows();
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONUP => {
                // Only the "set location" hint row is clickable.
                if let Some(hint) = (*pop).hint_row {
                    let y = ((lparam.0 as u32 >> 16) & 0xFFFF) as i16 as f32;
                    let scale = (*pop).scale;
                    let mut top = PAD * scale;
                    for (i, row) in (*pop).rows.iter().enumerate() {
                        let h = Pop::row_height(row) * scale;
                        if i == hint {
                            if y >= top && y <= top + h {
                                let path16: Vec<u16> = crate::config::path()
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
                                close();
                            }
                            break;
                        }
                        top += h;
                    }
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
                let _ = KillTimer(Some(hwnd), 1);
                (*pop).lhm_alive.store(false, Ordering::Relaxed);
                drop(Box::from_raw(pop));
                OPEN.store(0, Ordering::Relaxed);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
