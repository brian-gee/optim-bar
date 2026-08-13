use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::BarConfig;
use crate::widgets::{Role, Segment, Widget};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Splits a command line into (program, rest) honoring a quoted program path.
fn split_program(cmdline: &str) -> (String, String) {
    let s = cmdline.trim();
    if let Some(rest) = s.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (rest[..end].to_string(), rest[end + 1..].trim_start().to_string());
        }
    }
    match s.split_once(' ') {
        Some((p, r)) => (p.to_string(), r.to_string()),
        None => (s.to_string(), String::new()),
    }
}

/// Fire-and-forget a command line with no console window.
pub fn spawn_hidden(cmdline: &str) {
    if cmdline.is_empty() {
        return;
    }
    let (prog, rest) = split_program(cmdline);
    let mut cmd = Command::new(prog);
    if !rest.is_empty() {
        cmd.raw_arg(rest);
    }
    let _ = cmd
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Runs a command line hidden, returns trimmed stdout.
fn run_capture(cmdline: &str) -> String {
    let (prog, rest) = split_program(cmdline);
    let mut cmd = Command::new(prog);
    if !rest.is_empty() {
        cmd.raw_arg(rest);
    }
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// YASB custom widgets emit HTML like `<span style="color:#94e2d5;">txt</span>`.
/// Extract the first span color and strip all tags so those scripts render
/// correctly here without modification.
fn parse_spans(raw: &str) -> (String, Option<u32>) {
    let mut color = None;
    if let Some(i) = raw.find("color:#") {
        let hex: String = raw[i + 7..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hex.len() == 6 {
            color = u32::from_str_radix(&hex, 16).ok();
        }
    }
    let mut text = String::new();
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string();
    (text, color)
}

/// Generic script widget: `run` on an interval shows stdout; clicks exec
/// command lines. Brian's YASB custom widgets (EQ switcher, Voicemod mute)
/// map onto this directly.
pub struct Exec {
    state: Arc<Mutex<String>>,
    alive: Arc<AtomicBool>,
    shown: String,
    shown_color: Option<u32>,
    on_left: String,
    on_middle: String,
    on_right: String,
    color: Role,
}

impl Exec {
    pub fn new(cfg: &BarConfig, section: &str) -> Exec {
        let run = cfg.ini.get_or(section, "run", "");
        let interval = cfg.ini.get_u64(section, "interval", 5000).max(250);
        let color = cfg
            .ini
            .get(section, "color")
            .and_then(|v| u32::from_str_radix(v, 16).ok())
            .map(Role::Custom)
            .unwrap_or(Role::Fg);

        let state = Arc::new(Mutex::new(String::new()));
        let alive = Arc::new(AtomicBool::new(true));
        if !run.is_empty() {
            let state2 = state.clone();
            let alive2 = alive.clone();
            std::thread::spawn(move || {
                while alive2.load(Ordering::Relaxed) {
                    let out = run_capture(&run);
                    if let Ok(mut s) = state2.lock() {
                        *s = out;
                    }
                    std::thread::sleep(Duration::from_millis(interval));
                }
            });
        }

        Exec {
            state,
            alive,
            shown: String::new(),
            shown_color: None,
            on_left: cfg.ini.get_or(section, "on_left", ""),
            on_middle: cfg.ini.get_or(section, "on_middle", ""),
            on_right: cfg.ini.get_or(section, "on_right", ""),
            color,
        }
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

impl Widget for Exec {
    fn tick(&mut self) -> bool {
        let raw = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        let (text, span_color) = parse_spans(&raw);
        if text != self.shown || span_color != self.shown_color {
            self.shown = text;
            self.shown_color = span_color;
            true
        } else {
            false
        }
    }

    fn segments(&self) -> Vec<Segment> {
        if self.shown.is_empty() {
            return Vec::new(); // nothing to show -> widget hides
        }
        let role = match self.shown_color {
            Some(c) => Role::Custom(c),
            None => self.color,
        };
        vec![Segment::text(&self.shown, role)]
    }

    fn on_click(&mut self, _seg: usize, button: u8) {
        let cmd = match button {
            0 => &self.on_left,
            1 => &self.on_middle,
            _ => &self.on_right,
        };
        spawn_hidden(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_spans, split_program};

    #[test]
    fn span_parsing() {
        let (text, color) =
            parse_spans("<span style=\"color:#94e2d5;\">\u{f025} HD620S fun</span>");
        assert_eq!(text, "\u{f025} HD620S fun");
        assert_eq!(color, Some(0x94E2D5));
        assert_eq!(parse_spans("plain"), ("plain".into(), None));
    }

    #[test]
    fn program_splitting() {
        assert_eq!(
            split_program("pwsh -NoProfile -File a.ps1 status"),
            ("pwsh".into(), "-NoProfile -File a.ps1 status".into())
        );
        assert_eq!(
            split_program("\"C:\\Program Files\\nodejs\\node.exe\" script.js"),
            ("C:\\Program Files\\nodejs\\node.exe".into(), "script.js".into())
        );
        assert_eq!(split_program("cmd"), ("cmd".into(), String::new()));
    }
}
