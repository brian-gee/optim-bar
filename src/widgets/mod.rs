pub mod clock;
pub mod exec;

use std::sync::Arc;

use crate::config::BarConfig;

/// Color role resolved against the bar palette at draw time.
#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    Fg,
    Dim,
    Accent,
    Custom(u32),
}

/// One clickable piece of a widget: optional 32x32 BGRA icon + text.
/// Icons carry a stable cache key so the bar can keep D2D bitmaps around.
pub struct Segment {
    pub text: String,
    pub role: Role,
    pub icon: Option<(u64, Arc<Vec<u8>>)>,
}

impl Segment {
    pub fn text(t: impl Into<String>, role: Role) -> Segment {
        Segment {
            text: t.into(),
            role,
            icon: None,
        }
    }
}

pub trait Widget {
    /// Master tick (~250 ms). Return true to request a repaint.
    fn tick(&mut self) -> bool;

    /// Current segments, left to right. An empty vec hides the widget.
    fn segments(&self) -> Vec<Segment>;

    /// Click on segment `seg`: button 0 = left, 1 = middle, 2 = right.
    fn on_click(&mut self, _seg: usize, _button: u8) {}

    /// Wheel over the widget; delta in WHEEL_DELTA units (+up / -down).
    fn on_wheel(&mut self, _delta: i32) {}
}

/// Instantiate a widget by config name (the strings in [left]/[center]/[right]).
/// `type` in the widget's section overrides the name for lookup, so several
/// widgets can share an implementation (eq + voicemod are both `exec`).
pub fn build(name: &str, cfg: &BarConfig) -> Option<Box<dyn Widget>> {
    let section = format!("widget.{name}");
    match cfg.ini.get_or(&section, "type", name).as_str() {
        "clock" => Some(Box::new(clock::Clock::new(cfg, &section))),
        "exec" => Some(Box::new(exec::Exec::new(cfg, &section))),
        _ => None,
    }
}
