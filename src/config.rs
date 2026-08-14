use std::collections::HashMap;
use std::path::PathBuf;

/// "ctrl+shift+m" / "f13" -> (MOD_* bits, VK code). Modifiers optional only
/// for F13-F24, which can't hijack normal typing; everything else needs at
/// least one of ctrl/alt/shift/win.
pub fn parse_hotkey(v: &str) -> Option<(u32, u32)> {
    let v = v.to_lowercase();
    let parts: Vec<&str> = v.split('+').map(str::trim).collect();
    let (key, mods_parts) = parts.split_last()?;
    let mut mods = 0u32;
    for m in mods_parts {
        mods |= match *m {
            "alt" => 0x0001,
            "ctrl" | "control" => 0x0002,
            "shift" => 0x0004,
            "win" => 0x0008,
            _ => return None,
        };
    }
    let vk = match *key {
        "space" => 0x20,
        k if k.len() == 1 && k.chars().next().unwrap().is_ascii_alphanumeric() => {
            k.to_uppercase().bytes().next().unwrap() as u32
        }
        k if k.starts_with('f') => {
            let n: u32 = k[1..].parse().ok()?;
            if (1..=24).contains(&n) {
                0x70 + n - 1
            } else {
                return None;
            }
        }
        _ => return None,
    };
    if mods == 0 && !(0x7C..=0x87).contains(&vk) {
        return None; // unmodified non-F13..F24 keys would hijack typing
    }
    Some((mods, vk))
}

/// Sectioned INI: `[section]` headers, `key = value` lines, `#` comments.
/// Section "widget.foo" holds per-widget options; bare keys before any
/// section header land in the implicit "bar" section.
pub struct Ini {
    /// section -> key -> value (all lowercased keys; values verbatim, trimmed)
    sections: HashMap<String, HashMap<String, String>>,
}

