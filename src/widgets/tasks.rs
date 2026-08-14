//! Window-list taskbar: icons for this monitor's open windows, click to
//! focus/minimize. Polls at 1 s; no hooks, no per-frame cost.

use std::collections::HashMap;
use std::sync::Arc;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, MonitorFromWindow,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, EnumWindows, GetClassLongPtrW, GetForegroundWindow, GetWindow,
    GetWindowLongPtrW, GetWindowTextLengthW, IsIconic, IsWindowVisible, SendMessageTimeoutW,
    SetForegroundWindow, ShowWindow, DI_NORMAL, GCLP_HICON, GWL_EXSTYLE, GW_OWNER, HICON,
    SMTO_ABORTIFHUNG, SW_MINIMIZE, SW_RESTORE, WM_GETICON, WS_EX_TOOLWINDOW,
};

use crate::bar::ICON_SRC;
use crate::config::BarConfig;
use crate::widgets::{Segment, Widget};

struct EnumCtx {
    /// None = every monitor (the switcher wants the whole desktop).
    monitor: Option<isize>,
    out: Vec<isize>,
}

/// EnumWindows sink: collects candidate top-level windows.
unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumCtx);
    if !IsWindowVisible(hwnd).as_bool() {
        return true.into();
    }
    if GetWindowTextLengthW(hwnd) == 0 {
        return true.into();
    }
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return true.into();
    }
    if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
        return true.into();
    }
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut _ as _,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return true.into();
    }
    // Only windows on this bar's monitor.
    if let Some(mon) = ctx.monitor {
        if MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST).0 as isize != mon {
            return true.into();
        }
    }
    ctx.out.push(hwnd.0 as isize);
    true.into()
}

/// Candidate top-level windows in Z-order (topmost first). `monitor` limits
/// the result to one monitor handle; None returns the whole desktop.
pub fn enumerate(monitor: Option<isize>) -> Vec<isize> {
    let mut ctx = EnumCtx {
        monitor,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.out
}

/// Renders an HICON into 32x32 premultiplied BGRA.
pub fn icon_pixels(hicon: HICON) -> Option<Vec<u8>> {
    unsafe {
        let screen = GetDC(None);
        let hdc = CreateCompatibleDC(Some(screen));
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ICON_SRC as i32,
                biHeight: -(ICON_SRC as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let Ok(hbm) = CreateDIBSection(Some(hdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc);
            ReleaseDC(None, screen);
            return None;
        };
        let old = SelectObject(hdc, HGDIOBJ(hbm.0));
        std::ptr::write_bytes(bits as *mut u8, 0, ICON_SRC * ICON_SRC * 4);
        let ok = DrawIconEx(
            hdc,
            0,
            0,
            hicon,
            ICON_SRC as i32,
            ICON_SRC as i32,
            0,
            None,
            DI_NORMAL,
        )
        .is_ok();
        let mut out = None;
        if ok {
            let mut px =
                std::slice::from_raw_parts(bits as *const u8, ICON_SRC * ICON_SRC * 4).to_vec();
            if px.chunks_exact(4).all(|p| p[3] == 0) {
                for p in px.chunks_exact_mut(4) {
                    p[3] = 255;
                }
            } else {
                // D2D wants premultiplied alpha.
                for p in px.chunks_exact_mut(4) {
                    let a = p[3] as u32;
                    p[0] = (p[0] as u32 * a / 255) as u8;
                    p[1] = (p[1] as u32 * a / 255) as u8;
                    p[2] = (p[2] as u32 * a / 255) as u8;
                }
            }
            out = Some(px);
        }
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        let _ = DeleteDC(hdc);
        ReleaseDC(None, screen);
        out
    }
}

pub fn window_icon(hwnd: HWND) -> Option<Vec<u8>> {
    unsafe {
        let mut result = 0usize;
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            WPARAM(2), // ICON_SMALL2
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            80,
            Some(&mut result),
        );
        let mut hicon = HICON(result as *mut _);
        if hicon.is_invalid() {
            hicon = HICON(GetClassLongPtrW(hwnd, GCLP_HICON) as *mut _);
        }
        if hicon.is_invalid() {
            return None;
        }
        icon_pixels(hicon)
    }
}

pub struct Tasks {
    monitor: Option<isize>,
    windows: Vec<isize>,
    icons: HashMap<isize, Option<Arc<Vec<u8>>>>,
    ticks: u32,
}

impl Tasks {
    pub fn new(_cfg: &BarConfig, _section: &str, monitor: isize) -> Tasks {
        Tasks {
            monitor: Some(monitor),
            windows: Vec::new(),
            icons: HashMap::new(),
            ticks: 0,
        }
    }
}

impl Widget for Tasks {
    fn tick(&mut self) -> bool {
        self.ticks += 1;
        // Enumerate every 250 ms tick — enumeration is cheap; it's icon
        // extraction that costs, and that's cached below. This is what makes
        // desktop switches feel instant.
        let fresh = enumerate(self.monitor);
        // The icon cache survives workspace switches: a window that cloaks
        // and comes back keeps its pixels, no re-fetch. Prune only entries
        // whose window is actually gone (every ~30 s), so the map stays
        // bounded without evicting live apps.
        if self.ticks % 120 == 0 {
            self.icons.retain(|&k, _| unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(k as *mut _)))
                    .as_bool()
            });
        }
        if fresh == self.windows {
            return false;
        }
        for &hwnd in &fresh {
            self.icons
                .entry(hwnd)
                .or_insert_with(|| window_icon(HWND(hwnd as *mut _)).map(Arc::new));
        }
        self.windows = fresh;
        true
    }

    fn segments(&self) -> Vec<Segment> {
        self.windows
            .iter()
            .map(|&hwnd| {
                let icon = self
                    .icons
                    .get(&hwnd)
                    .and_then(|i| i.clone())
                    .map(|px| (hwnd as u64, px));
                match icon {
                    Some(ic) => Segment {
                        text: String::new(),
                        role: crate::widgets::Role::Fg,
                        icon: Some(ic),
                        fill: None,
                    },
                    // No icon extractable: two-char stand-in keeps it clickable.
                    None => Segment::text("\u{eb7f}", crate::widgets::Role::Dim),
                }
            })
            .collect()
    }

    fn on_click(&mut self, seg: usize, button: u8) {
        if button != 0 {
            return;
        }
        let Some(&hwnd_val) = self.windows.get(seg) else { return };
        let hwnd = HWND(hwnd_val as *mut _);
        unsafe {
            if GetForegroundWindow() == hwnd {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            } else {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }
}
