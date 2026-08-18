//! Indoor air quality from an AirGradient monitor.
//!
//! Two ways in, because the fleet isn't uniform:
//!
//! * **Local** — the monitor's own API (`GET /measures/current`, plain HTTP,
//!   firmware 3.0.10+ on the ONE / Open Air). Nothing leaves the LAN, and
//!   the reading is live.
//! * **Cloud** — `api.airgradient.com` with the account's API token. The
//!   older DIY boards have no local server at all (they ping, but nothing
//!   listens on :80), so this is the only route for them. Data is as fresh
//!   as the device's last upload, which is why readings carry their own
//!   timestamp rather than borrowing our fetch time.
//!
//! Either way the reading lands in a process-wide state the stats dropdown
//! reads, where it sits next to the outdoor numbers so "should I open the
//! windows" has both halves of the answer.
//!
//! Local API: <https://github.com/airgradienthq/arduino/blob/master/docs/local-server.md>

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::BarConfig;
use crate::json::Value;

/// One reading. Every field is optional because the fleet isn't uniform —
/// an Open Air outdoor unit has no CO2 sensor, older firmware has no NOx,
/// and the cloud API returns fewer fields than the device itself.
#[derive(Clone, Default)]
pub struct Reading {
    pub pm02: Option<f64>,
    pub co2: Option<f64>,
    pub tvoc: Option<f64>,
    pub nox: Option<f64>,
    pub temp_c: Option<f64>,
    pub humidity: Option<f64>,
    /// When the *device* took the reading, epoch seconds. Only the cloud
    /// API reports this; a local reading is by definition current.
    pub obs_epoch: Option<i64>,
}

#[derive(Default)]
pub struct AirState {
    /// False until [air] is configured; the popup then skips the whole
    /// indoor section rather than advertising hardware you may not own.
    pub configured: bool,
    pub reading: Option<Reading>,
    /// When *we* last fetched, for the freshness stamp when the reading
    /// carries no timestamp of its own.
    pub updated: Option<Instant>,
    /// Set when the last poll failed. The previous reading stays visible,
    /// stamped with its real age, instead of blanking the section.
    pub stale: bool,
}

static STATE: OnceLock<Mutex<AirState>> = OnceLock::new();

pub fn state() -> &'static Mutex<AirState> {
    STATE.get_or_init(|| Mutex::new(AirState::default()))
}

pub enum Source {
    /// The device's own web server on the LAN.
    Local { host: String, port: u16 },
    /// AirGradient's API. `location` picks one monitor out of a place with
    /// several, by locationId or name; empty takes the first.
    Cloud { token: String, location: String },
}

pub struct AirCfg {
    pub source: Source,
    pub interval: Duration,
    /// Prefer the firmware's compensated temp/humidity, which correct for
    /// the heat the board itself gives off.
    pub compensated: bool,
}

