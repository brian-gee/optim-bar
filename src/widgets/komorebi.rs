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

/// One window komorebi is managing, and where it lives.
#[derive(Clone)]
pub struct Managed {
    pub hwnd: isize,
    pub monitor: usize,
    pub workspace: usize,
    pub ws_name: String,
    /// Its workspace is the one currently displayed on that monitor.
    pub visible: bool,
}

/// Every managed window on every monitor, refreshed on each komorebi event.
///
/// komorebi hides off-workspace windows by *cloaking* them, and a cloaked
/// window is indistinguishable from a UWP ghost (`Windows.UI.Core.CoreWindow`
/// and friends, which are cloaked too and must stay out of window lists) by
/// DWM alone. This registry is how the switcher tells the two apart.
/// A `Vec` rather than a `HashMap` because `Mutex::new` is const and the list
/// is a few dozen entries at most.
static MANAGED: Mutex<Vec<Managed>> = Mutex::new(Vec::new());

/// Where komorebi is keeping `hwnd`, if it manages it at all.
pub fn managed(hwnd: isize) -> Option<Managed> {
    MANAGED
        .lock()
        .ok()?
        .iter()
        .find(|m| m.hwnd == hwnd)
        .cloned()
}

pub fn is_managed(hwnd: isize) -> bool {
    MANAGED
        .lock()
        .map(|m| m.iter().any(|w| w.hwnd == hwnd))
        .unwrap_or(false)
}

/// Focuses the workspace holding a window. komorebi uncloaks it asynchronously,
/// so callers must wait for the window to actually surface.
pub fn focus_workspace(monitor: usize, workspace: usize) {
    spawn_hidden(&format!(
        "komorebic focus-monitor-workspace {monitor} {workspace}"
    ));
}

/// Fills the registry from a one-shot `komorebic state`, for code paths that
/// run without a live subscription (the `--list-windows` diagnostic).
pub fn seed_registry() {
    let out = std::process::Command::new("komorebic")
        .arg("state")
        .creation_flags_hidden()
        .output_string();
    if let Some(v) = Value::parse(&out) {
        if let Ok(mut m) = MANAGED.lock() {
            *m = collect_managed(&v);
        }
    }
}

fn window_hwnd(v: &Value) -> Option<isize> {
    v.get("hwnd").and_then(|h| h.as_f64()).map(|h| h as isize)
}

