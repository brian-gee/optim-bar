//! Native system stats: CPU %, RAM %, GPU temp via NVML.
//! No drivers, no helper processes — straight from the OS and nvml.dll.

use std::ffi::c_void;

use windows::core::{w, PCSTR};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

use crate::config::BarConfig;
use crate::statspop;
use crate::widgets::{Role, Segment, Widget};

/// Ticks between refreshes (master tick is 250 ms → 8 = 2 s).
const REFRESH_TICKS: u32 = 8;

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

pub struct Cpu {
    icon: String,
    last_idle: u64,
    last_busy: u64,
    text: String,
    ticks: u32,
    style: statspop::Style,
}

impl Cpu {
    pub fn new(cfg: &BarConfig, section: &str) -> Cpu {
        Cpu {
            icon: cfg.values.get_or(section, "icon", "\u{f4bc}"),
            last_idle: 0,
            last_busy: 0,
            text: String::new(),
            ticks: 0,
            style: statspop::Style::from_cfg(cfg),
        }
    }
}

impl Widget for Cpu {
    fn tick(&mut self) -> bool {
        self.ticks += 1;
        if self.ticks % REFRESH_TICKS != 1 {
            return false;
        }
        unsafe {
            let (mut idle, mut kernel, mut user) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default());
            if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_err() {
                return false;
            }
            // Kernel time includes idle; busy = (kernel - idle) + user.
            let idle_v = ft(idle);
            let busy_v = ft(kernel) - idle_v + ft(user);
            let di = idle_v.saturating_sub(self.last_idle);
            let db = busy_v.saturating_sub(self.last_busy);
            self.last_idle = idle_v;
            self.last_busy = busy_v;
            let total = di + db;
            if total == 0 || db > total * 2 {
                return false; // first sample
            }
            let pct = (db * 100 / total).min(100);
            let fresh = format!("{} {pct}%", self.icon);
            if fresh != self.text {
                self.text = fresh;
                return true;
            }
        }
        false
    }

    fn segments(&self) -> Vec<Segment> {
        if self.text.is_empty() {
            return Vec::new();
        }
        vec![Segment::text(&self.text, Role::Fg)]
    }

    fn on_click(&mut self, _seg: usize, button: u8) {
        if button == 0 {
            statspop::toggle(self.style.clone());
        }
    }
}

pub struct Mem {
    icon: String,
    text: String,
    ticks: u32,
    style: statspop::Style,
}

impl Mem {
    pub fn new(cfg: &BarConfig, section: &str) -> Mem {
        Mem {
            icon: cfg.values.get_or(section, "icon", "\u{efc5}"),
            text: String::new(),
            ticks: 0,
            style: statspop::Style::from_cfg(cfg),
        }
    }
}

impl Widget for Mem {
    fn tick(&mut self) -> bool {
        self.ticks += 1;
        if self.ticks % REFRESH_TICKS != 1 {
            return false;
        }
        unsafe {
            let mut ms = MEMORYSTATUSEX {
                dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
                ..Default::default()
            };
            if GlobalMemoryStatusEx(&mut ms).is_err() {
                return false;
            }
            let fresh = format!("{} {}%", self.icon, ms.dwMemoryLoad);
            if fresh != self.text {
                self.text = fresh;
                return true;
            }
        }
        false
    }

    fn segments(&self) -> Vec<Segment> {
        if self.text.is_empty() {
            return Vec::new();
        }
        vec![Segment::text(&self.text, Role::Fg)]
    }

    fn on_click(&mut self, _seg: usize, button: u8) {
        if button == 0 {
            statspop::toggle(self.style.clone());
        }
    }
}

/// NVML function table, loaded once from the driver's nvml.dll.
struct Nvml {
    device: *mut c_void,
    get_temperature: unsafe extern "C" fn(*mut c_void, i32, *mut u32) -> i32,
}

// The NVML device handle is opaque and thread-agnostic; we only use it on
// the UI thread anyway.
unsafe impl Send for Nvml {}

fn nvml_load() -> Option<Nvml> {
    unsafe {
        let lib = LoadLibraryW(w!("nvml.dll")).ok()?;
        let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(GetProcAddress(
            lib,
            PCSTR(b"nvmlInit_v2\0".as_ptr()),
        )?);
        let by_index: unsafe extern "C" fn(u32, *mut *mut c_void) -> i32 = std::mem::transmute(
            GetProcAddress(lib, PCSTR(b"nvmlDeviceGetHandleByIndex_v2\0".as_ptr()))?,
        );
        let get_temperature = std::mem::transmute(GetProcAddress(
            lib,
            PCSTR(b"nvmlDeviceGetTemperature\0".as_ptr()),
        )?);
        if init() != 0 {
            return None;
        }
        let mut device: *mut c_void = std::ptr::null_mut();
        if by_index(0, &mut device) != 0 {
            return None;
        }
        Some(Nvml {
            device,
            get_temperature,
        })
    }
}

pub struct GpuTemp {
    icon: String,
    nvml: Option<Nvml>,
    text: String,
    ticks: u32,
    style: statspop::Style,
}

impl GpuTemp {
    pub fn new(cfg: &BarConfig, section: &str) -> GpuTemp {
        GpuTemp {
            icon: cfg.values.get_or(section, "icon", "\u{f0503}"),
            nvml: nvml_load(),
            text: String::new(),
            ticks: 0,
            style: statspop::Style::from_cfg(cfg),
        }
    }
}

impl Widget for GpuTemp {
    fn tick(&mut self) -> bool {
        self.ticks += 1;
        if self.ticks % REFRESH_TICKS != 1 {
            return false;
        }
        let Some(nvml) = &self.nvml else { return false };
        unsafe {
            let mut temp = 0u32;
            // 0 = NVML_TEMPERATURE_GPU
            if (nvml.get_temperature)(nvml.device, 0, &mut temp) != 0 {
                return false;
            }
            let fresh = format!("{} {temp}\u{b0}", self.icon);
            if fresh != self.text {
                self.text = fresh;
                return true;
            }
        }
        false
    }

    fn segments(&self) -> Vec<Segment> {
        if self.text.is_empty() {
            return Vec::new(); // no NVIDIA driver -> widget hides
        }
        vec![Segment::text(&self.text, Role::Fg)]
    }

    fn on_click(&mut self, _seg: usize, button: u8) {
        if button == 0 {
            statspop::toggle(self.style.clone());
        }
    }
}
