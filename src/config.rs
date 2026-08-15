use std::path::PathBuf;

use toml::Value;

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

/// Any TOML scalar rendered as text.
///
/// Call sites compare against string literals (`position == "top"`), so an
/// unquoted `height = 36` or `reserve = true` has to read back the same way
/// a quoted one would.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Integer(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Parsed config. Sections are dotted paths matching the `[table.header]`
/// they came from — `"widget.systray"`, `"left.1"` — so call sites read
/// exactly as they did under the old INI parser.
pub struct Cfg {
    root: toml::Table,
}

impl Cfg {
    pub fn parse(text: &str) -> Cfg {
        match text.parse::<toml::Table>() {
            Ok(root) => Cfg { root },
            Err(e) => {
                // We're a GUI-subsystem process, so stderr goes nowhere most
                // of the time. A broken file would otherwise show up as a bar
                // with no widgets and no hint why, so say it out loud and
                // fall back to the stock layout rather than to nothing.
                crate::toast::show("optim-bar: config error", &e.to_string());
                Cfg {
                    root: DEFAULT_FILE.parse().unwrap_or_default(),
                }
            }
        }
    }

    /// Walks a dotted section path, then the key inside it.
    fn value(&self, section: &str, key: &str) -> Option<&Value> {
        let mut parts = section.split('.');
        let mut cur = self.root.get(parts.next()?)?;
        for part in parts {
            cur = cur.as_table()?.get(part)?;
        }
        cur.as_table()?.get(key)
    }

    /// Whether a key exists at all, whatever its type. `get` can't answer
    /// this: it returns None for arrays, which is exactly what per-monitor
    /// `widgets =` overrides are.
    pub fn has(&self, section: &str, key: &str) -> bool {
        self.value(section, key).is_some()
    }

    pub fn get(&self, section: &str, key: &str) -> Option<String> {
        scalar_text(self.value(section, key)?)
    }

    pub fn get_or(&self, section: &str, key: &str, default: &str) -> String {
        self.get(section, key)
            .unwrap_or_else(|| default.to_string())
    }

    pub fn get_f32(&self, section: &str, key: &str, default: f32) -> f32 {
        match self.value(section, key) {
            None => default,
            Some(Value::Float(f)) => *f as f32,
            Some(Value::Integer(i)) => *i as f32,
            Some(Value::String(s)) => s.parse().unwrap_or(default),
            Some(_) => default,
        }
    }

    pub fn get_u64(&self, section: &str, key: &str, default: u64) -> u64 {
        match self.value(section, key) {
            None => default,
            Some(Value::Integer(i)) => (*i).try_into().unwrap_or(default),
            Some(Value::Float(f)) => *f as u64,
            Some(Value::String(s)) => s.parse().unwrap_or(default),
            Some(_) => default,
        }
    }

    /// 6-digit RGB or 8-digit RGBA hex -> (0xRRGGBB, alpha 0..1)
    pub fn get_color(&self, section: &str, key: &str, default: (u32, f32)) -> (u32, f32) {
        match self.value(section, key) {
            Some(Value::String(v)) => match v.len() {
                6 => u32::from_str_radix(v, 16).map(|c| (c, 1.0)).unwrap_or(default),
                8 => u32::from_str_radix(v, 16)
                    .map(|c| (c >> 8, (c & 0xFF) as f32 / 255.0))
                    .unwrap_or(default),
                _ => default,
            },
            // An all-digit colour such as 313244 is a valid decimal integer,
            // so an unquoted one parses silently and renders the wrong shade.
            Some(Value::Integer(_)) => {
                eprintln!("optim-bar: [{section}] {key} needs quotes, e.g. {key} = \"313244\"");
                default
            }
            _ => default,
        }
    }