/// None unless [air] names a host or a token. Neither ships as a default:
/// one is local network detail, the other is a credential.
pub fn read_cfg(cfg: &BarConfig) -> Option<AirCfg> {
    let s = "air";
    let get = |k: &str| cfg.values.get(s, k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let source = match (get("token"), get("host")) {
        // A token wins when both are set: it works from any network, and a
        // stale `host` line left behind shouldn't silently win.
        (Some(token), _) => Source::Cloud {
            token,
            location: get("location").unwrap_or_default(),
        },
        (None, Some(host)) => Source::Local {
            host,
            port: cfg.values.get_u64(s, "port", 80) as u16,
        },
        (None, None) => return None,
    };
    Some(AirCfg {
        source,
        interval: Duration::from_secs(cfg.values.get_u64(s, "interval", 60).max(5)),
        compensated: cfg.values.get_or(s, "compensated", "on") != "off",
    })
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// "2026-08-16T19:42:07.000Z" -> epoch seconds. The API stamps in UTC, and
/// we only ever subtract two of these, so no timezone handling is needed.
fn epoch_utc(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (hh, mm): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    // Seconds may carry fractions or a trailing Z; take the leading digits.
    let ss: i64 = t
        .next()
        .map(|s| s.chars().take_while(char::is_ascii_digit).collect::<String>())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Some(days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Current UTC as epoch seconds, from the same clock the stamps are in.
pub fn now_epoch_utc() -> i64 {
    let t = unsafe { windows::Win32::System::SystemInformation::GetSystemTime() };
    days_from_civil(t.wYear as i64, t.wMonth as i64, t.wDay as i64) * 86_400
        + t.wHour as i64 * 3600
        + t.wMinute as i64 * 60
        + t.wSecond as i64
}

/// One measurement object, local or cloud. The two payloads overlap on the
/// names that matter; where they don't, both spellings are accepted rather
/// than branching on which endpoint we came from.
fn reading_from(obj: &Value, compensated: bool) -> Reading {
    let f = |k: &str| obj.get(k).and_then(Value::as_f64);
    let any = |keys: &[&str]| keys.iter().find_map(|k| f(k));
    // The corrected reading is spelled two different ways depending on who
    // is answering: `atmpCompensated` from the device, `atmp_corrected`
    // from the cloud. Try both, then fall back to the raw sensor value —
    // older firmware corrects nothing.
    let pick = |raw: &str| {
        if compensated {
            any(&[
                &format!("{raw}Compensated"),
                &format!("{raw}_corrected"),
                raw,
            ])
        } else {
            f(raw)
        }
    };
    Reading {
        pm02: pick("pm02"),
        co2: pick("rco2"),
        // Both spellings appear in the same cloud payload and mean
        // different things: `tvocIndex` is Sensirion's 1-500 index (100 =
        // your baseline), `tvoc` is a raw concentration. The index is the
        // one worth showing, so it wins.
        tvoc: any(&["tvocIndex", "tvoc"]),
        nox: any(&["noxIndex", "nox"]),
        temp_c: pick("atmp"),
        humidity: pick("rhum"),
        obs_epoch: obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(epoch_utc),
    }
}

/// Local payload: a single object.
fn parse_local(body: &str, compensated: bool) -> Option<Reading> {
    let root = Value::parse(body)?;
    root.get("serialno")?; // cheap sanity check that this is a monitor
    Some(reading_from(&root, compensated))
}

/// Cloud payload: an array of locations, one per monitor on the account.
fn parse_cloud(body: &str, location: &str, compensated: bool) -> Option<Reading> {
    let root = Value::parse(body)?;
    let items = root.arr()?;
    let matches = |v: &Value| {
        if location.is_empty() {
            return true;
        }
        let by_name = v.get("locationName").and_then(Value::as_str) == Some(location);
        let by_id = v
            .get("locationId")
            .and_then(Value::as_f64)
            .map(|id| format!("{id:.0}") == location)
            .unwrap_or(false);
        by_name || by_id
    };
    items
        .iter()
        .find(|v| matches(v))
        .map(|v| reading_from(v, compensated))
}

fn poll(ac: &AirCfg) -> Option<Reading> {
    match &ac.source {
        Source::Local { host, port } => {
            let body = crate::http::get(host, *port, "/measures/current", false)?;
            parse_local(&String::from_utf8_lossy(&body), ac.compensated)
        }
        Source::Cloud { token, location } => {
            let path = format!("/public/api/v1/locations/measures/current?token={token}");
            let body = crate::http::get("api.airgradient.com", 443, &path, true)?;
            parse_cloud(&String::from_utf8_lossy(&body), location, ac.compensated)
        }
    }
}

/// Background poller. Failures leave the last reading in place and flip
/// `stale`, so a monitor that drops off wifi degrades to an aging number
/// rather than an empty panel.
pub fn spawn(ac: AirCfg) {
    if let Ok(mut s) = state().lock() {
        s.configured = true;
    }
    std::thread::spawn(move || loop {
        let reading = poll(&ac);
        if let Ok(mut s) = state().lock() {
            match reading {
                Some(r) => {
                    s.reading = Some(r);
                    s.updated = Some(Instant::now());
                    s.stale = false;
                }
                None => s.stale = true,
            }
        }
        std::thread::sleep(ac.interval);
    });
}

/// 0-100 "how good is this", so indoor numbers can borrow the same
/// red-yellow-green ramp the airing score uses. `good` scores 100, `bad`
/// scores 0, linear between.
pub fn goodness(v: f64, good: f64, bad: f64) -> u32 {
    (((bad - v) / (bad - good)).clamp(0.0, 1.0) * 100.0).round() as u32
}

/// PM2.5 ug/m3 -> goodness. Anchored on the WHO 24 h guideline (15) and the
/// US AQI "unhealthy for sensitive groups" threshold (35).
pub fn pm_goodness(pm: f64) -> u32 {
    goodness(pm, 5.0, 35.0)
}

/// CO2 ppm -> goodness. ~450 is outdoor air; 1000 is the classic stuffy
/// -room / ventilate-now line; 1500 is where people report headaches.
pub fn co2_goodness(ppm: f64) -> u32 {
    goodness(ppm, 600.0, 1500.0)
}

/// Sensirion's VOC index is centred on 100 = your own recent baseline;
/// above ~250 something is off-gassing.
pub fn voc_goodness(idx: f64) -> u32 {
    goodness(idx, 100.0, 300.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = r#"{"wifi":-46,"serialno":"ecda3b1eaaaf","rco2":447,"pm01":3,
        "pm02":7,"pm10":8,"pm003Count":442,"atmp":25.87,"atmpCompensated":24.47,
        "rhum":43,"rhumCompensated":49,"tvocIndex":100,"tvocRaw":33051,"noxIndex":1,
        "noxRaw":16307,"boot":6,"ledMode":"pm","firmware":"3.1.3","model":"I-9PSL"}"#;

    /// The real shape of a v1 cloud response (values neutralised): note
    /// `_corrected` rather than the device's `Compensated`, and `tvoc`
    /// alongside `tvocIndex` meaning something else entirely.
    const CLOUD: &str = r#"[{"locationId":10001,"locationName":"Living Room",
        "latitude":null,"longitude":null,"pm01":0.0,"pm02":6.0,"pm10":7.0,
        "pm01_corrected":0.0,"pm02_corrected":4.0,"pm10_corrected":5.0,
        "pm003Count":327,"atmp":23.5,"rhum":52,"rco2":812,"atmp_corrected":22.1,
        "rhum_corrected":55,"rco2_corrected":800,"wifi":-38,
        "timestamp":"2026-08-16T19:42:07.000Z","serialno":"aaaaaa","model":null,
        "firmwareVersion":null,"tvoc":25.3,"tvocIndex":140,"noxIndex":1,
        "locationType":"indoor","batteryVoltage":null,"panelVoltage":null},
        {"locationId":10002,"locationName":"Garage","pm02":31,"rco2":600,
        "timestamp":"2026-08-16T19:40:00.000Z"}]"#;

    #[test]
    fn parses_the_documented_local_payload() {
        let r = parse_local(LOCAL, true).expect("parse");
        assert_eq!(r.pm02, Some(7.0));
        assert_eq!(r.co2, Some(447.0));
        assert_eq!(r.tvoc, Some(100.0));
        assert_eq!(r.humidity, Some(49.0));
        assert_eq!(r.obs_epoch, None); // local readings are live
    }

    #[test]
    fn compensated_wins_when_asked_and_falls_back_when_absent() {
        assert_eq!(parse_local(LOCAL, true).unwrap().temp_c, Some(24.47));
        assert_eq!(parse_local(LOCAL, false).unwrap().temp_c, Some(25.87));
        // pm02Compensated isn't in the payload; compensated mode still reads pm02.
        assert_eq!(parse_local(LOCAL, true).unwrap().pm02, Some(7.0));
    }

    #[test]
    fn cloud_takes_the_first_location_or_the_named_one() {
        assert_eq!(parse_cloud(CLOUD, "", true).unwrap().co2, Some(800.0));
        assert_eq!(parse_cloud(CLOUD, "Garage", true).unwrap().pm02, Some(31.0));
        assert_eq!(parse_cloud(CLOUD, "10002", true).unwrap().pm02, Some(31.0));
        assert!(parse_cloud(CLOUD, "Attic", true).is_none());
    }

    #[test]
    fn cloud_corrections_and_the_two_meanings_of_tvoc() {
        let on = parse_cloud(CLOUD, "", true).unwrap();
        assert_eq!(on.pm02, Some(4.0), "_corrected is the cloud's spelling");
        assert_eq!(on.temp_c, Some(22.1));
        assert_eq!(on.humidity, Some(55.0));
        let off = parse_cloud(CLOUD, "", false).unwrap();
        assert_eq!(off.pm02, Some(6.0));
        assert_eq!(off.temp_c, Some(23.5));
        // tvoc (25.3, a concentration) must never win over tvocIndex.
        assert_eq!(on.tvoc, Some(140.0));
        assert_eq!(off.tvoc, Some(140.0));
    }

    #[test]
    fn cloud_readings_carry_their_own_age() {
        let r = parse_cloud(CLOUD, "", true).unwrap();
        assert_eq!(r.obs_epoch, Some(1_786_909_327)); // 2026-08-16T19:42:07Z
    }

    #[test]
    fn utc_stamps_convert_and_survive_junk() {
        assert_eq!(epoch_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_utc("2000-03-01T00:00:00Z"), Some(951868800));
        // No seconds field, and a stamp that isn't one at all.
        assert_eq!(epoch_utc("2026-08-16T19:42"), Some(1_786_909_320));
        assert_eq!(epoch_utc("not a time"), None);
    }

    #[test]
    fn goodness_ramps_and_clamps() {
        assert_eq!(pm_goodness(0.0), 100);
        assert_eq!(pm_goodness(35.0), 0);
        assert_eq!(pm_goodness(200.0), 0);
        assert!(co2_goodness(900.0) > co2_goodness(1400.0));
    }
}
