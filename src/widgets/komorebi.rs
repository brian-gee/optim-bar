use std::sync::atomic::{AtomicBool, Ordering};
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

const PIPE_NAME: &str = "optim-bar-komorebi";

#[derive(Clone, PartialEq)]
struct Ws {
    name: String,
    focused: bool,
    populated: bool,
}

#[derive(Clone, PartialEq, Default)]
struct State {
    online: bool,
    ws: Vec<Ws>,
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

fn seed(state: &Arc<Mutex<State>>, monitor: usize) {
    // One-shot `komorebic state` to paint before the first event arrives.
    let out = std::process::Command::new("komorebic")
        .arg("state")
        .creation_flags_hidden()
        .output_string();
    if let Some(v) = Value::parse(&out) {
        if let Some(ws) = extract(&v, monitor) {
            if let Ok(mut s) = state.lock() {
                *s = State { online: true, ws };
            }
        }
    }
}

/// Blocking subscription loop: named pipe + `komorebic subscribe-pipe`.
fn subscribe(state: Arc<Mutex<State>>, alive: Arc<AtomicBool>, monitor: usize) {
    unsafe {
        let path16: Vec<u16> = format!("\\\\.\\pipe\\{PIPE_NAME}")
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

            seed(&state, monitor);
            spawn_hidden(&format!("komorebic subscribe-pipe {PIPE_NAME}"));

            if ConnectNamedPipe(pipe, None).is_ok() {
                let mut acc: Vec<u8> = Vec::new();
                let mut buf = [0u8; 16384];
                loop {
                    let mut read = 0u32;
                    if ReadFile(pipe, Some(&mut buf), Some(&mut read), None).is_err() || read == 0 {
                        break;
                    }
                    acc.extend_from_slice(&buf[..read as usize]);
                    while let Some(nl) = acc.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = acc.drain(..=nl).collect();
                        if let Ok(text) = std::str::from_utf8(&line) {
                            if let Some(v) = Value::parse(text) {
                                if let Some(st) = v.get("state").and_then(|s| extract(s, monitor)) {
                                    if let Ok(mut s) = state.lock() {
                                        *s = State { online: true, ws: st };
                                    }
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

pub struct Workspaces {
    state: Arc<Mutex<State>>,
    alive: Arc<AtomicBool>,
    shown: State,
    hide_empty: bool,
    monitor: usize,
    /// segment index -> workspace index (varies when empties are hidden)
    seg_map: Vec<usize>,
}

impl Workspaces {
    pub fn new(cfg: &BarConfig, section: &str) -> Workspaces {
        let monitor = cfg.ini.get_u64(section, "monitor", 0) as usize;
        let hide_empty = cfg.ini.get_or(section, "hide_empty", "true") != "false";
        let state = Arc::new(Mutex::new(State::default()));
        let alive = Arc::new(AtomicBool::new(true));
        {
            let (s, a) = (state.clone(), alive.clone());
            std::thread::spawn(move || subscribe(s, a, monitor));
        }
        Workspaces {
            state,
            alive,
            shown: State::default(),
            hide_empty,
            monitor,
            seg_map: Vec::new(),
        }
    }
}

impl Drop for Workspaces {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
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
                self.monitor, ws_idx
            ));
        }
    }
}
