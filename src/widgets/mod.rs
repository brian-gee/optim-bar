pub mod clock;

use crate::config::BarConfig;

/// One segment on the bar. v1 widgets are text: the bar owns layout and
/// rendering; a widget just produces styled text and reacts to clicks/ticks.
pub trait Widget {
    /// Master tick (~250 ms). Return true to request a repaint.
    fn tick(&mut self) -> bool;

    /// Current display text (may contain Nerd Font glyphs).
    fn text(&self) -> &str;

    /// Color role for the text: None = default fg.
    fn color(&self) -> Option<u32> {
        None
    }

    /// Mouse click: button 0 = left, 1 = middle, 2 = right.
    fn on_click(&mut self, _button: u8) {}
}

/// Instantiate a widget by config name (the strings in [left]/[center]/[right]).
pub fn build(name: &str, cfg: &BarConfig) -> Option<Box<dyn Widget>> {
    let section = format!("widget.{name}");
    match cfg.ini.get_or(&section, "type", name).as_str() {
        "clock" => Some(Box::new(clock::Clock::new(cfg, &section))),
        _ => None,
    }
}