/// Pulls every managed hwnd out of a state JSON, tagged with its workspace.
fn collect_managed(state: &Value) -> Vec<Managed> {
    let mut out = Vec::new();
    let Some(monitors) = state
        .get("monitors")
        .and_then(|m| m.get("elements"))
        .and_then(|e| e.arr())
    else {
        return out;
    };
    for (mon_idx, mon) in monitors.iter().enumerate() {
        let Some(workspaces) = mon.get("workspaces") else {
            continue;
        };
        let focused = workspaces
            .get("focused")
            .and_then(|f| f.as_f64())
            .unwrap_or(0.0) as usize;
        let Some(list) = workspaces.get("elements").and_then(|e| e.arr()) else {
            continue;
        };
        for (ws_idx, ws) in list.iter().enumerate() {
            let ws_name = ws
                .get("name")
                .and_then(|n| n.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| (ws_idx + 1).to_string());
            let mut push = |hwnd: Option<isize>| {
                if let Some(h) = hwnd.filter(|h| *h != 0) {
                    out.push(Managed {
                        hwnd: h,
                        monitor: mon_idx,
                        workspace: ws_idx,
                        ws_name: ws_name.clone(),
                        visible: ws_idx == focused,
                    });
                }
            };
            // Tiled containers.
            if let Some(containers) = ws
                .get("containers")
                .and_then(|c| c.get("elements"))
                .and_then(|e| e.arr())
            {
                for c in containers {
                    if let Some(wins) = c
                        .get("windows")
                        .and_then(|w| w.get("elements"))
                        .and_then(|e| e.arr())
                    {
                        for w in wins {
                            push(window_hwnd(w));
                        }
                    }
                }
            }
            // Monocle and native-maximized windows live outside the container ring.
            if let Some(wins) = ws
                .get("monocle_container")
                .and_then(|m| m.get("windows"))
                .and_then(|w| w.get("elements"))
                .and_then(|e| e.arr())
            {
                for w in wins {
                    push(window_hwnd(w));
                }
            }
            if let Some(w) = ws.get("maximized_window").filter(|v| !v.is_null()) {
                push(window_hwnd(w));
            }
            // Floating windows: a plain array in some versions, a ring in others.
            if let Some(f) = ws.get("floating_windows") {
                let floats = f.arr().or_else(|| f.get("elements").and_then(|e| e.arr()));
                for w in floats.unwrap_or(&[]) {
                    push(window_hwnd(w));
                }
            }
        }
    }
    out
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
    // Whole-desktop registry, not just this bar's monitor: the switcher lists
    // every window on the machine. Bars on other monitors write the same data.
    if let Ok(mut m) = MANAGED.lock() {
        *m = collect_managed(v);
    }
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
            .values
            .get(section, "monitor")
            .and_then(|v| v.parse::<usize>().ok());
        let hide_empty = cfg.values.get_or(section, "hide_empty", "true") != "false";
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
        let active_fill = match cfg.values.get_or(section, "active_fill", "accent").as_str() {
            "none" => None,
            "surface" => Some(cfg.surface),
            "accent" => Some(cfg.accent),
            hex => u32::from_str_radix(hex, 16).ok().or(Some(cfg.accent)),
        };
        let active_fg = match cfg.values.get(section, "active_fg").as_deref() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped after a real `komorebic state` (0.1.41): workspaces is a ring
    /// (`elements` + `focused`), containers hold a `windows` ring, and
    /// `floating_windows` is a ring in this version but a bare array in others.
    const STATE: &str = r#"{
      "monitors": {
        "focused": 0,
        "elements": [
          {
            "id": 65537,
            "workspaces": {
              "focused": 1,
              "elements": [
                {
                  "name": "1",
                  "containers": { "focused": 0, "elements": [
                    { "id": "a", "windows": { "focused": 0, "elements": [
                      { "hwnd": 111, "exe": "brave.exe", "title": "one" },
                      { "hwnd": 222, "exe": "brave.exe", "title": "stacked" }
                    ] } }
                  ] },
                  "monocle_container": null,
                  "maximized_window": null,
                  "floating_windows": { "focused": 0, "elements": [] }
                },
                {
                  "name": "2",
                  "containers": { "focused": 0, "elements": [] },
                  "monocle_container": null,
                  "maximized_window": null,
                  "floating_windows": { "focused": 0, "elements": [
                    { "hwnd": 333, "exe": "mpv.exe", "title": "floating" }
                  ] }
                }
              ]
            }
          },
          {
            "id": 65539,
            "workspaces": {
              "focused": 0,
              "elements": [
                {
                  "containers": { "focused": 0, "elements": [] },
                  "monocle_container": { "id": "b", "windows": { "focused": 0, "elements": [
                    { "hwnd": 444, "exe": "code.exe", "title": "monocle" }
                  ] } },
                  "maximized_window": null,
                  "floating_windows": []
                },
                {
                  "name": "gaming",
                  "containers": { "focused": 0, "elements": [] },
                  "monocle_container": null,
                  "maximized_window": { "hwnd": 555, "exe": "game.exe", "title": "max" },
                  "floating_windows": [
                    { "hwnd": 666, "exe": "discord.exe", "title": "plain array form" }
                  ]
                }
              ]
            }
          }
        ]
      }
    }"#;

    fn parsed() -> Vec<Managed> {
        collect_managed(&Value::parse(STATE).expect("fixture parses"))
    }

    fn find(list: &[Managed], hwnd: isize) -> &Managed {
        list.iter()
            .find(|m| m.hwnd == hwnd)
            .unwrap_or_else(|| panic!("hwnd {hwnd} missing from registry"))
    }

    #[test]
    fn collects_every_window_shape() {
        let all = parsed();
        let mut hwnds: Vec<isize> = all.iter().map(|m| m.hwnd).collect();
        hwnds.sort();
        // tiled x2, floating ring, monocle, maximized, floating plain array
        assert_eq!(hwnds, vec![111, 222, 333, 444, 555, 666]);
    }

    #[test]
    fn tags_windows_with_monitor_and_workspace() {
        let all = parsed();
        let stacked = find(&all, 222);
        assert_eq!((stacked.monitor, stacked.workspace), (0, 0));
        let floating = find(&all, 333);
        assert_eq!((floating.monitor, floating.workspace), (0, 1));
        let maxed = find(&all, 555);
        assert_eq!((maxed.monitor, maxed.workspace), (1, 1));
    }

    /// The whole point: only off-workspace windows need the switcher's
    /// cloak exemption and a workspace switch before activation.
    #[test]
    fn visible_tracks_each_monitors_focused_workspace() {
        let all = parsed();
        assert!(!find(&all, 111).visible, "monitor 0 is showing workspace 1");
        assert!(find(&all, 333).visible);
        assert!(find(&all, 444).visible, "monitor 1 is showing workspace 0");
        assert!(!find(&all, 555).visible);
        assert!(!find(&all, 666).visible);
    }

    #[test]
    fn workspace_name_falls_back_to_its_index() {
        let all = parsed();
        assert_eq!(find(&all, 333).ws_name, "2");
        assert_eq!(find(&all, 666).ws_name, "gaming");
        // Unnamed workspace: komorebi's own 1-based label.
        assert_eq!(find(&all, 444).ws_name, "1");
    }

    #[test]
    fn survives_junk_state() {
        assert!(collect_managed(&Value::parse("{}").unwrap()).is_empty());
        assert!(collect_managed(&Value::parse(r#"{"monitors":null}"#).unwrap()).is_empty());
        assert!(collect_managed(
            &Value::parse(r#"{"monitors":{"elements":[{"workspaces":{"elements":[{}]}}]}}"#)
                .unwrap()
        )
        .is_empty());
    }

    #[test]
    fn lookup_helpers_read_the_registry() {
        if let Ok(mut m) = MANAGED.lock() {
            *m = parsed();
        }
        assert!(is_managed(444));
        assert!(!is_managed(999));
        assert_eq!(managed(666).map(|m| m.ws_name), Some("gaming".to_string()));
        assert!(managed(999).is_none());
    }
}
