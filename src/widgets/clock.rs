use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::config::BarConfig;
use crate::widgets::{Role, Segment, Widget};

const DAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAYS_LONG: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];
const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_LONG: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August",
    "September", "October", "November", "December",
];

/// Current local hour 0-23 (used by the weather toast's quiet-hours gate).
pub fn local_hour() -> u32 {
    unsafe { GetLocalTime().wHour as u32 }
}

/// Minimal strftime: %a %A %d %b %B %m %Y %H %I %M %S %p and %% literals.
fn format_time(fmt: &str) -> String {
    let t = unsafe { GetLocalTime() };
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('a') => out.push_str(DAYS_SHORT[t.wDayOfWeek as usize % 7]),
            Some('A') => out.push_str(DAYS_LONG[t.wDayOfWeek as usize % 7]),
            Some('d') => out.push_str(&format!("{:02}", t.wDay)),
            Some('b') => out.push_str(MONTHS_SHORT[(t.wMonth as usize - 1) % 12]),
            Some('B') => out.push_str(MONTHS_LONG[(t.wMonth as usize - 1) % 12]),
            Some('m') => out.push_str(&format!("{:02}", t.wMonth)),
            Some('Y') => out.push_str(&t.wYear.to_string()),
            Some('H') => out.push_str(&format!("{:02}", t.wHour)),
            Some('I') => {
                let h12 = match t.wHour % 12 {
                    0 => 12,
                    h => h,
                };
                out.push_str(&format!("{:02}", h12));
            }
            Some('M') => out.push_str(&format!("{:02}", t.wMinute)),
            Some('S') => out.push_str(&format!("{:02}", t.wSecond)),
            Some('p') => out.push_str(if t.wHour < 12 { "AM" } else { "PM" }),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

pub struct Clock {
    format: String,
    format_alt: String,
    show_alt: bool,
    text: String,
    style: crate::statspop::Style,
}

impl Clock {
    pub fn new(cfg: &BarConfig, section: &str) -> Clock {
        let format = cfg.ini.get_or(section, "format", "%a, %d %b %I:%M %p");
        let format_alt = cfg.ini.get_or(section, "format_alt", "%A, %d %B %Y");
        let text = format_time(&format);
        Clock {
            format,
            format_alt,
            show_alt: false,
            text,
            style: crate::statspop::Style::from_cfg(cfg),
        }
    }

    fn active_format(&self) -> &str {
        if self.show_alt {
            &self.format_alt
        } else {
            &self.format
        }
    }
}

impl Widget for Clock {
    fn tick(&mut self) -> bool {
        let fresh = format_time(self.active_format());
        if fresh != self.text {
            self.text = fresh;
            true
        } else {
            false
        }
    }

    fn segments(&self) -> Vec<Segment> {
        vec![Segment::text(&self.text, Role::Fg)]
    }

    fn on_click(&mut self, _seg: usize, button: u8) {
        if button == 0 {
            crate::calendar::toggle(self.style.clone());
            return;
        }
        if button == 2 {
            self.show_alt = !self.show_alt;
            self.text = format_time(self.active_format());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn formats_render() {
        let s = format_time("%a, %d %b %I:%M %p");
        assert!(s.contains(':'), "{s}");
        assert!(s.ends_with("AM") || s.ends_with("PM"), "{s}");
        assert_eq!(format_time("100%%"), "100%");
    }
}
