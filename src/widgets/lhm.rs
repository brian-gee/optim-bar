//! Optional LibreHardwareMonitor sensor widget. CPU die temp needs LHM's
//! kernel driver, so this reads its web server (localhost:8085/data.json)
//! exactly like YASB did — but only if it's reachable. Unreachable = the
//! widget hides; the bar never depends on LHM.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::BarConfig;
use crate::json::Value;
use crate::widgets::{Role, Segment, Widget};

fn http_get_json(host: &str, port: u16, path: &str) -> Option<Value> {
    let mut stream =
        TcpStream::connect_timeout(&format!("{host}:{port}").parse().ok()?, Duration::from_millis(800))
            .ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(1500))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let body = text.split_once("\r\n\r\n")?.1;
    // Body may be chunked; find the first '{' and parse from there — the
    // parser stops at the end of the value, trailing chunk framing is ignored.
    let start = body.find('{')?;
    Value::parse(&body[start..])
}

/// One-shot read of a sensor's numeric value (used by the stats popup).
pub fn read_sensor(host: &str, port: u16, sensor_id: &str) -> Option<f64> {
    http_get_json(host, port, "/data.json")
        .as_ref()
        .and_then(|root| find_sensor(root, sensor_id))
        .and_then(|node| node.get("Value")?.as_str())
        .and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok())
}

/// Depth-first search for the node whose SensorId matches.
fn find_sensor<'a>(node: &'a Value, sensor_id: &str) -> Option<&'a Value> {
    if node.get("SensorId").and_then(|s| s.as_str()) == Some(sensor_id) {
        return Some(node);
    }
    for child in node.get("Children").and_then(|c| c.arr()).unwrap_or(&[]) {
        if let Some(hit) = find_sensor(child, sensor_id) {
            return Some(hit);
        }
    }
    None
}

pub struct Lhm {
    state: Arc<Mutex<String>>, // formatted text, empty = hidden
    alive: Arc<AtomicBool>,
    shown: String,
    style: crate::statspop::Style,
}

impl Lhm {
    pub fn new(cfg: &BarConfig, section: &str) -> Lhm {
        let sensor_id = cfg.values.get_or(section, "sensor_id", "");
        let icon = cfg.values.get_or(section, "icon", "\u{f4bc}");
        let host = cfg.values.get_or(section, "host", "localhost");
        let port = cfg.values.get_u64(section, "port", 8085) as u16;
        let interval = cfg.values.get_u64(section, "interval", 2000).max(1000);

        let state = Arc::new(Mutex::new(String::new()));
        let alive = Arc::new(AtomicBool::new(true));
        {
            let (state, alive) = (state.clone(), alive.clone());
            std::thread::spawn(move || {
                let host = if host == "localhost" { "127.0.0.1".into() } else { host };
                while alive.load(Ordering::Relaxed) {
                    let text = http_get_json(&host, port, "/data.json")
                        .as_ref()
                        .and_then(|root| find_sensor(root, &sensor_id))
                        .and_then(|node| node.get("Value")?.as_str())
                        .and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok())
                        .map(|v| format!("{icon} {}\u{b0}", v.round() as i64))
                        .unwrap_or_default();
                    if let Ok(mut s) = state.lock() {
                        *s = text;
                    }
                    std::thread::sleep(Duration::from_millis(interval));
                }
            });
        }
        Lhm {
            state,
            alive,
            shown: String::new(),
            style: crate::statspop::Style::from_cfg(cfg),
        }
    }
}

impl Drop for Lhm {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

impl Widget for Lhm {
    fn tick(&mut self) -> bool {
        let fresh = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        if fresh != self.shown {
            self.shown = fresh;
            true
        } else {
            false
        }
    }

    fn segments(&self) -> Vec<Segment> {
        if self.shown.is_empty() {
            return Vec::new(); // LHM not running -> hidden by design
        }
        vec![Segment::text(&self.shown, Role::Fg)]
    }

    fn on_click(&mut self, _seg: usize, button: u8) {
        if button == 0 {
            crate::statspop::toggle(self.style.clone());
        }
    }
}
