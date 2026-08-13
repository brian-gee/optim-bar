//! Weather + apartment-airing advisor.
//!
//! Fetches the Open-Meteo hourly forecast (free, no key) over WinHTTP every
//! 30 minutes on a background thread, scores every forecast hour for "open
//! the windows" quality, and keeps the result in a process-wide state the
//! stats dropdown reads. The scoring cares about wind *direction* because
//! the apartment only has windows on two bearings — wind has to arrive from
//! roughly those directions to actually flow through.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WINHTTP_ACCESS_TYPE_NO_PROXY,
    WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
};

use crate::config::BarConfig;
use crate::json::Value;

/// One scored forecast hour.
#[derive(Clone)]
pub struct HourScore {
    /// Local time as "YYYY-MM-DDTHH:MM" straight from the API.
    pub time: String,
    pub score: u32, // 0-100
    pub wind_kmh: f64,
    pub wind_dir: f64,
    pub temp_c: f64,
    pub humidity: f64,
}

#[derive(Clone, Default)]
pub struct Current {
    pub temp_c: f64,
    pub humidity: f64,
    pub wind_kmh: f64,
    pub wind_dir: f64,
    pub score: u32,
}

#[derive(Default)]
pub struct WeatherState {
    pub current: Option<Current>,
    /// All forecast hours, chronological.
    pub hours: Vec<HourScore>,
    /// Config echo so the popup can annotate ("wind feeds NE window").
    pub bearings: Vec<f64>,
    /// Score at or above this counts as a good airing window.
    pub threshold: u32,
    /// False until a [weather] location is configured; the popup shows a
    /// "set location" hint instead of weather rows.
    pub configured: bool,
}

static STATE: OnceLock<Mutex<WeatherState>> = OnceLock::new();

pub fn state() -> &'static Mutex<WeatherState> {
    STATE.get_or_init(|| Mutex::new(WeatherState::default()))
}

pub struct WeatherCfg {
    pub lat: f64,
    pub lon: f64,
    pub bearings: Vec<f64>,
    pub toast: bool,
    pub toast_threshold: u32,
    pub toast_from_hour: u32,
    pub toast_to_hour: u32,
}

/// None unless the user has configured a location — there is deliberately
/// no default: coordinates are personal data and never ship in the binary.
pub fn read_cfg(cfg: &BarConfig) -> Option<WeatherCfg> {
    let s = "weather";
    let lat: f64 = cfg.ini.get(s, "lat")?.trim().parse().ok()?;
    let lon: f64 = cfg.ini.get(s, "lon")?.trim().parse().ok()?;
    let bearings: Vec<f64> = cfg
        .ini
        .get_or(s, "window_bearings", "")
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    Some(WeatherCfg {
        lat,
        lon,
        // Without bearings every direction counts (alignment factor maxed).
        bearings,
        toast: cfg.ini.get_or(s, "toast", "on") != "off",
        toast_threshold: cfg.ini.get_u64(s, "toast_threshold", 65) as u32,
        toast_from_hour: cfg.ini.get_u64(s, "toast_hours_from", 8) as u32,
        toast_to_hour: cfg.ini.get_u64(s, "toast_hours_to", 22) as u32,
    })
}

