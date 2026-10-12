// backlight.rs -- a wall panel's screen rests when nobody uses it (issue
// #107, heat and power): dimmed after DIM_AFTER without a touch, off after
// OFF_AFTER. The next touch brings it back -- and while the screen is OFF
// that touch only wakes it: it doesn't also press whatever is under the
// finger (nobody could see what they were pressing).
//
// The display's backlight is a sysfs device (/sys/class/backlight/<name>:
// the official 7" Raspberry Pi display's, for instance): `brightness`
// (0..max_brightness) and `bl_power` (0 = on, 4 = off). The UI's user may
// write them through group "video" (panel-tuning's udev rule) and its
// sandbox allows exactly that (the panel's drop-in of ui-layer.service).
//
// Only on a wall panel (main.rs): the hub's own screen is unchanged.

use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// No touch for this long: dimmed.
const DIM_AFTER: Duration = Duration::from_secs(60);
/// ...and for this long: off.
const OFF_AFTER: Duration = Duration::from_secs(5 * 60);
/// Dimmed = this share of the full brightness.
const DIM_SHARE: f32 = 0.25;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Level {
    Full,
    Dim,
    Off,
}

/// What the screen should be after `idle` without a touch.
pub fn level_for(idle: Duration) -> Level {
    if idle >= OFF_AFTER {
        Level::Off
    } else if idle >= DIM_AFTER {
        Level::Dim
    } else {
        Level::Full
    }
}

pub struct Backlight {
    /// The device's folder; None: this screen has no backlight control
    /// (nothing to do then, everything stays as it is).
    dir: Option<PathBuf>,
    /// The device's highest brightness: what "full" is. (Not the value
    /// found at start: after a restart that may be a dimmed one.)
    max: u32,
    level: Cell<Level>,
    last_touch: Cell<Instant>,
    /// The screen was off when the current touch began: the rest of that
    /// touch (until the finger lifts) is swallowed too.
    waking: Cell<bool>,
}

impl Backlight {
    /// Finds the first backlight device.
    pub fn new() -> Self {
        let dir = std::fs::read_dir("/sys/class/backlight").ok().and_then(|mut entries| entries.next()).and_then(|e| e.ok()).map(|e| e.path());
        let read = |file: &str| -> Option<u32> {
            std::fs::read_to_string(dir.as_ref()?.join(file)).ok()?.trim().parse().ok()
        };
        let max = read("max_brightness").unwrap_or(0);
        match &dir {
            Some(d) => println!("ui_layer: backlight {} (max {max}): dims after {} s, off after {} s", d.display(), DIM_AFTER.as_secs(), OFF_AFTER.as_secs()),
            None => println!("ui_layer: no backlight control on this screen"),
        }
        let backlight = Backlight { dir, max, level: Cell::new(Level::Full), last_touch: Cell::new(Instant::now()), waking: Cell::new(false) };
        // On at full, whatever a previous run left: a UI restarted while
        // the screen was off would otherwise stay dark (seen on the Pi).
        backlight.set(Level::Full);
        backlight
    }

    /// Every input event (main.rs's libinput hook). Returns true when
    /// Slint must IGNORE it: a touch that only wakes the screen.
    pub fn on_input(&self, event: &input::Event) -> bool {
        use input::event::TouchEvent;
        self.last_touch.set(Instant::now());
        // Dimmed or off: any touch brings it back to full.
        let was_off = self.level.get() == Level::Off;
        if self.level.get() != Level::Full {
            self.set(Level::Full);
        }
        match event {
            input::Event::Touch(TouchEvent::Down(_)) if was_off => {
                self.waking.set(true);
                true
            }
            input::Event::Touch(TouchEvent::Up(_)) if self.waking.get() => {
                self.waking.set(false);
                true
            }
            input::Event::Touch(_) => self.waking.get(),
            _ => false,
        }
    }

    /// Called every second or so (main.rs's timer).
    pub fn tick(&self) {
        let wanted = level_for(self.last_touch.get().elapsed());
        if wanted != self.level.get() {
            self.set(wanted);
        }
    }

    fn set(&self, level: Level) {
        self.level.set(level);
        let Some(dir) = &self.dir else { return };
        let write = |file: &str, value: String| {
            if let Err(e) = std::fs::write(dir.join(file), value) {
                println!("ui_layer: backlight {file}: {e}");
            }
        };
        match level {
            Level::Off => write("bl_power", "4".into()),
            Level::Dim | Level::Full => {
                let share = if level == Level::Dim { DIM_SHARE } else { 1.0 };
                let value = ((self.max as f32 * share).round() as u32).clamp(1, self.max.max(1));
                write("brightness", value.to_string());
                write("bl_power", "0".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_screen_rests_in_steps() {
        assert_eq!(level_for(Duration::from_secs(5)), Level::Full);
        assert_eq!(level_for(DIM_AFTER), Level::Dim);
        assert_eq!(level_for(OFF_AFTER - Duration::from_secs(1)), Level::Dim);
        assert_eq!(level_for(OFF_AFTER), Level::Off);
    }
}