impl Ini {
    pub fn parse(text: &str) -> Ini {
        let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut current = "bar".to_string();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                current = name.trim().to_lowercase();
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                sections
                    .entry(current.clone())
                    .or_default()
                    .insert(k.trim().to_lowercase(), v.trim().to_string());
            }
        }
        Ini { sections }
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(section)?.get(key).map(|s| s.as_str())
    }

    pub fn get_or(&self, section: &str, key: &str, default: &str) -> String {
        self.get(section, key).unwrap_or(default).to_string()
    }

    pub fn get_f32(&self, section: &str, key: &str, default: f32) -> f32 {
        self.get(section, key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    pub fn get_u64(&self, section: &str, key: &str, default: u64) -> u64 {
        self.get(section, key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    /// 6-digit RGB or 8-digit RGBA hex -> (0xRRGGBB, alpha 0..1)
    pub fn get_color(&self, section: &str, key: &str, default: (u32, f32)) -> (u32, f32) {
        let Some(v) = self.get(section, key) else {
            return default;
        };
        match v.len() {
            6 => u32::from_str_radix(v, 16).map(|c| (c, 1.0)).unwrap_or(default),
            8 => u32::from_str_radix(v, 16)
                .map(|c| (c >> 8, (c & 0xFF) as f32 / 255.0))
                .unwrap_or(default),
            _ => default,
        }
    }

    /// Comma-separated list value.
    pub fn get_list(&self, section: &str, key: &str) -> Vec<String> {
        self.get(section, key)
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub struct BarConfig {
    pub height: f32,
    pub position_top: bool,
    pub reserve: bool,
    /// "all" (default) or "primary".
    pub all_monitors: bool,
    /// Device-id substrings of monitors that never get a bar.
    pub exclude: Vec<String>,
    pub font: String,
    pub font_size: f32,
    pub bg: (u32, f32),
    pub fg: u32,
    pub dim: u32,
    pub accent: u32,   // lavender — active pills, highlights
    pub surface: u32,  // hover/pill backgrounds
    pub pad: f32,
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
    pub ini: Ini,
}

impl BarConfig {
    /// Widget list for a side on a given bar index. `[left.1]` overrides
    /// `[left]` for the second monitor; unspecified monitors inherit.
    pub fn side_widgets(&self, side: &str, bar_index: usize) -> Vec<String> {
        if bar_index > 0 {
            let per = self.ini.get_list(&format!("{side}.{bar_index}"), "widgets");
            if self
                .ini
                .get(&format!("{side}.{bar_index}"), "widgets")
                .is_some()
            {
                return per;
            }
        }
        match side {
            "left" => self.left.clone(),
            "center" => self.center.clone(),
            _ => self.right.clone(),
        }
    }
}

/// Catppuccin Mocha defaults, mirroring Brian's styles.css.
const DEFAULT_FILE: &str = "\
# optim-bar configuration
# edit, save — the bar watches this file and reloads itself

[bar]
height = 36
position = top
reserve = true           # register as AppBar: maximized windows stop at the bar
monitors = all           # 'all' or 'primary'
# exclude =              # device-id substrings that never get a bar, e.g. SAM78B7
# per-monitor widget overrides: add [left.1] / [center.1] / [right.1]
# sections with their own `widgets =` line; monitors without one inherit
font = JetBrainsMono NFP
font_size = 13
bg = 181825D9            # RGBA — mantle, ~85% opacity
fg = CDD6F4              # text
dim = 7F849C             # overlay1
accent = B4BEFE          # lavender
surface = 313244         # surface0

[left]
widgets = workspaces, tasks

[widget.workspaces]
hide_empty = true
min_width = 16           # clickable cell per number, logical px; font unchanged
# active_fill = accent   # pill behind the focused workspace: accent | surface | none | RRGGBB
# active_fg =            # glyph on that pill; defaults to the bar bg (dark on light)

[widget.tasks]
type = tasks

[center]
widgets = clock

# NOTE: right-side widgets render first-listed NEAREST THE RIGHT EDGE.
# The line below reads visually left-to-right as: systray, mic, volume,
# cpu_temp, gpu_temp.
[right]
widgets = gpu_temp, cpu_temp, volume, mic, systray

[widget.systray]
type = systray

[widget.volume]
type = volume
scroll_step = 2

# Mic mute indicator/toggle (Windows endpoint level; red = muted, click toggles)
[widget.mic]
type = mic
hotkey = alt+shift+m

# cpu / mem have no bar slot by default — click any temp widget for the
# stats dropdown, which shows both.
[widget.cpu]
type = cpu

[widget.mem]
type = mem

[widget.gpu_temp]
type = gpu_temp
icon = 󰔃

# CPU die temp via LibreHardwareMonitor's web server — optional; the widget
# hides itself whenever LHM isn't running. sensor_id is hardware-specific;
# the one below is an AMD Ryzen Tctl path. Browse http://localhost:8085 to
# find yours.
[widget.cpu_temp]
type = lhm
sensor_id = /amdcpu/0/temperature/2
icon =

[widget.clock]
format = %a, %d %b %I:%M %p
format_alt = %A, %d %B %Y

# Generic exec widget: run a command on a timer, click to run others.
# Add its name to a widgets = line above to show it.
# [widget.eq]
# type = exec
# run = pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\\path\\to\\script.ps1 status
# interval = 10000
# on_left = pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\\path\\to\\script.ps1 menu
# on_middle = ...
# on_right = ...

# Alt+Tab replacement: a vertical list of window titles instead of thumbnails.
# Alt+Tab steps down, Alt+Shift+Tab steps up, Esc cancels, release Alt to go.
# Colors come from [bar]. Set enabled = false to hand Alt+Tab back to Windows.
[switcher]
enabled = true
width = 620            # logical px
max_rows = 14          # taller lists scroll to keep the selection visible
# font =               # defaults to the bar font
# font_size =          # defaults to bar font_size + 1
# bypass =             # exe names that keep the stock Alt+Tab, e.g. acs.exe

# Weather + airing advisor (Open-Meteo, no key). Click any stat widget
# (cpu / mem / gpu_temp / cpu_temp) for the system+weather dropdown.
# Weather stays OFF until you set your coordinates here — they are personal
# data, live only in this local file, and are sent only to open-meteo.com.
# window_bearings: compass directions your windows face; wind arriving
# from within ~60 deg of a bearing counts as feeding the window. Optional;
# without it wind direction is ignored.
# [weather]
# lat = 40.7128        # example: New York City
# lon = -74.0060
# window_bearings =
# toast = on
# toast_threshold = 65
# toast_hours_from = 8
# toast_hours_to = 22
";

pub fn path() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
        .join("optim-bar")
        .join("config.ini")
}

pub fn load() -> BarConfig {
    let p = path();
    if !p.exists() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, DEFAULT_FILE);
    }
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    let ini = Ini::parse(&text);
    BarConfig {
        height: ini.get_f32("bar", "height", 36.0).clamp(20.0, 80.0),
        position_top: ini.get_or("bar", "position", "bottom") == "top",
        reserve: ini.get_or("bar", "reserve", "true") != "false",
        all_monitors: ini.get_or("bar", "monitors", "all") != "primary",
        exclude: ini.get_list("bar", "exclude"),
        font: ini.get_or("bar", "font", "JetBrainsMono NFP"),
        font_size: ini.get_f32("bar", "font_size", 13.0).clamp(8.0, 24.0),
        bg: ini.get_color("bar", "bg", (0x181825, 0.85)),
        fg: ini.get_color("bar", "fg", (0xCDD6F4, 1.0)).0,
        dim: ini.get_color("bar", "dim", (0x7F849C, 1.0)).0,
        accent: ini.get_color("bar", "accent", (0xB4BEFE, 1.0)).0,
        surface: ini.get_color("bar", "surface", (0x313244, 1.0)).0,
        pad: ini.get_f32("bar", "pad", 6.0),
        left: ini.get_list("left", "widgets"),
        center: ini.get_list("center", "widgets"),
        right: ini.get_list("right", "widgets"),
        ini,
    }
}

#[cfg(test)]
mod tests {
    use super::Ini;

    #[test]
    fn sections_and_colors() {
        let ini = Ini::parse("[bar]\nbg = 181825D9\n[widget.clock]\nformat = %H:%M\n");
        assert_eq!(ini.get("widget.clock", "format"), Some("%H:%M"));
        let (rgb, a) = ini.get_color("bar", "bg", (0, 1.0));
        assert_eq!(rgb, 0x181825);
        assert!((a - 0.851).abs() < 0.01);
    }

    #[test]
    fn lists() {
        let ini = Ini::parse("[right]\nwidgets = systray, volume , clock\n");
        assert_eq!(ini.get_list("right", "widgets"), ["systray", "volume", "clock"]);
    }
}
