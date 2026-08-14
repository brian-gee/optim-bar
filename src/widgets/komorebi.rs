use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{ReadFile, PIPE_ACCESS_INBOUND};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};

use crate::config::BarConfig;
use crate::json::Value;
use crate::widgets::exec::spawn_hidden;
use crate::widgets::{Role, Segment, Widget};


#[derive(Clone, PartialEq)]
struct Ws {
    name: String,
    focused: bool,
    populated: bool,
}

#[derive(Clone, PartialEq, Default)]
struct State {
    online: bool,
    /// komorebi's index for our monitor, resolved from the last state JSON.
    mon_idx: usize,
    ws: Vec<Ws>,
}

/// komorebi's monitor index for the bar's monitor. komorebi's `id` field is
/// the HMONITOR value, so matching by it survives ordering differences
/// between komorebi's enumeration and ours. `override_mon` (config
/// `monitor =`) wins when set.
fn resolve(state: &Value, hmonitor: isize, override_mon: Option<usize>) -> Option<usize> {
    if let Some(m) = override_mon {
        return Some(m);
    }
    state
        .get("monitors")?
        .get("elements")?
        .arr()?
        .iter()
        .position(|m| {
            m.get("id").and_then(|v| v.as_f64()).map(|v| v as isize) == Some(hmonitor)
        })
}

