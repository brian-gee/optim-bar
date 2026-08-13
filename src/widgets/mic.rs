//! Default-microphone mute indicator + toggle, at the Windows endpoint level.
//! Replaces the yasb Voicemod widget: Voicemod's Control API only accepts
//! cloud-registered client keys (dev-portal issued), but endpoint mute works
//! regardless of what sits above it in the chain - muting the capture device
//! silences Discord & co. no matter which app's mute button you never pressed.

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use crate::config::BarConfig;
use crate::widgets::{Role, Segment, Widget};

/// Same red the old yasb voicemod widget used for muted.
const MUTED_RED: u32 = 0xF38BA8;

fn endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        // eCommunications: the role Discord/voice apps capture from.
        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eCommunications)
            .ok()?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

pub struct Mic {
    ep: Option<IAudioEndpointVolume>,
    icon_live: String,
    icon_muted: String,
    muted: Option<bool>, // None = no capture device
    ticks: u32,
    hotkey: Option<(u32, u32)>,
}

impl Mic {
    pub fn new(cfg: &BarConfig, section: &str) -> Mic {
        Mic {
            ep: endpoint(),
            icon_live: cfg.ini.get_or(section, "icon", "\u{f130}"), // nf-fa-microphone
            icon_muted: cfg.ini.get_or(section, "icon_muted", "\u{f131}"),
            muted: None,
            ticks: 0,
            hotkey: cfg
                .ini
                .get(section, "hotkey")
                .and_then(crate::config::parse_hotkey),
        }
    }

    fn toggle_mute(&mut self) {
        unsafe {
            if self.ep.is_none() {
                self.ep = endpoint();
            }
            if let Some(ep) = &self.ep {
                let mute = ep.GetMute().map(|m| m.as_bool()).unwrap_or(false);
                let _ = ep.SetMute(!mute, std::ptr::null());
            }
        }
        self.refresh();
    }

    fn refresh(&mut self) -> bool {
        unsafe {
            if self.ep.is_none() {
                self.ep = endpoint();
            }
            let fresh = match &self.ep {
                Some(ep) => match ep.GetMute() {
                    Ok(m) => Some(m.as_bool()),
                    Err(_) => {
                        self.ep = None; // device changed; reacquire next tick
                        None
                    }
                },
                None => None,
            };
            if fresh != self.muted {
                self.muted = fresh;
                return true;
            }
            false
        }
    }
}

impl Widget for Mic {
    fn tick(&mut self) -> bool {
        self.ticks += 1;
        if self.ticks % 2 != 1 {
            return false; // 500 ms cadence
        }
        self.refresh()
    }

    fn segments(&self) -> Vec<Segment> {
        match self.muted {
            None => Vec::new(), // no capture device: hide
            Some(true) => vec![Segment::text(&self.icon_muted, Role::Custom(MUTED_RED))],
            Some(false) => vec![Segment::text(&self.icon_live, Role::Fg)],
        }
    }

    fn on_click(&mut self, _seg: usize, button: u8) {
        if button == 0 || button == 2 {
            self.toggle_mute();
        }
    }

    fn hotkey_spec(&self) -> Option<(u32, u32)> {
        self.hotkey
    }

    fn on_hotkey(&mut self) {
        self.toggle_mute();
    }
}
