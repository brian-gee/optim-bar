//! Bar widget rendering the icons collected by the tray host (tray.rs).

use std::sync::Arc;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, SetForegroundWindow, WM_CONTEXTMENU, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

use crate::config::BarConfig;
use crate::tray::{self, TrayIcon, TrayState};
use crate::widgets::{Role, Segment, Widget};

pub struct Systray {
    state: TrayState,
    shown: Vec<TrayIcon>,
}

impl Systray {
    pub fn new(_cfg: &BarConfig, _section: &str) -> Systray {
        tray::ensure_host();
        Systray {
            state: tray::state(),
            shown: Vec::new(),
        }
    }
}

fn same(a: &[TrayIcon], b: &[TrayIcon]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.owner == y.owner
                && x.uid == y.uid
                && x.hidden == y.hidden
                && match (&x.pixels, &y.pixels) {
                    (Some(p), Some(q)) => Arc::ptr_eq(p, q),
                    (None, None) => true,
                    _ => false,
                }
        })
}

impl Widget for Systray {
    fn tick(&mut self) -> bool {
        let fresh = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        if !same(&fresh, &self.shown) {
            self.shown = fresh;
            true
        } else {
            false
        }
    }

    fn segments(&self) -> Vec<Segment> {
        self.shown
            .iter()
            .filter(|i| !i.hidden)
            .map(|i| {
                let key = ((i.owner as u64) << 32) | i.uid as u64;
                match &i.pixels {
                    Some(px) => Segment {
                        text: String::new(),
                        role: Role::Fg,
                        icon: Some((key, px.clone())),
                    },
                    None => Segment::text("\u{f111}", Role::Dim),
                }
            })
            .collect()
    }

    fn on_click(&mut self, seg: usize, button: u8) {
        let visible: Vec<&TrayIcon> = self.shown.iter().filter(|i| !i.hidden).collect();
        let Some(icon) = visible.get(seg) else { return };
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            // Menus opened by the app need foreground rights to dismiss right.
            let _ = SetForegroundWindow(windows::Win32::Foundation::HWND(
                icon.owner as usize as *mut _,
            ));
        }
        let cursor = (pt.x, pt.y);
        match button {
            0 => {
                tray::forward_click(icon, WM_LBUTTONDOWN, cursor);
                tray::forward_click(icon, WM_LBUTTONUP, cursor);
            }
            2 => {
                tray::forward_click(icon, WM_RBUTTONDOWN, cursor);
                tray::forward_click(icon, WM_RBUTTONUP, cursor);
                if icon.version >= 4 {
                    tray::forward_click(icon, WM_CONTEXTMENU, cursor);
                }
            }
            _ => {
                tray::forward_click(icon, WM_MBUTTONUP, cursor);
            }
        }
    }
}