/// Extracts our monitor's workspace list from a komorebi state JSON.
fn extract(state: &Value, monitor: usize) -> Option<Vec<Ws>> {
    let workspaces = state
        .get("monitors")?
        .get("elements")?
        .idx(monitor)?
        .get("workspaces")?;
    let focused = workspaces.get("focused")?.as_f64()? as usize;
    let mut out = Vec::new();
    for (i, ws) in workspaces.get("elements")?.arr()?.iter().enumerate() {
        let name = ws
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| (i + 1).to_string());
        let containers = ws
            .get("containers")
            .and_then(|c| c.get("elements"))
            .and_then(|e| e.arr())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let monocle = ws.get("monocle_container").map(|v| !v.is_null()).unwrap_or(false);
        let maximized = ws.get("maximized_window").map(|v| !v.is_null()).unwrap_or(false);
        let floating = ws
            .get("floating_windows")
            .map(|f| {
                // plain array in some versions, {elements: []} ring in others
                f.arr().map(|a| !a.is_empty()).unwrap_or_else(|| {
                    f.get("elements")
                        .and_then(|e| e.arr())
                        .map(|a| !a.is_empty())
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        out.push(Ws {
            name,
            focused: i == focused,
            populated: containers || monocle || maximized || floating,
        });
    }
    Some(out)
}

/// Applies a state JSON to the shared State. Connected-but-unresolvable
/// (komorebi doesn't manage this monitor) shows as online with no
/// workspaces, which hides the widget instead of claiming "offline".
fn apply(state: &Arc<Mutex<State>>, v: &Value, hmonitor: isize, override_mon: Option<usize>) {
    let idx = resolve(v, hmonitor, override_mon);
    let ws = idx.and_then(|i| extract(v, i)).unwrap_or_default();
    if let Ok(mut s) = state.lock() {
        *s = State {
            online: true,
            mon_idx: idx.unwrap_or(0),
            ws,
        };
    }
}

fn seed(state: &Arc<Mutex<State>>, hmonitor: isize, override_mon: Option<usize>) {
    // One-shot `komorebic state` to paint before the first event arrives.
    let out = std::process::Command::new("komorebic")
        .arg("state")
        .creation_flags_hidden()
        .output_string();
    if let Some(v) = Value::parse(&out) {
        apply(state, &v, hmonitor, override_mon);
    }
}

/// Blocking subscription loop: named pipe + `komorebic subscribe-pipe`.
/// The pipe name is unique per widget instance (pid + counter): instances
/// allow exactly one connection, so a name shared across bars — or reused
/// across a rebuild while the old thread still holds the pipe — leaves
/// every later subscriber dead and the widget stuck on "offline".
fn subscribe(
    state: Arc<Mutex<State>>,
    alive: Arc<AtomicBool>,
    hmonitor: isize,
    override_mon: Option<usize>,
    pipe_name: String,
) {
    unsafe {
        let path16: Vec<u16> = format!("\\\\.\\pipe\\{pipe_name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        while alive.load(Ordering::Relaxed) {
            let pipe = CreateNamedPipeW(
                PCWSTR(path16.as_ptr()),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                1 << 20,
                1 << 20,
                0,
                None,
            );
            if pipe.is_invalid() {
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }

            // Register with komorebi, retrying until it's actually running —
            // at boot the bar regularly starts seconds before komorebi, and
            // a failed fire-and-forget registration used to leave the
            // blocking ConnectNamedPipe below waiting forever ("offline").
            let mut registered = false;
            while alive.load(Ordering::Relaxed) {
                let ok = std::process::Command::new("komorebic")
                    .args(["subscribe-pipe", &pipe_name])
                    .creation_flags_hidden()
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if ok {
                    registered = true;
                    break;
                }
                std::thread::sleep(Duration::from_secs(3));
            }
            if !registered {
                let _ = CloseHandle(pipe);
                continue; // alive turned false; loop exits above
            }

            seed(&state, hmonitor, override_mon);

            // komorebi may have connected the instant we registered, before
            // we reach ConnectNamedPipe — that surfaces as
            // ERROR_PIPE_CONNECTED, which IS a successful connection.
            let connected = match ConnectNamedPipe(pipe, None) {
                Ok(()) => true,
                Err(e) => {
                    e.code()
                        == windows::Win32::Foundation::ERROR_PIPE_CONNECTED.to_hresult()
                }
            };
            if connected {
                let mut acc: Vec<u8> = Vec::new();
                let mut buf = [0u8; 16384];
                loop {
                    let mut read = 0u32;
                    if ReadFile(pipe, Some(&mut buf), Some(&mut read), None).is_err()
                        || read == 0
                        || !alive.load(Ordering::Relaxed)
                    {
                        break;
                    }
                    acc.extend_from_slice(&buf[..read as usize]);
                    while let Some(nl) = acc.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = acc.drain(..=nl).collect();
                        if let Ok(text) = std::str::from_utf8(&line) {
                            if let Some(v) = Value::parse(text) {
                                if let Some(st) = v.get("state") {
                                    apply(&state, st, hmonitor, override_mon);
                                }
                            }
                        }
                    }
                }
            }
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
            if let Ok(mut s) = state.lock() {
                s.online = false;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }
}

trait HiddenExt {
    fn creation_flags_hidden(&mut self) -> &mut Self;
    fn output_string(&mut self) -> String;
}
impl HiddenExt for std::process::Command {
    fn creation_flags_hidden(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000)
    }
    fn output_string(&mut self) -> String {
        self.output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    }
}

static PIPE_SEQ: AtomicUsize = AtomicUsize::new(0);

pub struct Workspaces {
    state: Arc<Mutex<State>>,
    alive: Arc<AtomicBool>,
    shown: State,
    hide_empty: bool,
    pipe_name: String,
    /// segment index -> workspace index (varies when empties are hidden)
    seg_map: Vec<usize>,
    /// Pill behind the focused workspace, and the text color on top of it.
    /// None disables the pill and falls back to a plain accent glyph.
    active_fill: Option<u32>,
    active_fg: u32,
}

impl Workspaces {
    pub fn new(cfg: &BarConfig, section: &str, _bar_index: usize, hmonitor: isize) -> Workspaces {
        // The bar's HMONITOR is matched against komorebi's monitor ids at
        // state time; `monitor =` overrides for the rare mismatch.
        let override_mon = cfg
            .ini
            .get(section, "monitor")
            .and_then(|v| v.parse::<usize>().ok());
        let hide_empty = cfg.ini.get_or(section, "hide_empty", "true") != "false";
        let pipe_name = format!(
            "optim-bar-komorebi-{}-{}",
            std::process::id(),
            PIPE_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let state = Arc::new(Mutex::new(State::default()));
        let alive = Arc::new(AtomicBool::new(true));
        {
            let (s, a, p) = (state.clone(), alive.clone(), pipe_name.clone());
            std::thread::spawn(move || subscribe(s, a, hmonitor, override_mon, p));
        }
        // Accent-on-bg by default: two light Catppuccin foregrounds (accent
        // B4BEFE vs text CDD6F4) are too close to tell apart at a glance, so
        // the focused workspace gets a filled pill instead of another shade.
        let active_fill = match cfg.ini.get_or(section, "active_fill", "accent").as_str() {
            "none" => None,
            "surface" => Some(cfg.surface),
            "accent" => Some(cfg.accent),
            hex => u32::from_str_radix(hex, 16).ok().or(Some(cfg.accent)),
        };
        let active_fg = match cfg.ini.get(section, "active_fg") {
            Some("accent") => cfg.accent,
            Some("fg") => cfg.fg,
            Some(hex) => u32::from_str_radix(hex, 16).unwrap_or(cfg.bg.0),
            // Dark glyph on a light pill; readable without touching font size.
            None => cfg.bg.0,
        };
        Workspaces {
            state,
            alive,
            shown: State::default(),
            hide_empty,
            pipe_name,
            seg_map: Vec::new(),
            active_fill,
            active_fg,
        }
    }
}

impl Drop for Workspaces {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        // komorebi drops the subscription and disconnects the pipe, which
        // unblocks the reader thread's ReadFile so it can exit.
        spawn_hidden(&format!("komorebic unsubscribe-pipe {}", self.pipe_name));
    }
}

impl Widget for Workspaces {
    fn tick(&mut self) -> bool {
        let fresh = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        if fresh != self.shown {
            self.shown = fresh;
            self.seg_map = self
                .shown
                .ws
                .iter()
                .enumerate()
                .filter(|(_, w)| !self.hide_empty || w.populated || w.focused)
                .map(|(i, _)| i)
                .collect();
            true
        } else {
            false
        }
    }

    fn segments(&self) -> Vec<Segment> {
        if !self.shown.online {
            return vec![Segment::text("komorebi offline", Role::Dim)];
        }
        self.shown
            .ws
            .iter()
            .filter(|w| !self.hide_empty || w.populated || w.focused)
            .map(|w| {
                match (w.focused, self.active_fill) {
                    (true, Some(fill)) => {
                        Segment::text(&w.name, Role::Custom(self.active_fg)).with_fill(fill)
                    }
                    (true, None) => Segment::text(&w.name, Role::Accent),
                    _ if w.populated => Segment::text(&w.name, Role::Fg),
                    _ => Segment::text(&w.name, Role::Dim),
                }
            })
            .collect()
    }

    fn on_click(&mut self, seg: usize, button: u8) {
        if button != 0 || !self.shown.online {
            return;
        }
        if let Some(&ws_idx) = self.seg_map.get(seg) {
            spawn_hidden(&format!(
                "komorebic focus-monitor-workspace {} {}",
                self.shown.mon_idx, ws_idx
            ));
        }
    }
}
