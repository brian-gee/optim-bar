//! Bar widget rendering the icons collected by the tray host (tray.rs).
//! Default: a chevron that opens the icons in a flyout popup (flyout.rs);
//! `collapsed = false` renders them inline in the bar instead.

use std::sync::Arc;

use crate::config::BarConfig;
use crate::flyout;
use crate::tray::{self, TrayIcon, TrayState};
use crate::widgets::{Role, Segment, Widget};

pub struct Systray {
    state: TrayState,
    shown: Vec<TrayIcon>,
    collapsible: bool,
    /// Flyout-open state at last tick, to repaint the chevron on dismiss.
    was_open: bool,
    bg: (u32, f32),
    dim: u32,
    surface: u32,
}

impl Systray {
    pub fn new(cfg: &BarConfig, section: &str) -> Systray {
        tray::ensure_host();
        let collapsible = cfg.ini.get_or(section, "collapsed", "true") != "false";
        Systray {
            state: tray::state(),
            shown: Vec::new(),
            collapsible,
            was_open: false,
            bg: cfg.bg,
            dim: cfg.dim,
            surface: cfg.surface,
        }
    }

    fn visible(&self) -> Vec<TrayIcon> {
        self.shown.iter().filter(|i| !i.hidden).cloned().collect()
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
        let mut dirty = false;
        let fresh = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        if !same(&fresh, &self.shown) {
            self.shown = fresh;
            dirty = true;
        }
        let open = flyout::is_open();
        if open != self.was_open {
            self.was_open = open;
            dirty = true;
        }
        dirty
    }

    fn segments(&self) -> Vec<Segment> {
        if self.collapsible {
            // nf-fa chevrons: down = "click to drop the flyout", up = open
            let glyph = if flyout::is_open() { "\u{f077}" } else { "\u{f078}" };
            return vec![Segment::text(glyph, Role::Dim)];
        }
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
                        fill: None,
                    },
                    None => Segment::text("\u{f111}", Role::Dim),
                }
            })
            .collect()
    }

    fn on_click(&mut self, seg: usize, button: u8) {
        if self.collapsible {
            if seg == 0 && button == 0 {
                flyout::toggle(self.visible(), self.bg, self.dim, self.surface);
            }
            return;
        }
        let visible: Vec<&TrayIcon> = self.shown.iter().filter(|i| !i.hidden).collect();
        let Some(icon) = visible.get(seg) else { return };
        tray::send_button(icon, button);
    }
}
