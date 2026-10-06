//! The text the status bar shows, from the files it is read from.
//!
//! Pure: strings in, strings out, so every format is a host test rather than a
//! look at the panel. The I/O and the drawing are in `rioterm`.
//!
//! ASCII only, on purpose. On the Akuma box there is no fontconfig, so a
//! codepoint the configured font lacks renders as tofu; a battery glyph would be
//! a gamble and the word "BAT" is not.

use std::fmt::Write;

/// How loudly an item wants to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Normal,
    /// Low battery, a failed association.
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub level: Level,
}

impl Item {
    fn normal(text: String) -> Self {
        Item {
            text,
            level: Level::Normal,
        }
    }
    fn warn(text: String) -> Self {
        Item {
            text,
            level: Level::Warn,
        }
    }
}

fn kv<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
}

fn hm(minutes: u32) -> String {
    format!("{}h{:02}m", minutes / 60, minutes % 60)
}

/// `/proc/power` -> the battery item. `None` when the machine reports no power
/// source at all (a desktop, a VM), so the bar shows nothing rather than a lie.
pub fn battery(proc_power: &str) -> Option<Item> {
    if kv(proc_power, "source")? == "none" {
        return None;
    }
    let ac = kv(proc_power, "ac") == Some("1");
    if kv(proc_power, "battery") != Some("1") {
        return ac.then(|| Item::normal("AC".into()));
    }
    if kv(proc_power, "valid") != Some("1") {
        return Some(Item::warn("BAT ?".into()));
    }
    let pct: u32 = kv(proc_power, "percent")?.parse().ok()?;
    let minutes = kv(proc_power, "minutes").and_then(|m| m.parse::<u32>().ok());
    let status = kv(proc_power, "status").unwrap_or("Unknown");
    let mut s = format!("BAT {pct}%");
    let mut low = false;
    match status {
        "Discharging" => {
            low = pct <= 15;
            if let Some(m) = minutes {
                let _ = write!(s, " {}", hm(m));
            }
        }
        "Charging" => {
            s.push_str(" chg");
            if let Some(m) = minutes {
                let _ = write!(s, " {}", hm(m));
            }
        }
        "Not charging" => s.push_str(" AC"),
        _ => {}
    }
    Some(if low { Item::warn(s) } else { Item::normal(s) })
}

fn unhex(s: &str) -> Option<String> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect();
    let out = String::from_utf8(bytes?).ok()?;
    // The name goes on screen: nothing that is not printable ASCII.
    out.chars()
        .all(|c| c.is_ascii_graphic() || c == ' ')
        .then_some(out)
}

/// `/dev/wifi0` -> the wifi item. `None` when there is no radio.
pub fn wifi(dev_wifi: &str, show_ssid: bool) -> Option<Item> {
    match kv(dev_wifi, "state")? {
        "no-radio" => None,
        "down" => Some(Item::normal("WIFI off".into())),
        "scanning" | "associating" => Some(Item::normal("WIFI ...".into())),
        "failed" => Some(Item::warn(format!(
            "WIFI {}",
            kv(dev_wifi, "error")
                .filter(|e| *e != "none")
                .unwrap_or("failed")
        ))),
        "connected" => {
            let mut s = String::from("WIFI");
            if show_ssid {
                if let Some(name) = kv(dev_wifi, "ssid")
                    .and_then(unhex)
                    .filter(|n| !n.is_empty())
                {
                    let _ = write!(s, " {name}");
                }
            }
            if let Some(sig) = kv(dev_wifi, "signal").and_then(|v| v.parse::<i32>().ok())
            {
                let _ = write!(s, " {sig}dBm");
            }
            Some(Item::normal(s))
        }
        _ => None,
    }
}

