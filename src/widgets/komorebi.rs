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

            seed(&state, hmonitor, override_mon);
            spawn_hidden(&format!("komorebic subscribe-pipe {pipe_name}"));

            if ConnectNamedPipe(pipe, None).is_ok() {
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
        Workspaces {
            state,
            alive,
            shown: State::default(),
            hide_empty,
            pipe_name,
            seg_map: Vec::new(),
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
                let role = if w.focused {
                    Role::Accent
                } else if w.populated {
                    Role::Fg
                } else {
                    Role::Dim
                };
                Segment::text(&w.name, role)
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
