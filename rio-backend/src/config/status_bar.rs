use crate::config::defaults::default_bool_true;
use serde::{Deserialize, Serialize};

/// A one-row bar along the bottom of the window: clock, battery, wifi and a
/// bell indicator. Off by default; the Akuma panel config turns it on.
///
/// The sources are files, not APIs, so the bar works wherever they exist and
/// quietly omits an item where they do not: `/proc/power` (battery, Akuma),
/// `/dev/wifi0` (wifi, Akuma), and the system clock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusBar {
    #[serde(default = "bool::default")]
    pub enabled: bool,
    /// Height of the bar in logical pixels. The terminal grid loses this much.
    #[serde(default = "default_height")]
    pub height: f32,
    #[serde(default = "default_font_size", rename = "font-size")]
    pub font_size: f32,
    /// Seconds between refreshes of the battery/wifi/clock text.
    #[serde(default = "default_interval", rename = "interval-secs")]
    pub interval_secs: u64,
    #[serde(default = "default_bool_true")]
    pub clock: bool,
    #[serde(default = "default_bool_true")]
    pub battery: bool,
    #[serde(default = "default_bool_true")]
    pub wifi: bool,
    /// Show a marker after the bell rings, until the next keypress.
    #[serde(default = "default_bool_true")]
    pub bell: bool,
    /// Show the connected network's name next to the signal strength. Off by
    /// default: the name is the user's, and a screenshot shares it.
    #[serde(default = "bool::default")]
    pub ssid: bool,
    /// Minutes east of UTC. There is no timezone database on the Akuma box, so
    /// the clock is UTC plus this.
    #[serde(default, rename = "utc-offset-minutes")]
    pub utc_offset_minutes: i32,
}

impl Default for StatusBar {
    fn default() -> Self {
        StatusBar {
            enabled: false,
            height: default_height(),
            font_size: default_font_size(),
            interval_secs: default_interval(),
            clock: true,
            battery: true,
            wifi: true,
            bell: true,
            ssid: false,
            utc_offset_minutes: 0,
        }
    }
}

impl StatusBar {
    /// Logical pixels the bar takes from the grid: 0 when it is off.
    #[inline]
    pub fn reserved_height(&self) -> f32 {
        if self.enabled {
            self.height.max(0.0)
        } else {
            0.0
        }
    }
}

fn default_height() -> f32 {
    26.0
}

fn default_font_size() -> f32 {
    15.0
}

fn default_interval() -> u64 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_and_reserves_nothing() {
        let c = StatusBar::default();
        assert!(!c.enabled);
        assert_eq!(c.reserved_height(), 0.0);
    }

    #[test]
    fn parses_kebab_case_keys() {
        let c: StatusBar = toml::from_str(
            "enabled = true\nheight = 30.0\ninterval-secs = 5\nssid = true\nutc-offset-minutes = 180\nbattery = false\n",
        )
        .unwrap();
        assert!(c.enabled && c.ssid && !c.battery && c.clock && c.wifi && c.bell);
        assert_eq!(
            (c.height, c.interval_secs, c.utc_offset_minutes),
            (30.0, 5, 180)
        );
        assert_eq!(c.reserved_height(), 30.0);
    }

    #[test]
    fn a_negative_height_reserves_nothing() {
        let c = StatusBar {
            enabled: true,
            height: -4.0,
            ..StatusBar::default()
        };
        assert_eq!(c.reserved_height(), 0.0);
    }
}