/// Smallest angular distance between two bearings, degrees 0-180.
fn ang_dist(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// 1.0 at the center of a band, linearly to 0.0 at the edges.
fn band(v: f64, zero_lo: f64, one_lo: f64, one_hi: f64, zero_hi: f64) -> f64 {
    if v <= zero_lo || v >= zero_hi {
        0.0
    } else if v < one_lo {
        (v - zero_lo) / (one_lo - zero_lo)
    } else if v <= one_hi {
        1.0
    } else {
        1.0 - (v - one_hi) / (zero_hi - one_hi)
    }
}

/// Airing quality 0-100 for one forecast hour.
pub fn score_hour(
    bearings: &[f64],
    wind_dir: f64,
    wind_kmh: f64,
    temp_c: f64,
    humidity: f64,
    precip_prob: f64,
) -> u32 {
    // Wind must come FROM roughly a window bearing to blow in. Full credit
    // inside +-25 deg, fading to nothing at +-70 deg. No configured
    // bearings = direction doesn't matter.
    let align = if bearings.is_empty() {
        1.0
    } else {
        bearings
            .iter()
            .map(|b| band(ang_dist(wind_dir, *b), -1.0, 0.0, 25.0, 70.0))
            .fold(0.0f64, f64::max)
    };
    let speed = band(wind_kmh, 3.0, 8.0, 24.0, 38.0); // breeze, not a gale
    let temp = band(temp_c, 11.0, 17.0, 27.0, 33.0);
    let humid = band(humidity, -1.0, 0.0, 60.0, 85.0); // Florida-calibrated
    let rain_gate = if precip_prob >= 60.0 {
        0.0
    } else {
        1.0 - (precip_prob / 100.0) * 0.6
    };
    let weighted = 0.35 * align + 0.20 * speed + 0.20 * temp + 0.25 * humid;
    (weighted * rain_gate * 100.0).round() as u32
}

/// Blocking HTTPS GET via WinHTTP; returns the response body.
fn https_get(host: &str, path: &str) -> Option<Vec<u8>> {
    unsafe {
        let host16: Vec<u16> = host.encode_utf16().chain(std::iter::once(0)).collect();
        let path16: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let session = WinHttpOpen(
            w!("optim-bar/weather"),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            return None;
        }
        let mut out = None;
        let conn = WinHttpConnect(session, PCWSTR(host16.as_ptr()), 443, 0);
        if !conn.is_null() {
            let req = WinHttpOpenRequest(
                conn,
                w!("GET"),
                PCWSTR(path16.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null_mut(),
                WINHTTP_OPEN_REQUEST_FLAGS(WINHTTP_FLAG_SECURE.0),
            );
            if !req.is_null() {
                if WinHttpSendRequest(req, None, None, 0, 0, 0).is_ok()
                    && WinHttpReceiveResponse(req, std::ptr::null_mut()).is_ok()
                {
                    let mut body = Vec::new();
                    loop {
                        let mut avail = 0u32;
                        if WinHttpQueryDataAvailable(req, &mut avail).is_err() || avail == 0 {
                            break;
                        }
                        let start = body.len();
                        body.resize(start + avail as usize, 0);
                        let mut read = 0u32;
                        if WinHttpReadData(
                            req,
                            body[start..].as_mut_ptr() as _,
                            avail,
                            &mut read,
                        )
                        .is_err()
                        {
                            break;
                        }
                        body.truncate(start + read as usize);
                        if read == 0 {
                            break;
                        }
                    }
                    if !body.is_empty() {
                        out = Some(body);
                    }
                }
                let _ = WinHttpCloseHandle(req);
            }
            let _ = WinHttpCloseHandle(conn);
        }
        let _ = WinHttpCloseHandle(session);
        out
    }
}

fn fetch_forecast(wc: &WeatherCfg) -> Option<(Current, Vec<HourScore>)> {
    let path = format!(
        "/v1/forecast?latitude={}&longitude={}\
         &current=temperature_2m,relative_humidity_2m,wind_speed_10m,wind_direction_10m,precipitation\
         &hourly=temperature_2m,relative_humidity_2m,wind_speed_10m,wind_direction_10m,precipitation_probability\
         &forecast_days=7&timezone=auto",
        wc.lat, wc.lon
    );
    let body = https_get("api.open-meteo.com", &path)?;
    let root = Value::parse(&String::from_utf8_lossy(&body))?;

    let cur = root.get("current")?;
    let f = |k: &str| cur.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let current = Current {
        temp_c: f("temperature_2m"),
        humidity: f("relative_humidity_2m"),
        wind_kmh: f("wind_speed_10m"),
        wind_dir: f("wind_direction_10m"),
        score: score_hour(
            &wc.bearings,
            f("wind_direction_10m"),
            f("wind_speed_10m"),
            f("temperature_2m"),
            f("relative_humidity_2m"),
            if f("precipitation") > 0.0 { 100.0 } else { 0.0 },
        ),
    };

    let hourly = root.get("hourly")?;
    let col = |k: &str| hourly.get(k).and_then(Value::arr);
    let times = col("time")?;
    let (temps, hums, winds, dirs, precs) = (
        col("temperature_2m")?,
        col("relative_humidity_2m")?,
        col("wind_speed_10m")?,
        col("wind_direction_10m")?,
        col("precipitation_probability")?,
    );
    let g = |a: &[Value], i: usize| a.get(i).and_then(Value::as_f64).unwrap_or(0.0);
    let hours = (0..times.len())
        .filter_map(|i| {
            let time = times[i].as_str()?.to_string();
            let (wind_dir, wind_kmh) = (g(dirs, i), g(winds, i));
            let (temp_c, humidity) = (g(temps, i), g(hums, i));
            Some(HourScore {
                score: score_hour(&wc.bearings, wind_dir, wind_kmh, temp_c, humidity, g(precs, i)),
                time,
                wind_kmh,
                wind_dir,
                temp_c,
                humidity,
            })
        })
        .collect();
    Some((current, hours))
}

/// Compass point ("NE") for a bearing.
pub fn compass(dir: f64) -> &'static str {
    const PTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    PTS[((dir / 22.5).round() as usize) % 16]
}

/// True when this wind direction meaningfully feeds one of the windows.
pub fn feeds_windows(bearings: &[f64], wind_dir: f64) -> bool {
    bearings.iter().any(|b| ang_dist(wind_dir, *b) <= 60.0)
}

/// Background refresher; also drives the airing toast via `on_good_window`.
pub fn spawn(wc: WeatherCfg, on_good_window: impl Fn(&HourScore) + Send + 'static) {
    if let Ok(mut s) = state().lock() {
        s.configured = true;
    }
    std::thread::spawn(move || {
        let mut was_good = false;
        loop {
            if let Some((current, hours)) = fetch_forecast(&wc) {
                let is_good = current.score >= wc.toast_threshold;
                let now_hour = crate::widgets::clock::local_hour();
                if wc.toast
                    && is_good
                    && !was_good
                    && now_hour >= wc.toast_from_hour
                    && now_hour < wc.toast_to_hour
                {
                    let snapshot = HourScore {
                        time: String::new(),
                        score: current.score,
                        wind_kmh: current.wind_kmh,
                        wind_dir: current.wind_dir,
                        temp_c: current.temp_c,
                        humidity: current.humidity,
                    };
                    on_good_window(&snapshot);
                }
                was_good = is_good;
                if let Ok(mut s) = state().lock() {
                    s.current = Some(current);
                    s.hours = hours;
                    s.bearings = wc.bearings.clone();
                    s.threshold = wc.toast_threshold;
                }
            }
            std::thread::sleep(Duration::from_secs(30 * 60));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wind_into_ne_window_scores_high() {
        // 12 km/h from 40 deg (nearly dead-on the 37 deg window), 24 C, 50% RH
        let s = score_hour(&[37.0, 307.0], 40.0, 12.0, 24.0, 50.0, 0.0);
        assert!(s >= 90, "expected >=90, got {s}");
    }

    #[test]
    fn wind_from_wrong_side_scores_low() {
        // Same conditions but wind from due south (172 deg off both windows)
        let s = score_hour(&[37.0, 307.0], 170.0, 12.0, 24.0, 50.0, 0.0);
        assert!(s < 70, "expected <70, got {s}");
    }

    #[test]
    fn rain_kills_it() {
        assert_eq!(score_hour(&[37.0], 40.0, 12.0, 24.0, 50.0, 80.0), 0);
    }

    #[test]
    fn florida_humidity_hurts() {
        let dry = score_hour(&[37.0], 40.0, 12.0, 24.0, 45.0, 0.0);
        let swamp = score_hour(&[37.0], 40.0, 12.0, 24.0, 92.0, 0.0);
        assert!(dry > swamp + 15);
    }

    #[test]
    fn compass_points() {
        assert_eq!(compass(37.0), "NE");
        assert_eq!(compass(307.0), "NW");
        assert_eq!(compass(0.0), "N");
    }
}