/// Days since 1970-01-01 -> (year, month 1..=12, day 1..=31). Hinnant's
/// `civil_from_days`.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `Tue 07 Oct 22:54` for a Unix time and an offset east of UTC in minutes.
pub fn clock(unix_secs: u64, utc_offset_minutes: i32) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
        "Dec",
    ];
    let t = unix_secs as i64 + i64::from(utc_offset_minutes) * 60;
    let days = t.div_euclid(86_400);
    let sod = t.rem_euclid(86_400);
    let (_, m, d) = civil(days);
    // 1970-01-01 was a Thursday.
    let wd = (days + 4).rem_euclid(7) as usize;
    format!(
        "{} {:02} {} {:02}:{:02}",
        DAYS[wd],
        d,
        MONTHS[(m - 1) as usize],
        sod / 3600,
        (sod % 3600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISCHARGING: &str = "source=ec\nac=0\nbattery=1\nvalid=1\nstatus=Discharging\npercent=97\n\
        voltage_mv=13028\ncurrent_ma=536\npower_mw=6983\nminutes=454\nbtst=1\nec_raw=0600\n";

    #[test]
    fn battery_discharging() {
        let i = battery(DISCHARGING).unwrap();
        assert_eq!(i.text, "BAT 97% 7h34m");
        assert_eq!(i.level, Level::Normal);
    }

    #[test]
    fn battery_low_warns() {
        let t = DISCHARGING.replace("percent=97", "percent=9");
        assert_eq!(battery(&t).unwrap().level, Level::Warn);
        // low while charging is fine
        let t = t.replace("Discharging", "Charging");
        assert_eq!(battery(&t).unwrap().level, Level::Normal);
    }

    #[test]
    fn battery_charging_and_ac() {
        let t = DISCHARGING
            .replace("Discharging", "Charging")
            .replace("ac=0", "ac=1");
        assert_eq!(battery(&t).unwrap().text, "BAT 97% chg 7h34m");
        let t = DISCHARGING
            .replace("Discharging", "Not charging")
            .replace("ac=0", "ac=1");
        assert_eq!(battery(&t).unwrap().text, "BAT 97% AC");
    }

    #[test]
    fn battery_absent_or_invalid() {
        assert_eq!(battery("source=none\nac=unknown\nbattery=0\n"), None);
        assert_eq!(battery("source=ec\nac=1\nbattery=0\n").unwrap().text, "AC");
        assert_eq!(battery("source=ec\nac=0\nbattery=0\n"), None);
        let bad = "source=ec\nac=0\nbattery=1\nvalid=0\nbtst=0\n";
        let i = battery(bad).unwrap();
        assert_eq!((i.text.as_str(), i.level), ("BAT ?", Level::Warn));
        assert_eq!(battery(""), None);
        assert_eq!(battery("garbage"), None);
    }

    #[test]
    fn battery_without_minutes() {
        let t = DISCHARGING.replace("minutes=454\n", "");
        assert_eq!(battery(&t).unwrap().text, "BAT 97%");
    }

    const WIFI: &str = "iface=wlan0\nradio=rtw89\nstate=connected\nssid=616b756d61\n\
        bssid=02:00:00:00:00:01\nchan=1\nsignal=-55\nsecurity=wpa2\nerror=none\nscans=1\n";

    #[test]
    fn wifi_connected_hides_ssid_by_default() {
        assert_eq!(wifi(WIFI, false).unwrap().text, "WIFI -55dBm");
        assert_eq!(wifi(WIFI, true).unwrap().text, "WIFI akuma -55dBm");
    }

    #[test]
    fn wifi_never_shows_unprintable_names_or_bssid() {
        let t = WIFI.replace("616b756d61", "1b5b324a");
        assert_eq!(wifi(&t, true).unwrap().text, "WIFI -55dBm");
        assert!(!wifi(WIFI, true).unwrap().text.contains("02:00"));
    }

    #[test]
    fn wifi_states() {
        let s = |st: &str| WIFI.replace("state=connected", &format!("state={st}"));
        assert_eq!(wifi(&s("no-radio"), false), None);
        assert_eq!(wifi(&s("down"), false).unwrap().text, "WIFI off");
        assert_eq!(wifi(&s("scanning"), false).unwrap().text, "WIFI ...");
        let f = s("failed").replace("error=none", "error=auth-failed");
        let i = wifi(&f, false).unwrap();
        assert_eq!(
            (i.text.as_str(), i.level),
            ("WIFI auth-failed", Level::Warn)
        );
        assert_eq!(wifi("", false), None);
    }

    #[test]
    fn clock_known_instants() {
        // 2026-10-07 00:00:00 UTC was a Wednesday.
        assert_eq!(clock(1_791_331_200, 0), "Wed 07 Oct 00:00");
        assert_eq!(
            clock(1_791_331_200 + 22 * 3600 + 54 * 60, 0),
            "Wed 07 Oct 22:54"
        );
        // +03:00 pushes it over midnight into Thursday.
        assert_eq!(
            clock(1_791_331_200 + 22 * 3600 + 54 * 60, 180),
            "Thu 08 Oct 01:54"
        );
        assert_eq!(clock(0, 0), "Thu 01 Jan 00:00");
        // leap day
        assert_eq!(clock(951_782_400, 0), "Tue 29 Feb 00:00");
        // before the epoch with a negative offset must not panic
        assert_eq!(clock(0, -60), "Wed 31 Dec 23:00");
    }
}
