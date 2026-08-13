use std::collections::HashMap;
use std::path::PathBuf;

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
widgets =

[center]
widgets = clock

[right]
widgets =

[widget.clock]
format = %a, %d %b %I:%M %p
format_alt = %A, %d %B %Y
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
