// Copyright (c) 2023-present, Raphael Amorim.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! The status bar: one row along the bottom with battery, wifi and a bell
//! marker on the left and the clock on the right.
//!
//! What it shows is decided in `rio_backend::statusline` (pure, host-tested).
//! This file does the two things that are not: reading the files the text comes
//! from, and drawing. Reading happens on a timer tick, never in `render`, so a
//! slow or missing file cannot stall a frame; `render` only draws what the last
//! tick left behind.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rio_backend::config::status_bar::StatusBar as Config;
use rio_backend::statusline::{self, Item, Level};
use rio_backend::sugarloaf::text::DrawOpts;
use rio_backend::sugarloaf::Sugarloaf;

/// Akuma's battery/AC state. Absent on anything else, which hides the item.
const POWER_PATH: &str = "/proc/power";
/// Akuma's wifi control device; reading it returns the state as `key=value`.
const WIFI_PATH: &str = "/dev/wifi0";
/// How long a bell stays on the bar if no key is pressed.
const BELL_HOLD: Duration = Duration::from_secs(30);
const PAD_X: f32 = 12.0;
const SEPARATOR: &str = "   ";

pub struct StatusBar {
    cfg: Config,
    items: Vec<Item>,
    clock: Option<String>,
    bell_at: Option<Instant>,
}

/// A small file, or `None`. Bounded: these are `/proc` and device nodes, and a
/// runaway read must not grow the bar's memory.
fn read_small(path: &str) -> Option<String> {
    use std::io::Read;
    let mut s = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(8192)
        .read_to_string(&mut s)
        .ok()?;
    Some(s)
}

impl StatusBar {
    pub fn new(cfg: &Config) -> Self {
        let mut bar = StatusBar {
            cfg: cfg.clone(),
            items: Vec::new(),
            clock: None,
            bell_at: None,
        };
        if bar.cfg.enabled {
            bar.refresh();
        }
        bar
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Logical pixels the bar takes from the terminal grid.
    #[inline]
    pub fn reserved_height(&self) -> f32 {
        self.cfg.reserved_height()
    }

    #[inline]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.cfg.interval_secs.max(1))
    }

    /// Re-read every source. Returns whether anything on the bar changed, so
    /// the caller redraws only when there is something new to show.
    pub fn refresh(&mut self) -> bool {
        let mut items = Vec::with_capacity(2);
        if self.cfg.wifi {
            if let Some(i) =
                read_small(WIFI_PATH).and_then(|t| statusline::wifi(&t, self.cfg.ssid))
            {
                items.push(i);
            }
        }
        if self.cfg.battery {
            if let Some(i) = read_small(POWER_PATH).and_then(|t| statusline::battery(&t))
            {
                items.push(i);
            }
        }
        let clock = self.cfg.clock.then(|| {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            statusline::clock(secs, self.cfg.utc_offset_minutes)
        });

        let mut changed = items != self.items || clock != self.clock;
        if self.bell_at.is_some_and(|t| t.elapsed() >= BELL_HOLD) {
            self.bell_at = None;
            changed = true;
        }
        self.items = items;
        self.clock = clock;
        changed
    }

    /// The bell rang. Returns whether the bar needs redrawing.
    pub fn ring(&mut self) -> bool {
        if !self.cfg.enabled || !self.cfg.bell {
            return false;
        }
        let was = self.bell_at.is_some();
        self.bell_at = Some(Instant::now());
        !was
    }

    /// A key was pressed: the bell has been seen.
    pub fn clear_bell(&mut self) -> bool {
        self.bell_at.take().is_some()
    }

    /// `dimensions` is `(window_width, window_height, scale_factor)`; `bg` is
    /// the terminal background, which the bar lifts slightly so it reads as a
    /// separate strip.
    pub fn render(
        &self,
        sugarloaf: &mut Sugarloaf,
        dimensions: (f32, f32, f32),
        bg: [f32; 4],
    ) {
        if !self.cfg.enabled {
            return;
        }
        let (width, height, scale) = dimensions;
        let win_w = width / scale;
        let win_h = height / scale;
        let bar_h = self.cfg.height;
        let y = win_h - bar_h;

        let lift = |c: f32| c + (1.0 - c) * 0.12;
        sugarloaf.rect(
            None,
            0.0,
            y,
            win_w,
            bar_h,
            [lift(bg[0]), lift(bg[1]), lift(bg[2]), 1.0],
            0.0,
            19,
        );

        let size = self.cfg.font_size;
        let text_y = y + (bar_h - size * 1.2) / 2.0;
        let normal = DrawOpts {
            font_size: size,
            color: [205, 205, 205, 255],
            ..DrawOpts::default()
        };
        let warn = DrawOpts {
            font_size: size,
            color: [255, 110, 100, 255],
            ..DrawOpts::default()
        };
        let bell = DrawOpts {
            font_size: size,
            color: [255, 205, 60, 255],
            ..DrawOpts::default()
        };

        let ui = sugarloaf.text_mut();
        let mut x = PAD_X;
        for item in &self.items {
            let opts = if item.level == Level::Warn {
                &warn
            } else {
                &normal
            };
            x += ui.draw(x, text_y, &item.text, opts);
            x += ui.measure(SEPARATOR, &normal);
        }
        if self.bell_at.is_some() {
            ui.draw(x, text_y, "BELL", &bell);
        }
        if let Some(clock) = &self.clock {
            let w = ui.measure(clock, &normal);
            ui.draw(win_w - w - PAD_X, text_y, clock, &normal);
        }
    }
}
