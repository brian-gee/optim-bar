use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use crate::config::BarConfig;
use crate::widgets::exec::spawn_hidden;
use crate::widgets::{Role, Segment, Widget};

fn endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

pub struct Volume {
    ep: Option<IAudioEndpointVolume>,
    icons: Vec<String>, // [muted, low, mid, high]
    step: f32,
    text: String,
    muted: bool,
    ticks: u32,
}

impl Volume {
    pub fn new(cfg: &BarConfig, section: &str) -> Volume {
        let icons = {
            let list = cfg.ini.get_list(section, "icons");
            if list.len() >= 4 {
                list
            } else {
                vec![
                    "\u{eee8}".into(), // muted
                    "\u{f057f}".into(), // low
                    "\u{f0580}".into(), // medium
                    "\u{f057e}".into(), // high
                ]
            }
        };
        Volume {
            ep: endpoint(),
            icons,
            step: cfg.ini.get_f32(section, "scroll_step", 2.0) / 100.0,
            text: String::new(),
            muted: false,
            ticks: 0,
        }
    }

    fn refresh(&mut self) -> bool {
        unsafe {
            if self.ep.is_none() {
                self.ep = endpoint();
            }
            let Some(ep) = &self.ep else {
                return if self.text.is_empty() {
                    false
                } else {
                    self.text.clear();
                    true
                };
            };
            let (level, mute) = match (ep.GetMasterVolumeLevelScalar(), ep.GetMute()) {
                (Ok(l), Ok(m)) => (l, m.as_bool()),
                _ => {
                    self.ep = None; // device changed; reacquire next tick
                    return false;
                }
            };
            let pct = (level * 100.0).round() as u32;
            let icon = if mute {
                &self.icons[0]
            } else if pct < 33 {
                &self.icons[1]
            } else if pct < 66 {
                &self.icons[2]
            } else {
                &self.icons[3]
            };
            self.muted = mute;
            let fresh = format!("{icon} {pct}%");
            if fresh != self.text {
                self.text = fresh;
                return true;
            }
            false
        }
    }
}

impl Widget for Volume {
    fn tick(&mut self) -> bool {
        self.ticks += 1;
        if self.ticks % 2 != 1 {
            return false; // 500 ms cadence
        }
        self.refresh()
    }

    fn segments(&self) -> Vec<Segment> {
        if self.text.is_empty() {
            return Vec::new();
        }
        let role = if self.muted { Role::Dim } else { Role::Fg };
        vec![Segment::text(&self.text, role)]
    }

    fn on_click(&mut self, _seg: usize, button: u8) {
        unsafe {
            match button {
                2 => {
                    if let Some(ep) = &self.ep {
                        let mute = ep.GetMute().map(|m| m.as_bool()).unwrap_or(false);
                        let _ = ep.SetMute(!mute, std::ptr::null());
                    }
                }
                0 => spawn_hidden("sndvol.exe"), // per-app mixer until a popup lands
                _ => {}
            }
        }
        self.refresh();
    }

    fn on_wheel(&mut self, delta: i32) {
        unsafe {
            if let Some(ep) = &self.ep {
                if let Ok(level) = ep.GetMasterVolumeLevelScalar() {
                    let next = (level + self.step * delta.signum() as f32).clamp(0.0, 1.0);
                    let _ = ep.SetMasterVolumeLevelScalar(next, std::ptr::null());
                }
            }
        }
        self.refresh();
    }
}