    /// A TOML array, or — tolerated for hand-edited files — the old
    /// comma-separated string form.
    pub fn get_list(&self, section: &str, key: &str) -> Vec<String> {
        match self.value(section, key) {
            Some(Value::Array(a)) => a.iter().filter_map(scalar_text).collect(),
            Some(Value::String(s)) => s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
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
    pub values: Cfg,
}

impl BarConfig {
    /// Widget list for a side on a given bar index. `[left.1]` overrides
    /// `[left]` for the second monitor; unspecified monitors inherit.
    pub fn side_widgets(&self, side: &str, bar_index: usize) -> Vec<String> {
        if bar_index > 0 {
            let section = format!("{side}.{bar_index}");
            if self.values.has(&section, "widgets") {
                return self.values.get_list(&section, "widgets");
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
const DEFAULT_FILE: &str = r##"# optim-bar configuration
# edit, save — the bar watches this file and reloads itself
#
# TOML: strings need quotes, lists are [arrays], numbers and true/false are
# bare. Windows paths go in 'single quotes' — inside "double quotes" a
# backslash starts an escape and C:\Users is an error.

[bar]
height = 36
position = "top"
reserve = true            # register as AppBar: maximized windows stop at the bar
monitors = "all"          # "all" or "primary"
# exclude = ["SAM78B7"]   # device-id substrings that never get a bar
# per-monitor widget overrides: add [left.1] / [center.1] / [right.1]
# tables with their own `widgets =` line; monitors without one inherit
font = "JetBrainsMono NFP"
font_size = 13
bg = "181825D9"           # RGBA — mantle, ~85% opacity
fg = "CDD6F4"             # text
dim = "7F849C"            # overlay1
accent = "B4BEFE"         # lavender
surface = "313244"        # surface0

[left]
widgets = ["workspaces", "tasks"]

[widget.workspaces]
hide_empty = true
min_width = 16            # clickable cell per number, logical px; font unchanged
# active_fill = "accent"  # pill behind the focused workspace: accent | surface | none | RRGGBB
# active_fg = ""          # glyph on that pill; defaults to the bar bg (dark on light)

[widget.tasks]
type = "tasks"

[center]
widgets = ["clock"]

# NOTE: right-side widgets render first-listed NEAREST THE RIGHT EDGE.
# The line below reads visually left-to-right as: systray, mic, volume,
# cpu_temp, gpu_temp.
[right]
widgets = ["gpu_temp", "cpu_temp", "volume", "mic", "systray"]

[widget.systray]
type = "systray"
# collapsed = false       # render icons inline in the bar instead of a flyout
flyout_cols = 5           # icons per row in the flyout
flyout_icon = 18          # drawn icon edge, logical px
flyout_cell = 40          # click target per icon; also the row height
flyout_pad = 8            # border padding around the grid

[widget.volume]
type = "volume"
scroll_step = 2

# Mic mute indicator/toggle (Windows endpoint level; red = muted, click toggles)
[widget.mic]
type = "mic"
hotkey = "alt+shift+m"

# cpu / mem have no bar slot by default — click any temp widget for the
# stats dropdown, which shows both.
[widget.cpu]
type = "cpu"

[widget.mem]
type = "mem"

[widget.gpu_temp]
type = "gpu_temp"
icon = "󰔃"

# CPU die temp via LibreHardwareMonitor's web server — optional; the widget
# hides itself whenever LHM isn't running. sensor_id is hardware-specific;
# the one below is an AMD Ryzen Tctl path. Browse http://localhost:8085 to
# find yours.
[widget.cpu_temp]
type = "lhm"
sensor_id = "/amdcpu/0/temperature/2"
icon = ""

[widget.clock]
format = "%a, %d %b %I:%M %p"
format_alt = "%A, %d %B %Y"

# Generic exec widget: run a command on a timer, click to run others.
# Add its name to a widgets = line above to show it.
# [widget.eq]
# type = "exec"
# run = 'pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\path\to\script.ps1 status'
# interval = 10000
# on_left = 'pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\path\to\script.ps1 menu'
# on_middle = ""
# on_right = ""

# Alt+Tab replacement: a vertical list of window titles instead of thumbnails.
# Alt+Tab steps down, Alt+Shift+Tab steps up, Esc cancels, release Alt to go.
# Each row is numbered: press 1-9 (or 0 for the tenth) to jump straight to it.
# Colors come from [bar]. Set enabled = false to hand Alt+Tab back to Windows.
[switcher]
enabled = true
width = 620               # logical px
max_rows = 14             # taller lists scroll to keep the selection visible
# font = ""               # defaults to the bar font
# font_size = 0           # defaults to bar font_size + 1
# bypass = ["acs.exe"]    # exe names that keep the stock Alt+Tab

# Weather + airing advisor (Open-Meteo, no key). Click any stat widget
# (cpu / mem / gpu_temp / cpu_temp) for the system+weather dropdown.
# Weather stays OFF until you set your coordinates here — they are personal
# data, live only in this local file, and are sent only to open-meteo.com.
# window_bearings: compass directions your windows face; wind arriving
# from within ~60 deg of a bearing counts as feeding the window. Optional;
# without it wind direction is ignored.
# [weather]
# lat = 40.7128           # example: New York City
# lon = -74.0060
# window_bearings = []
# toast = "on"
# toast_threshold = 65
# toast_hours_from = 8
# toast_hours_to = 22
"##;

/// Keys whose value is a colour written as bare hex. `313244` is all digits,
/// so without this list TOML reads it as the decimal integer 313244 and the
/// colour comes out wrong with no error anywhere.
const COLOR_KEYS: &[&str] = &["bg", "fg", "dim", "accent", "surface", "active_fill", "active_fg"];

/// Keys the INI parser split on commas, which become TOML arrays.
const LIST_KEYS: &[&str] = &["widgets", "exclude", "window_bearings", "bypass"];

/// Whether a value should be emitted bare as a TOML number.
///
/// Guards the first character rather than trusting `parse` alone: "nan" and
/// "inf" are both valid `f64`s, and neither is a number anyone wrote on
/// purpose in a config file.
fn looks_numeric(v: &str) -> bool {
    match v.chars().next() {
        Some(c) if c.is_ascii_digit() || c == '-' || c == '+' => {}
        _ => return false,
    }
    v.parse::<i64>().is_ok() || v.parse::<f64>().is_ok()
}

/// A TOML string literal. Windows paths must use the single-quoted literal
/// form: inside a basic string `C:\Users` is an invalid `\U` escape.
fn quote(v: &str) -> String {
    if v.contains('\\') && !v.contains('\'') {
        format!("'{v}'")
    } else {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn toml_scalar(v: &str) -> String {
    if v == "true" || v == "false" || looks_numeric(v) {
        return v.to_string();
    }
    quote(v)
}

/// Converts a pre-0.5.4 INI config to TOML, preserving comments and order.
///
/// Deliberately line-oriented rather than parse-and-re-emit: the comments in
/// this file are its documentation, and a serializer would drop every one.
pub fn ini_to_toml(text: &str) -> String {
    let mut out = String::new();
    let mut seen_header = false;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
            seen_header |= trimmed.starts_with('[');
            out.push_str(raw);
            out.push('\n');
            continue;
        }
        let Some((k, rest)) = trimmed.split_once('=') else {
            out.push_str(raw);
            out.push('\n');
            continue;
        };
        // The INI parser treated everything after '#' as a comment, so no
        // existing value can contain one.
        let (val, comment) = match rest.split_once('#') {
            Some((v, c)) => (v.trim(), Some(c)),
            None => (rest.trim(), None),
        };
        let key = k.trim();
        // Bare keys ahead of any header belonged to the implicit [bar].
        if !seen_header {
            out.push_str("[bar]\n");
            seen_header = true;
        }
        let lower = key.to_lowercase();
        let rendered = if LIST_KEYS.contains(&lower.as_str()) {
            let items: Vec<String> = val
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(toml_scalar)
                .collect();
            format!("[{}]", items.join(", "))
        } else if COLOR_KEYS.contains(&lower.as_str()) {
            quote(val)
        } else {
            toml_scalar(val)
        };
        out.push_str(&format!("{key} = {rendered}"));
        if let Some(c) = comment {
            out.push_str("  #");
            out.push_str(c);
        }
        out.push('\n');
    }
    out
}

fn dir() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("optim-bar")
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

pub fn load() -> BarConfig {
    let p = path();
    if !p.exists() {
        let _ = std::fs::create_dir_all(dir());
        let legacy = dir().join("config.ini");
        match std::fs::read_to_string(&legacy) {
            Ok(old) => {
                if std::fs::write(&p, ini_to_toml(&old)).is_ok() {
                    // Kept rather than deleted: the converter is mechanical
                    // and a hand-tuned config is worth more than the bytes.
                    let _ = std::fs::rename(&legacy, dir().join("config.ini.bak"));
                    crate::toast::show(
                        "optim-bar",
                        "Config migrated to config.toml — the old file is now config.ini.bak",
                    );
                }
            }
            Err(_) => {
                let _ = std::fs::write(&p, DEFAULT_FILE);
            }
        }
    }
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    let values = Cfg::parse(&text);
    BarConfig {
        height: values.get_f32("bar", "height", 36.0).clamp(20.0, 80.0),
        position_top: values.get_or("bar", "position", "bottom") == "top",
        reserve: values.get_or("bar", "reserve", "true") != "false",
        all_monitors: values.get_or("bar", "monitors", "all") != "primary",
        exclude: values.get_list("bar", "exclude"),
        font: values.get_or("bar", "font", "JetBrainsMono NFP"),
        font_size: values.get_f32("bar", "font_size", 13.0).clamp(8.0, 24.0),
        bg: values.get_color("bar", "bg", (0x181825, 0.85)),
        fg: values.get_color("bar", "fg", (0xCDD6F4, 1.0)).0,
        dim: values.get_color("bar", "dim", (0x7F849C, 1.0)).0,
        accent: values.get_color("bar", "accent", (0xB4BEFE, 1.0)).0,
        surface: values.get_color("bar", "surface", (0x313244, 1.0)).0,
        pad: values.get_f32("bar", "pad", 6.0),
        left: values.get_list("left", "widgets"),
        center: values.get_list("center", "widgets"),
        right: values.get_list("right", "widgets"),
        values,
    }
}

/// `--check-config`: migrate if needed, then print what the bar will actually
/// use. A wrong value that parses (`position = "topp"`) silently falls back to
/// its default, and this is the only way to see that happen before restarting.
pub fn check() {
    let c = load(); // migrates config.ini on first run
    let p = path();
    println!("config: {}", p.display());
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    if let Err(e) = text.parse::<toml::Table>() {
        println!("\nPARSE ERROR — the bar falls back to built-in defaults:\n{e}");
        return;
    }
    println!("  height     {}", c.height);
    println!(
        "  position   {}",
        if c.position_top { "top" } else { "bottom" }
    );
    println!("  reserve    {}", c.reserve);
    println!(
        "  monitors   {}",
        if c.all_monitors { "all" } else { "primary" }
    );
    println!("  exclude    {:?}", c.exclude);
    println!("  font       {} @ {}", c.font, c.font_size);
    println!(
        "  colors     bg={:06X}/{:.2} fg={:06X} dim={:06X} accent={:06X} surface={:06X}",
        c.bg.0, c.bg.1, c.fg, c.dim, c.accent, c.surface
    );
    println!("  left       {:?}", c.left);
    println!("  center     {:?}", c.center);
    println!("  right      {:?}", c.right);
    println!(
        "  flyout     cols={} icon={} cell={} pad={}",
        c.values.get_u64("widget.systray", "flyout_cols", 5),
        c.values.get_f32("widget.systray", "flyout_icon", 18.0),
        c.values.get_f32("widget.systray", "flyout_cell", 40.0),
        c.values.get_f32("widget.systray", "flyout_pad", 8.0),
    );
    println!(
        "  switcher   enabled={} width={} max_rows={}",
        c.values.get_or("switcher", "enabled", "true"),
        c.values.get_f32("switcher", "width", 620.0),
        c.values.get_u64("switcher", "max_rows", 14),
    );
    let lat = c.values.get_or("weather", "lat", "");
    println!(
        "  weather    {}",
        if lat.is_empty() {
            "off (no coordinates)".to_string()
        } else {
            format!(
                "lat={lat} lon={} bearings={:?}",
                c.values.get_or("weather", "lon", ""),
                c.values.get_list("weather", "window_bearings")
            )
        }
    );
}

#[cfg(test)]
mod tests {
    use super::{ini_to_toml, Cfg, DEFAULT_FILE};

    fn cfg(text: &str) -> Cfg {
        Cfg {
            root: text.parse().expect("test config must be valid TOML"),
        }
    }

    #[test]
    fn sections_and_colors() {
        let c = cfg("[bar]\nbg = \"181825D9\"\n[widget.clock]\nformat = \"%H:%M\"\n");
        assert_eq!(c.get("widget.clock", "format").as_deref(), Some("%H:%M"));
        let (rgb, a) = c.get_color("bar", "bg", (0, 1.0));
        assert_eq!(rgb, 0x181825);
        assert!((a - 0.851).abs() < 0.01);
    }

    #[test]
    fn lists() {
        let c = cfg("[right]\nwidgets = [\"systray\", \"volume\", \"clock\"]\n");
        assert_eq!(c.get_list("right", "widgets"), ["systray", "volume", "clock"]);
    }

    /// Call sites compare against string literals, so a bare number or bool
    /// has to read back as text.
    #[test]
    fn bare_scalars_read_back_as_text() {
        let c = cfg("[bar]\nheight = 36\nreserve = false\nposition = \"top\"\n");
        assert_eq!(c.get_or("bar", "reserve", "true"), "false");
        assert_eq!(c.get_or("bar", "position", "bottom"), "top");
        assert_eq!(c.get_f32("bar", "height", 0.0), 36.0);
    }

    /// `get` returns None for arrays, so existence checks need `has`.
    #[test]
    fn per_monitor_overrides_are_detectable() {
        let c = cfg("[left]\nwidgets = [\"a\"]\n[left.1]\nwidgets = [\"b\"]\n");
        assert!(c.has("left.1", "widgets"));
        assert!(!c.has("left.2", "widgets"));
        assert_eq!(c.get_list("left.1", "widgets"), ["b"]);
    }

    #[test]
    fn the_shipped_default_is_valid_toml() {
        let c = cfg(DEFAULT_FILE);
        assert_eq!(c.get_color("bar", "surface", (0, 1.0)).0, 0x313244);
        assert_eq!(c.get_u64("widget.systray", "flyout_cols", 0), 5);
    }

    /// The three things TOML would silently misread if the converter got
    /// lazy: all-digit colours, comma lists, and backslash paths.
    #[test]
    fn migration_quotes_what_toml_would_misread() {
        let old = "[bar]\n\
                   surface = 313244        # surface0\n\
                   height = 36\n\
                   reserve = true\n\
                   \n\
                   [right]\n\
                   widgets = gpu_temp, mic, systray\n\
                   \n\
                   [widget.eq]\n\
                   run = pwsh -File C:\\Users\\brian\\eq.ps1 status\n";
        let c = cfg(&ini_to_toml(old));
        // Would have become the integer 313244 without quoting.
        assert_eq!(c.get_color("bar", "surface", (0, 1.0)).0, 0x313244);
        assert_eq!(c.get_f32("bar", "height", 0.0), 36.0);
        assert_eq!(c.get_or("bar", "reserve", "false"), "true");
        assert_eq!(c.get_list("right", "widgets"), ["gpu_temp", "mic", "systray"]);
        // A basic string would have choked on \U in C:\Users.
        assert_eq!(
            c.get_or("widget.eq", "run", ""),
            "pwsh -File C:\\Users\\brian\\eq.ps1 status"
        );
    }

    /// Numeric list members stay numbers and still read back as text.
    ///
    /// Values here are deliberately synthetic — this is a public repo, and a
    /// real lat/lon in a fixture is a home address. See the [weather] note in
    /// DEFAULT_FILE: coordinates live only in the user's local config.
    #[test]
    fn migration_keeps_numeric_lists_usable() {
        let c = cfg(&ini_to_toml("[weather]\nwindow_bearings = 90, 270\nlat = 12.3456\n"));
        assert_eq!(c.get_list("weather", "window_bearings"), ["90", "270"]);
        assert_eq!(c.get_or("weather", "lat", ""), "12.3456");
    }

    /// Comments are the documentation in this file; a serializer would eat them.
    #[test]
    fn migration_preserves_comments() {
        let out = ini_to_toml("# top note\n[bar]\nheight = 36   # inline note\n");
        assert!(out.contains("# top note"));
        assert!(out.contains("# inline note"));
    }
}
