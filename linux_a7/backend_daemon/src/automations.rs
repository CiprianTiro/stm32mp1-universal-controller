/*
 * automations.rs -- scenes and automations (issue #47): what turns a
 * remote control into a hub. Everything runs ON THE HUB: no internet
 * needed (only the clock has to be right, see TIME).
 *
 * A SCENE is a named list of steps, run on request ("Movie": TV on, hall
 * light off, blind closed) -- from the touchscreen, the app, the cloud,
 * or an automation.
 *
 * An AUTOMATION is: WHEN one of its triggers happens, IF all its
 * conditions hold, DO its steps. Triggers:
 *   time       {"type": "time", "at": "07:30", "days": ["mon", "tue"]}
 *              (no days = every day)
 *   sun        {"type": "sun", "event": "sunset", "offset_min": -15, "days": [...]}
 *   state      {"type": "state", "device": "tv", "capability": "switch",
 *               "field": "on", "to": true, "from"?: false}
 *              a value of a device changes (to / from optional: any change)
 *   threshold  {"type": "threshold", "device": "plug", "capability": "energy",
 *               "field": "power_w", "above": 5}       (or "below")
 *              a number CROSSES the limit (not: is above it -- once per
 *              crossing, not on every report while it stays there)
 * Conditions (all must hold):
 *   time       {"type": "time", "after": "18:00", "before": "23:00", "days"?: [...]}
 *              (across midnight too: after 22:00 before 06:00)
 *   sun        {"type": "sun", "is": "day" | "night"}
 *   state      {"type": "state", "device", "capability", "field", "is": <value>}
 *   value      {"type": "value", "device", "capability", "field", "above"?, "below"?}
 * Steps (scenes and automations):
 *   {"device": "hall", "capability": "switch", "value": {"on": true}}   a command
 *   {"device": "blind", "capability": "cover", "action": "close", "args"?: {}}
 *   {"scene": "movie"}                                       automations only
 * A field is a path into the capability's state: "on", "power_w",
 * "readings.temperature.value", "position".
 *
 * Steps go through control.rs like a tap on the screen: checked the same
 * way, and an UNLOCK must carry "confirmed": true (#77's unlock rule) --
 * the person wrote it into the automation, and the screens warn about it.
 *
 * LOOP PROTECTION -- an automation must never keep itself (or a ring of
 * them) running:
 *   - a device an automation just changed doesn't trigger THAT automation
 *     again for CAUSED_FOR;
 *   - a change caused by an automation that triggers another one counts as
 *     one more link in a chain; a chain longer than MAX_CHAIN stops;
 *   - an automation running more than MAX_RUNS_PER_MINUTE times in a minute
 *     is switched off, with a log entry saying why.
 *
 * TIME: time and sun triggers are checked once a minute, in the hub's time
 * zone (settings.rs) -- and only while the clock is NTP-synced: the DK2
 * has no battery-backed clock, so after a power cut without internet it
 * starts with a wrong date, and "07:30" would happen at a random moment.
 * Sun times need the hub's location (latitude/longitude in the settings).
 *
 * SAVED in automations.json (store.rs: checksum, previous copy) on every
 * change. The LOG of what ran is only kept in memory (the last LOG_SIZE
 * entries): it would cost a flash write per run, and is for "what
 * happened today", not history.
 */
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Offset, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{broadcast, watch};

use crate::control::Control;
use crate::device::{self, Device, Origin};
use crate::settings::{HubSettings, Settings};
use crate::state::{DeviceId, Event};

pub const AUTOMATIONS_SCHEMA: u32 = 1;
/* Log entries kept. */
const LOG_SIZE: usize = 200;
/* Loop protection (see the header). */
const CAUSED_FOR: Duration = Duration::from_secs(5);
const MAX_CHAIN: u32 = 5;
const MAX_RUNS_PER_MINUTE: usize = 10;
/* Sizes, so a mistake (or a hostile client) can't make huge ones. */
const MAX_ITEMS: usize = 100;
const MAX_STEPS: usize = 50;
const MAX_TRIGGERS: usize = 10;

/* ------------------------------------------------------------------ */
/* The model                                                           */
/* ------------------------------------------------------------------ */

/* Everything saved: the scenes and the automations. */
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Book {
    #[serde(default)]
    pub scenes: Vec<Scene>,
    #[serde(default)]
    pub automations: Vec<Automation>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scene {
    /* Empty when saving a new one: made from the name. */
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub steps: Vec<Step>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Automation {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub triggers: Vec<Trigger>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    pub steps: Vec<Step>,
}

fn yes() -> bool {
    true
}

/* One step: a command, an action, or (automations only) a scene. Kept as
 * one struct with optional parts (rather than an enum) so the JSON stays
 * the plain shapes in the header; check_step says which combinations are
 * valid. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    fn of(day: chrono::Weekday) -> Weekday {
        match day {
            chrono::Weekday::Mon => Weekday::Mon,
            chrono::Weekday::Tue => Weekday::Tue,
            chrono::Weekday::Wed => Weekday::Wed,
            chrono::Weekday::Thu => Weekday::Thu,
            chrono::Weekday::Fri => Weekday::Fri,
            chrono::Weekday::Sat => Weekday::Sat,
            chrono::Weekday::Sun => Weekday::Sun,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SunEvent {
    Sunrise,
    Sunset,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DayOrNight {
    Day,
    Night,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    Time {
        at: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        days: Vec<Weekday>,
    },
    Sun {
        event: SunEvent,
        #[serde(default)]
        offset_min: i32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        days: Vec<Weekday>,
    },
    State {
        device: DeviceId,
        capability: String,
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Value>,
    },
    Threshold {
        device: DeviceId,
        capability: String,
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        above: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        below: Option<f64>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    Time {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        days: Vec<Weekday>,
    },
    Sun {
        is: DayOrNight,
    },
    State {
        device: DeviceId,
        capability: String,
        field: String,
        is: Value,
    },
    Value {
        device: DeviceId,
        capability: String,
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        above: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        below: Option<f64>,
    },
}

/* One line of the log. */
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct LogEntry {
    /* Unix time, seconds (clients show it in the hub's time zone). */
    pub at: u64,
    /* "scene" or "automation". */
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    /* Why it ran: "07:30", "sunset", "Living room TV: switch.on is true",
     * "touchscreen", "cloud". */
    pub cause: String,
    pub ok: bool,
    /* What went wrong (a step that failed), or why it was stopped. */
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/* ------------------------------------------------------------------ */
/* Checking (when saving)                                              */
/* ------------------------------------------------------------------ */

/* 1-40 of a-z, 0-9, "-": like device template ids. */
fn valid_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/* "Movie night!" -> "movie-night"; unique among `taken`. */
fn id_from_name(name: &str, taken: &[&str]) -> String {
    let mut base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    base.truncate(32);
    let base = base.trim_end_matches('-').to_string();
    let base = if base.is_empty() { "item".to_string() } else { base };
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id.as_str()) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

fn check_name(name: &str) -> Result<(), String> {
    let chars = name.chars().count();
    if !(1..=64).contains(&chars) || name.chars().any(char::is_control) || name.trim() != name {
        return Err("a name must be 1-64 characters, no spaces around it".into());
    }
    Ok(())
}

/* "07:30" -> 07:30. */
fn parse_hhmm(text: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(text, "%H:%M").map_err(|_| format!("time {text:?}: write it as HH:MM, e.g. 07:30"))
}

fn check_days(days: &[Weekday]) -> Result<(), String> {
    for (i, day) in days.iter().enumerate() {
        if days[..i].contains(day) {
            return Err(format!("{day:?} is listed twice"));
        }
    }
    Ok(())
}

/* What every check needs: the devices as they are now, the scenes, and
 * whether the hub knows where it is (for the sun). */
struct CheckContext<'a> {
    devices: &'a HashMap<DeviceId, Device>,
    scenes: &'a [Scene],
    has_location: bool,
}

impl CheckContext<'_> {
    fn device(&self, id: &str, capability: &str) -> Result<&Device, String> {
        let device = self.devices.get(id).ok_or_else(|| format!("there's no device {id:?}"))?;
        let caps = serde_json::to_value(&device.capabilities).unwrap_or_default();
        if caps.get(capability).is_none() {
            return Err(format!("{} has no capability {capability:?}", device.name));
        }
        Ok(device)
    }

    fn field(&self, id: &str, capability: &str, field: &str) -> Result<(), String> {
        self.device(id, capability)?;
        if field.is_empty() || field.len() > 64 || field.split('.').any(str::is_empty) {
            return Err(format!("field {field:?}: a path like \"on\" or \"readings.temperature.value\""));
        }
        Ok(())
    }

    fn sun(&self) -> Result<(), String> {
        if self.has_location {
            Ok(())
        } else {
            Err("sunrise and sunset need the hub's location: set it in Settings first".into())
        }
    }

    /* A step: exactly one form, and it must be accepted right now (the
     * same check a tap gets -- so an unlock without "confirmed" can't
     * even be saved). `in_scene`: scenes can't run scenes. */
    fn step(&self, step: &Step, in_scene: bool) -> Result<(), String> {
        match step {
            Step { scene: Some(scene), device: None, capability: None, value: None, action: None, args: None } => {
                if in_scene {
                    return Err("a scene can't run another scene".into());
                }
                if !self.scenes.iter().any(|s| &s.id == scene) {
                    return Err(format!("there's no scene {scene:?}"));
                }
                Ok(())
            }
            Step { device: Some(id), capability: Some(cap), value: Some(value), action: None, args: None, scene: None } => {
                let device = self.device(id, cap)?;
                device::set_capability(device, cap, value.clone(), Origin::Client).map(|_| ())
            }
            Step { device: Some(id), capability: Some(cap), action: Some(action), value: None, scene: None, args } => {
                let device = self.device(id, cap)?;
                device::check_action(device, cap, action, args.as_ref().unwrap_or(&Value::Null))
            }
            _ => Err("a step is a command {device, capability, value}, an action {device, capability, action, args?} or {scene}".into()),
        }
    }

    fn steps(&self, steps: &[Step], in_scene: bool) -> Result<(), String> {
        if steps.is_empty() || steps.len() > MAX_STEPS {
            return Err(format!("1-{MAX_STEPS} steps"));
        }
        for (i, step) in steps.iter().enumerate() {
            self.step(step, in_scene).map_err(|e| format!("step {}: {e}", i + 1))?;
        }
        Ok(())
    }

    fn trigger(&self, trigger: &Trigger) -> Result<(), String> {
        match trigger {
            Trigger::Time { at, days } => {
                parse_hhmm(at)?;
                check_days(days)
            }
            Trigger::Sun { offset_min, days, .. } => {
                self.sun()?;
                if !(-180..=180).contains(offset_min) {
                    return Err("offset_min must be -180 to 180".into());
                }
                check_days(days)
            }
            Trigger::State { device, capability, field, .. } => self.field(device, capability, field),
            Trigger::Threshold { device, capability, field, above, below } => {
                self.field(device, capability, field)?;
                match (above, below) {
                    (Some(v), None) | (None, Some(v)) if v.is_finite() => Ok(()),
                    _ => Err("a threshold has one number: above or below".into()),
                }
            }
        }
    }

    fn condition(&self, condition: &Condition) -> Result<(), String> {
        match condition {
            Condition::Time { after, before, days } => {
                if after.is_none() && before.is_none() && days.is_empty() {
                    return Err("a time condition needs after, before or days".into());
                }
                for t in [after, before].into_iter().flatten() {
                    parse_hhmm(t)?;
                }
                check_days(days)
            }
            Condition::Sun { .. } => self.sun(),
            Condition::State { device, capability, field, .. } => self.field(device, capability, field),
            Condition::Value { device, capability, field, above, below } => {
                self.field(device, capability, field)?;
                if above.is_none() && below.is_none() || [above, below].into_iter().flatten().any(|v| !v.is_finite()) {
                    return Err("a value condition needs above and/or below".into());
                }
                Ok(())
            }
        }
    }
}

/* ------------------------------------------------------------------ */
/* Evaluating (pure, unit-tested)                                      */
/* ------------------------------------------------------------------ */

/* A field of a device's capability: device "plug", capability "energy",
 * field "power_w" -> 40.2. None if the device doesn't have it. */
fn field_value(device: &Device, capability: &str, field: &str) -> Option<Value> {
    let caps = serde_json::to_value(&device.capabilities).ok()?;
    field
        .split('.')
        .try_fold(caps.get(capability)?, |v, key| match v {
            Value::Object(map) => map.get(key),
            Value::Array(items) => items.get(key.parse::<usize>().ok()?),
            _ => None,
        })
        .cloned()
}

/* JSON equality, except that numbers compare as numbers (1 == 1.0). */
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn in_range(value: f64, above: Option<f64>, below: Option<f64>) -> bool {
    above.is_none_or(|a| value > a) && below.is_none_or(|b| value < b)
}

/* Did this change fire a state / threshold trigger? */
fn fires_on_change(trigger: &Trigger, old: &Device, new: &Device) -> bool {
    match trigger {
        Trigger::State { device, capability, field, to, from } if *device == new.id => {
            let (before, after) = (field_value(old, capability, field), field_value(new, capability, field));
            let changed = match (&before, &after) {
                (Some(b), Some(a)) => !same(b, a),
                (None, Some(_)) => true,
                _ => false,
            };
            changed
                && to.as_ref().is_none_or(|to| after.as_ref().is_some_and(|a| same(a, to)))
                && from.as_ref().is_none_or(|from| before.as_ref().is_some_and(|b| same(b, from)))
        }
        Trigger::Threshold { device, capability, field, above, below } if *device == new.id => {
            let number = |d: &Device| field_value(d, capability, field).and_then(|v| v.as_f64());
            let was = number(old).is_some_and(|v| in_range(v, *above, *below));
            let is = number(new).is_some_and(|v| in_range(v, *above, *below));
            /* Crossing INTO the range: once, not on every report. */
            !was && is
        }
        _ => false,
    }
}

/* A device's current state as scene steps ("Save current state"):
 *   - switched off: only "off" (its brightness doesn't matter then);
 *   - on: "on", then colour, brightness, a cover's position (if it can go
 *     to any), a climate's mode/target/fan, a TV's volume/mute/input (only
 *     while an input is shown -- not an app);
 *   - never a lock: a scene that unlocks the door would be too easy to
 *     make by accident (#77's unlock rule). Sensors, energy, remotes have
 *     nothing to set. */
pub fn capture_steps(device: &Device) -> Vec<Step> {
    let caps = &device.capabilities;
    let step = |capability: &str, value: Value| Step {
        device: Some(device.id.clone()),
        capability: Some(capability.to_string()),
        value: Some(value),
        action: None,
        args: None,
        scene: None,
    };
    let mut steps = Vec::new();
    if let Some(switch) = &caps.switch {
        steps.push(step("switch", serde_json::json!({ "on": switch.on })));
        if !switch.on {
            return steps;
        }
    }
    if let Some(color) = &caps.color {
        steps.push(step("color", serde_json::json!(color)));
    }
    if let Some(dimmer) = &caps.dimmer {
        steps.push(step("dimmer", serde_json::json!(dimmer)));
    }
    if let Some(cover) = caps.cover.as_ref().filter(|c| c.can_position) {
        if let Some(position) = cover.position {
            steps.push(step("cover", serde_json::json!({ "position": position })));
        }
    }
    if let Some(climate) = &caps.climate {
        let mut value = serde_json::json!({ "mode": climate.mode, "target": climate.target });
        if let Some(fan) = &climate.fan {
            value["fan"] = fan.clone().into();
        }
        steps.push(step("climate", value));
    }
    if let Some(media) = caps.media.as_ref().filter(|m| !m.input.is_empty()) {
        steps.push(step("media", serde_json::json!({ "volume": media.volume, "muted": media.muted, "input": media.input })));
    }
    steps
}

/* The local time a scheduled trigger checks against. */
#[derive(Clone, Copy, Debug)]
struct Now {
    /* Minutes since local midnight. */
    minute: u32,
    weekday: Weekday,
    /* Today's sunrise / sunset, local minutes since midnight (None:
     * location not set, or polar day/night). */
    sun: Option<(i32, i32)>,
}

fn day_ok(days: &[Weekday], weekday: Weekday) -> bool {
    days.is_empty() || days.contains(&weekday)
}

fn minutes(t: NaiveTime) -> u32 {
    t.hour() * 60 + t.minute()
}

/* Did this minute fire a time / sun trigger? */
fn fires_at(trigger: &Trigger, now: &Now) -> bool {
    match trigger {
        Trigger::Time { at, days } => {
            day_ok(days, now.weekday) && parse_hhmm(at).is_ok_and(|t| minutes(t) == now.minute)
        }
        Trigger::Sun { event, offset_min, days } => {
            let Some((rise, set)) = now.sun else { return false };
            let base = if *event == SunEvent::Sunrise { rise } else { set };
            day_ok(days, now.weekday) && base + offset_min == now.minute as i32
        }
        _ => false,
    }
}

/* Do all conditions hold? (Devices as they are now.) */
fn conditions_hold(conditions: &[Condition], now: &Now, devices: &HashMap<DeviceId, Device>) -> bool {
    conditions.iter().all(|condition| match condition {
        Condition::Time { after, before, days } => {
            let m = now.minute;
            let after = after.as_deref().and_then(|t| parse_hhmm(t).ok()).map(minutes);
            let before = before.as_deref().and_then(|t| parse_hhmm(t).ok()).map(minutes);
            let in_window = match (after, before) {
                (Some(a), Some(b)) if a <= b => m >= a && m < b,
                /* Across midnight: after 22:00, before 06:00. */
                (Some(a), Some(b)) => m >= a || m < b,
                (Some(a), None) => m >= a,
                (None, Some(b)) => m < b,
                (None, None) => true,
            };
            in_window && day_ok(days, now.weekday)
        }
        Condition::Sun { is } => {
            let Some((rise, set)) = now.sun else { return false };
            let day = (now.minute as i32) >= rise && (now.minute as i32) < set;
            day == (*is == DayOrNight::Day)
        }
        Condition::State { device, capability, field, is } => devices
            .get(device)
            .and_then(|d| field_value(d, capability, field))
            .is_some_and(|v| same(&v, is)),
        Condition::Value { device, capability, field, above, below } => devices
            .get(device)
            .and_then(|d| field_value(d, capability, field))
            .and_then(|v| v.as_f64())
            .is_some_and(|v| in_range(v, *above, *below)),
    })
}

/* Sunrise and sunset on `date` at latitude/longitude (degrees, north and
 * east positive), as minutes after UTC midnight. None in polar day or
 * night (the sun doesn't rise or set). The NOAA solar calculator's
 * formulas: accurate to a minute or so, plenty for "lights on at sunset". */
pub fn sun_times(date: NaiveDate, latitude: f64, longitude: f64) -> Option<(f64, f64)> {
    let epoch = NaiveDate::from_ymd_opt(2000, 1, 1)?;
    /* Julian centuries since 2000-01-01 12:00 UTC, at noon of `date`. */
    let t = (date - epoch).num_days() as f64 / 36525.0;
    let rad = f64::to_radians;
    let deg = f64::to_degrees;
    let l0 = (280.46646 + t * (36000.76983 + t * 0.0003032)).rem_euclid(360.0);
    let m = 357.52911 + t * (35999.05029 - 0.0001537 * t);
    let e = 0.016708634 - t * (0.000042037 + 0.0000001267 * t);
    let c = rad(m).sin() * (1.914602 - t * (0.004817 + 0.000014 * t))
        + rad(2.0 * m).sin() * (0.019993 - 0.000101 * t)
        + rad(3.0 * m).sin() * 0.000289;
    let omega = 125.04 - 1934.136 * t;
    let lambda = l0 + c - 0.00569 - 0.00478 * rad(omega).sin();
    let eps0 = 23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.00059 - t * 0.001813))) / 60.0) / 60.0;
    let eps = eps0 + 0.00256 * rad(omega).cos();
    let decl = (rad(eps).sin() * rad(lambda).sin()).asin();
    let y = (rad(eps) / 2.0).tan().powi(2);
    /* The equation of time, minutes. */
    let eot = 4.0
        * deg(y * (2.0 * rad(l0)).sin() - 2.0 * e * rad(m).sin() + 4.0 * e * y * rad(m).sin() * (2.0 * rad(l0)).cos()
            - 0.5 * y * y * (4.0 * rad(l0)).sin()
            - 1.25 * e * e * (2.0 * rad(m)).sin());
    /* 90.833°: the sun's centre below the horizon, with refraction and its
     * radius -- when its top edge appears or disappears. */
    let cos_ha = rad(90.833).cos() / (rad(latitude).cos() * decl.cos()) - rad(latitude).tan() * decl.tan();
    if !(-1.0..=1.0).contains(&cos_ha) {
        return None;
    }
    let ha = deg(cos_ha.acos());
    let noon = 720.0 - 4.0 * longitude - eot;
    Some((noon - 4.0 * ha, noon + 4.0 * ha))
}

/* "Now" in the hub's time zone, with today's sun times. */
fn local_now(at: DateTime<Utc>, settings: &HubSettings) -> Now {
    let tz: chrono_tz::Tz = settings.time_zone.parse().unwrap_or(chrono_tz::UTC);
    let local = at.with_timezone(&tz);
    let date = local.date_naive();
    let sun = match (settings.latitude, settings.longitude) {
        (Some(lat), Some(lon)) => sun_times(date, lat, lon).map(|(rise, set)| {
            /* UTC minutes -> local minutes, with today's offset. */
            let offset = local.offset().fix().local_minus_utc() / 60;
            ((rise.round() as i32) + offset, (set.round() as i32) + offset)
        }),
        _ => None,
    };
    Now {
        minute: local.hour() * 60 + local.minute(),
        weekday: Weekday::of(local.weekday()),
        sun,
    }
}

/* The time now. (chrono's Utc::now needs its "clock" feature, which also
 * brings the system time zone lookup we don't use.) */
fn utc_now() -> DateTime<Utc> {
    DateTime::<Utc>::from(SystemTime::now())
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/* ------------------------------------------------------------------ */
/* The engine                                                          */
/* ------------------------------------------------------------------ */

/* Loop protection's memory (see the header). */
#[derive(Default)]
struct Runtime {
    /* Devices changed by an automation: which one, at which chain link,
     * until when that counts. */
    caused: HashMap<DeviceId, (String, u32, Instant)>,
    /* When each automation last ran (the last minute's runs). */
    runs: HashMap<String, VecDeque<Instant>>,
}

pub struct Automations {
    book: Mutex<Book>,
    runtime: Mutex<Runtime>,
    log: Mutex<VecDeque<LogEntry>>,
    /* store::writer's channel: whatever is sent is saved. */
    save_tx: watch::Sender<Vec<u8>>,
    /* Bumped on every change of the book (clients: automations_changed). */
    changed: watch::Sender<u64>,
    /* Every new log entry (clients: automation_ran). */
    ran: broadcast::Sender<LogEntry>,
    control: Control,
    settings: Arc<Settings>,
}

/* Who asked for a scene (the log's cause). */
pub enum Asker {
    Screen,
    App,
    Cloud,
}

impl Automations {
    pub fn new(book: Book, save_tx: watch::Sender<Vec<u8>>, control: Control, settings: Arc<Settings>) -> Self {
        Automations {
            book: Mutex::new(book),
            runtime: Mutex::new(Runtime::default()),
            log: Mutex::new(VecDeque::new()),
            save_tx,
            changed: watch::Sender::new(0),
            ran: broadcast::channel(32).0,
            control,
            settings,
        }
    }

    pub fn book(&self) -> Book {
        self.book.lock().unwrap().clone()
    }

    pub fn log(&self) -> Vec<LogEntry> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn subscribe_log(&self) -> broadcast::Receiver<LogEntry> {
        self.ran.subscribe()
    }

    /* The checks' context: the devices now, the location. */
    async fn devices(&self) -> Result<HashMap<DeviceId, Device>, String> {
        Ok(self.control.list().await?.into_iter().map(|d| (d.id.clone(), d)).collect())
    }

    fn has_location(&self) -> bool {
        let s = self.settings.get();
        s.latitude.is_some() && s.longitude.is_some()
    }

    /* Applies a change to the book, then saves it and tells clients. */
    fn change<T>(&self, apply: impl FnOnce(&mut Book) -> Result<T, String>) -> Result<T, String> {
        let mut book = self.book.lock().unwrap();
        let result = apply(&mut book)?;
        self.save_tx.send_replace(serde_json::to_vec(&*book).expect("a Book is always valid JSON"));
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        Ok(result)
    }

    /* Adds (no id, or a new one) or replaces (an existing id) a scene. */
    pub async fn save_scene(&self, mut scene: Scene) -> Result<Scene, String> {
        let devices = self.devices().await?;
        let scenes = self.book().scenes;
        check_name(&scene.name)?;
        CheckContext { devices: &devices, scenes: &scenes, has_location: self.has_location() }
            .steps(&scene.steps, true)
            .map_err(|e| format!("{}: {e}", scene.name))?;
        self.change(|book| {
            if scene.id.is_empty() {
                let taken: Vec<&str> = book.scenes.iter().map(|s| s.id.as_str()).collect();
                scene.id = id_from_name(&scene.name, &taken);
            } else if !valid_id(&scene.id) {
                return Err(format!("scene id {:?}: 1-40 of a-z, 0-9, -", scene.id));
            }
            match book.scenes.iter().position(|s| s.id == scene.id) {
                Some(i) => book.scenes[i] = scene.clone(),
                None if book.scenes.len() >= MAX_ITEMS => return Err(format!("at most {MAX_ITEMS} scenes")),
                None => book.scenes.push(scene.clone()),
            }
            Ok(scene)
        })
    }

    /* "Save current state" (the touchscreen, the app): a scene that puts
     * these devices back the way they are now. See capture_steps. */
    pub async fn capture_scene(&self, name: &str, device_ids: &[DeviceId]) -> Result<Scene, String> {
        if device_ids.is_empty() {
            return Err("pick at least one device".into());
        }
        let devices = self.devices().await?;
        let mut steps = Vec::new();
        for id in device_ids {
            let device = devices.get(id).ok_or_else(|| format!("there's no device {id:?}"))?;
            let captured = capture_steps(device);
            if captured.is_empty() {
                return Err(format!("{} has nothing a scene can set", device.name));
            }
            steps.extend(captured);
        }
        self.save_scene(Scene {
            id: String::new(),
            name: name.trim().to_string(),
            steps,
        })
        .await
    }

    /* A scene an automation runs can't be deleted (it would break it). */
    pub fn delete_scene(&self, id: &str) -> Result<(), String> {
        self.change(|book| {
            if let Some(a) = book.automations.iter().find(|a| a.steps.iter().any(|s| s.scene.as_deref() == Some(id))) {
                return Err(format!("the automation \"{}\" runs this scene: change it first", a.name));
            }
            let before = book.scenes.len();
            book.scenes.retain(|s| s.id != id);
            if book.scenes.len() == before {
                return Err(format!("there's no scene {id:?}"));
            }
            Ok(())
        })
    }

    pub async fn save_automation(&self, mut automation: Automation) -> Result<Automation, String> {
        let devices = self.devices().await?;
        let scenes = self.book().scenes;
        check_name(&automation.name)?;
        let ctx = CheckContext { devices: &devices, scenes: &scenes, has_location: self.has_location() };
        let named = |e: String| format!("{}: {e}", automation.name);
        if automation.triggers.is_empty() || automation.triggers.len() > MAX_TRIGGERS {
            return Err(named(format!("1-{MAX_TRIGGERS} triggers")));
        }
        if automation.conditions.len() > MAX_TRIGGERS {
            return Err(named(format!("at most {MAX_TRIGGERS} conditions")));
        }
        for (i, t) in automation.triggers.iter().enumerate() {
            ctx.trigger(t).map_err(|e| named(format!("trigger {}: {e}", i + 1)))?;
        }
        for (i, c) in automation.conditions.iter().enumerate() {
            ctx.condition(c).map_err(|e| named(format!("condition {}: {e}", i + 1)))?;
        }
        ctx.steps(&automation.steps, false).map_err(named)?;
        self.change(|book| {
            if automation.id.is_empty() {
                let taken: Vec<&str> = book.automations.iter().map(|a| a.id.as_str()).collect();
                automation.id = id_from_name(&automation.name, &taken);
            } else if !valid_id(&automation.id) {
                return Err(format!("automation id {:?}: 1-40 of a-z, 0-9, -", automation.id));
            }
            match book.automations.iter().position(|a| a.id == automation.id) {
                Some(i) => book.automations[i] = automation.clone(),
                None if book.automations.len() >= MAX_ITEMS => return Err(format!("at most {MAX_ITEMS} automations")),
                None => book.automations.push(automation.clone()),
            }
            Ok(automation)
        })
    }

    pub fn delete_automation(&self, id: &str) -> Result<(), String> {
        self.change(|book| {
            let before = book.automations.len();
            book.automations.retain(|a| a.id != id);
            if book.automations.len() == before {
                return Err(format!("there's no automation {id:?}"));
            }
            Ok(())
        })
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        self.change(|book| {
            let a = book.automations.iter_mut().find(|a| a.id == id).ok_or_else(|| format!("there's no automation {id:?}"))?;
            a.enabled = enabled;
            Ok(())
        })
        .inspect(|()| {
            /* A switched-on automation starts with a clean slate. */
            if enabled {
                self.runtime.lock().unwrap().runs.remove(id);
            }
        })
    }

    /* Runs a scene on request; the answer comes when it's done (a TV
     * waking can take a while). */
    pub async fn run_scene(&self, id: &str, asker: Asker) -> Result<(), String> {
        let scene = self.book().scenes.into_iter().find(|s| s.id == id).ok_or_else(|| format!("there's no scene {id:?}"))?;
        let cause = match asker {
            Asker::Screen => "touchscreen",
            Asker::App => "app",
            Asker::Cloud => "cloud",
        };
        let errors = self.run_steps(&scene.steps, None, 0).await;
        self.add_log("scene", &scene.id, &scene.name, cause, &errors);
        match errors.is_empty() {
            true => Ok(()),
            false => Err(errors.join("; ")),
        }
    }

    /* "Run now" for an automation (testing it): its steps, without
     * triggers or conditions. */
    pub async fn run_automation_now(&self, id: &str) -> Result<(), String> {
        let a = self.book().automations.into_iter().find(|a| a.id == id).ok_or_else(|| format!("there's no automation {id:?}"))?;
        let errors = self.run_steps(&a.steps, Some(&a.id), 0).await;
        self.add_log("automation", &a.id, &a.name, "run now", &errors);
        match errors.is_empty() {
            true => Ok(()),
            false => Err(errors.join("; ")),
        }
    }

    /* Carries out steps one after the other; a failed one doesn't stop
     * the rest (the lights still go off if the TV doesn't answer). But a
     * device that failed once is skipped for the rest of the run: an
     * unplugged bulb would otherwise cost every one of its steps a full
     * timeout, and say the same thing each time. The errors, one sentence
     * per device. `by`: the automation running them (loop protection), at
     * chain link `depth`. */
    async fn run_steps(&self, steps: &[Step], by: Option<&str>, depth: u32) -> Vec<String> {
        let mut errors = Vec::new();
        let mut failed: Vec<DeviceId> = Vec::new();
        self.run_steps_inner(steps, by, depth, &mut errors, &mut failed).await;
        errors
    }

    async fn run_steps_inner(&self, steps: &[Step], by: Option<&str>, depth: u32, errors: &mut Vec<String>, failed: &mut Vec<DeviceId>) {
        for step in steps {
            if let Some(scene) = &step.scene {
                let found = self.book().scenes.into_iter().find(|s| &s.id == scene);
                match found {
                    Some(s) => Box::pin(self.run_steps_inner(&s.steps, by, depth, errors, failed)).await,
                    None => errors.push(format!("there's no scene {scene:?} any more")),
                }
                continue;
            }
            let (Some(id), Some(cap)) = (&step.device, &step.capability) else { continue };
            if failed.contains(id) {
                continue;
            }
            /* Recorded BEFORE the command: the device's change may arrive
             * before the command's answer. */
            if let Some(by) = by {
                self.runtime.lock().unwrap().caused.insert(id.clone(), (by.to_string(), depth, Instant::now() + CAUSED_FOR));
            }
            let result = match (&step.value, &step.action) {
                (Some(value), _) => self.control.command(id, cap, value.clone()).await.map(|_| ()),
                (None, Some(action)) => self
                    .control
                    .action(id, cap, action, step.args.clone().unwrap_or(Value::Null))
                    .await
                    .map(|_| ()),
                (None, None) => Ok(()),
            };
            if let Err(e) = result {
                errors.push(format!("{id}: {e}"));
                failed.push(id.clone());
            }
        }
    }

    fn add_log(&self, kind: &'static str, id: &str, name: &str, cause: &str, errors: &[String]) {
        let entry = LogEntry {
            at: unix_now(),
            kind,
            id: id.to_string(),
            name: name.to_string(),
            cause: cause.to_string(),
            ok: errors.is_empty(),
            detail: errors.join("; "),
        };
        println!(
            "automations: {kind} {name} ({cause}){}",
            if entry.ok { String::new() } else { format!(": {}", entry.detail) }
        );
        let mut log = self.log.lock().unwrap();
        if log.len() >= LOG_SIZE {
            log.pop_front();
        }
        log.push_back(entry.clone());
        let _ = self.ran.send(entry);
    }

    /* An automation's trigger fired: loop protection, conditions, then
     * its steps -- in a task of their own, so a slow device never holds
     * up the next trigger. */
    fn fire(self: &Arc<Self>, automation: Automation, cause: String, depth: u32, now: &Now, devices: &HashMap<DeviceId, Device>) {
        if !conditions_hold(&automation.conditions, now, devices) {
            return;
        }
        if depth > MAX_CHAIN {
            self.add_log(
                "automation",
                &automation.id,
                &automation.name,
                &cause,
                &[format!("stopped: {MAX_CHAIN} automations triggered each other in a row (a loop?)")],
            );
            return;
        }
        let too_often = {
            let mut runtime = self.runtime.lock().unwrap();
            let runs = runtime.runs.entry(automation.id.clone()).or_default();
            let minute_ago = Instant::now().checked_sub(Duration::from_secs(60));
            runs.retain(|t| minute_ago.is_none_or(|m| *t > m));
            runs.push_back(Instant::now());
            runs.len() > MAX_RUNS_PER_MINUTE
        };
        if too_often {
            let _ = self.set_enabled(&automation.id, false);
            self.add_log(
                "automation",
                &automation.id,
                &automation.name,
                &cause,
                &[format!("switched off: it ran more than {MAX_RUNS_PER_MINUTE} times in a minute (a loop?)")],
            );
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            let errors = engine.run_steps(&automation.steps, Some(&automation.id), depth).await;
            engine.add_log("automation", &automation.id, &automation.name, &cause, &errors);
        });
    }

    /* A device changed: its state and threshold triggers. */
    fn on_change(self: &Arc<Self>, old: &Device, new: &Device, devices: &HashMap<DeviceId, Device>, now: &Now) {
        /* Which automation caused this change, at which link? */
        let caused = {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.caused.retain(|_, (_, _, until)| *until > Instant::now());
            runtime.caused.get(&new.id).cloned()
        };
        for automation in self.book().automations.into_iter().filter(|a| a.enabled) {
            let Some(trigger) = automation.triggers.iter().find(|t| fires_on_change(t, old, new)) else { continue };
            let depth = match &caused {
                /* Its own doing: ignored (see the header). */
                Some((by, _, _)) if *by == automation.id => continue,
                Some((_, depth, _)) => depth + 1,
                None => 0,
            };
            let cause = match trigger {
                Trigger::State { field, capability, .. } | Trigger::Threshold { field, capability, .. } => {
                    let value = field_value(new, capability, field).map_or("?".into(), |v| v.to_string());
                    format!("{}: {capability}.{field} is {value}", new.name)
                }
                _ => String::new(),
            };
            self.fire(automation, cause, depth, now, devices);
        }
    }

    /* A new minute: time and sun triggers. */
    fn on_minute(self: &Arc<Self>, now: &Now, devices: &HashMap<DeviceId, Device>) {
        for automation in self.book().automations.into_iter().filter(|a| a.enabled) {
            let Some(trigger) = automation.triggers.iter().find(|t| fires_at(t, now)) else { continue };
            let cause = match trigger {
                Trigger::Time { at, .. } => at.clone(),
                Trigger::Sun { event: SunEvent::Sunrise, offset_min, .. } => with_offset("sunrise", *offset_min),
                Trigger::Sun { event: SunEvent::Sunset, offset_min, .. } => with_offset("sunset", *offset_min),
                _ => String::new(),
            };
            self.fire(automation, cause, 0, now, devices);
        }
    }
}

fn with_offset(event: &str, offset: i32) -> String {
    match offset {
        0 => event.to_string(),
        o if o > 0 => format!("{o} min after {event}"),
        o => format!("{} min before {event}", -o),
    }
}

/* Runs for the daemon's lifetime (spawned from main): follows every
 * device change, and wakes at the start of every minute. */
pub async fn run(engine: Arc<Automations>, mut events: broadcast::Receiver<Event>) {
    let mut devices: HashMap<DeviceId, Device> = engine.devices().await.unwrap_or_default();
    let mut warned_clock = false;
    loop {
        /* To the start of the next minute (+ a little, so it's surely in). */
        let into_minute = unix_now() % 60;
        let wake = tokio::time::sleep(Duration::from_secs(60 - into_minute) + Duration::from_millis(200));
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Changed(new)) => {
                    let old = devices.insert(new.id.clone(), new.clone());
                    if let Some(old) = old {
                        let now = local_now(utc_now(), &engine.settings.get());
                        engine.on_change(&old, &new, &devices, &now);
                    }
                }
                Ok(Event::Removed(id)) => {
                    devices.remove(&id);
                }
                /* Too slow for a moment: read every device again (changes
                 * in between are missed -- logged). */
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    println!("automations: missed {n} device events");
                    devices = engine.devices().await.unwrap_or(devices);
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = wake => {
                if !crate::mqtt::clock_synced() {
                    if !warned_clock {
                        println!("automations: the clock isn't synced (no NTP yet): time and sun triggers wait");
                        warned_clock = true;
                    }
                    continue;
                }
                warned_clock = false;
                let now = local_now(utc_now(), &engine.settings.get());
                engine.on_minute(&now, &devices);
            }
        }
    }
}

/* For store::load_or_default. */
pub fn decode(schema: u32, payload: &[u8]) -> Result<Book, String> {
    if schema != AUTOMATIONS_SCHEMA {
        return Err(format!("unsupported automations schema {schema}"));
    }
    serde_json::from_slice(payload).map_err(|e| e.to_string())
}

/* The scenes, for the cloud's "scenes" shadow (mqtt.rs): id and name. */
pub fn scene_list(book: &Book) -> BTreeMap<String, String> {
    book.scenes.iter().map(|s| (s.id.clone(), s.name.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn device(id: &str, caps: Value) -> Device {
        serde_json::from_value(json!({"id": id, "name": id.to_uppercase(), "capabilities": caps})).unwrap()
    }

    fn devices() -> HashMap<DeviceId, Device> {
        [
            device("tv", json!({"switch": {"on": false}})),
            device("plug", json!({"switch": {"on": true}, "energy": {"power_w": 2.0}})),
            device("door", json!({"lock": {"state": "locked"}})),
            device("thermo", json!({"sensor": {"readings": {"temperature": {"value": 21.0, "unit": "°C"}}}})),
        ]
        .into_iter()
        .map(|d| (d.id.clone(), d))
        .collect()
    }

    fn now(minute: u32, weekday: Weekday) -> Now {
        Now {
            minute,
            weekday,
            sun: Some((7 * 60 + 15, 18 * 60 + 50)),
        }
    }

    #[test]
    fn sun_times_match_the_almanac() {
        /* Bucharest (44.43 N, 26.10 E). Almanac, UTC: 21 June sunrise
         * 02:31, sunset 18:01; 21 December 05:51 and 14:39. */
        let check = |date: (i32, u32, u32), rise: f64, set: f64| {
            let (r, s) = sun_times(NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap(), 44.4268, 26.1025).unwrap();
            assert!((r - rise).abs() < 3.0 && (s - set).abs() < 3.0, "{date:?}: {r:.1} {s:.1}");
        };
        check((2026, 6, 21), 2.0 * 60.0 + 31.0, 18.0 * 60.0 + 1.0);
        check((2026, 12, 21), 5.0 * 60.0 + 51.0, 14.0 * 60.0 + 39.0);
        /* Polar night at Tromsø (69.6 N) in December: no sunrise. */
        assert!(sun_times(NaiveDate::from_ymd_opt(2026, 12, 21).unwrap(), 69.65, 18.96).is_none());
    }

    #[test]
    fn local_time_uses_the_hub_time_zone_and_location() {
        let settings = HubSettings {
            time_zone: "Europe/Bucharest".into(),
            latitude: Some(44.4268),
            longitude: Some(26.1025),
            ..Default::default()
        };
        /* 2026-06-21 18:01 UTC = 21:01 in Bucharest (EEST, +3). */
        let at = DateTime::parse_from_rfc3339("2026-06-21T18:01:00Z").unwrap().with_timezone(&Utc);
        let now = local_now(at, &settings);
        assert_eq!(now.minute, 21 * 60 + 1);
        assert_eq!(now.weekday, Weekday::Sun);
        let (_, set) = now.sun.unwrap();
        assert!((set - (21 * 60 + 1)).abs() <= 3, "sunset {set}");
    }

    #[test]
    fn time_and_sun_triggers() {
        let at = |t: &str, days: Vec<Weekday>| Trigger::Time { at: t.into(), days };
        assert!(fires_at(&at("07:30", vec![]), &now(7 * 60 + 30, Weekday::Fri)));
        assert!(!fires_at(&at("07:30", vec![]), &now(7 * 60 + 31, Weekday::Fri)));
        assert!(!fires_at(&at("07:30", vec![Weekday::Sat, Weekday::Sun]), &now(7 * 60 + 30, Weekday::Fri)));
        let sunset = |offset_min| Trigger::Sun { event: SunEvent::Sunset, offset_min, days: vec![] };
        assert!(fires_at(&sunset(0), &now(18 * 60 + 50, Weekday::Fri)));
        assert!(fires_at(&sunset(-20), &now(18 * 60 + 30, Weekday::Fri)));
        /* No location: never. */
        assert!(!fires_at(&sunset(0), &Now { sun: None, ..now(18 * 60 + 50, Weekday::Fri) }));
    }

    #[test]
    fn state_and_threshold_triggers_fire_on_the_change_only() {
        let off = device("tv", json!({"switch": {"on": false}}));
        let on = device("tv", json!({"switch": {"on": true}}));
        let turns_on = Trigger::State { device: "tv".into(), capability: "switch".into(), field: "on".into(), to: Some(json!(true)), from: None };
        assert!(fires_on_change(&turns_on, &off, &on));
        assert!(!fires_on_change(&turns_on, &on, &off));
        assert!(!fires_on_change(&turns_on, &on, &on), "no change, no trigger");
        let any = Trigger::State { device: "tv".into(), capability: "switch".into(), field: "on".into(), to: None, from: None };
        assert!(fires_on_change(&any, &on, &off));

        let watts = |w: f64| device("plug", json!({"energy": {"power_w": w}}));
        let above5 = Trigger::Threshold { device: "plug".into(), capability: "energy".into(), field: "power_w".into(), above: Some(5.0), below: None };
        assert!(fires_on_change(&above5, &watts(2.0), &watts(40.0)));
        assert!(!fires_on_change(&above5, &watts(40.0), &watts(41.0)), "still above: once per crossing");
        assert!(!fires_on_change(&above5, &watts(40.0), &watts(2.0)));
        /* Another device's change doesn't concern it. */
        assert!(!fires_on_change(&above5, &off, &on));
        /* A path into readings. */
        let temp = |t: f64| device("thermo", json!({"sensor": {"readings": {"temperature": {"value": t, "unit": "°C"}}}}));
        let cold = Trigger::Threshold { device: "thermo".into(), capability: "sensor".into(), field: "readings.temperature.value".into(), above: None, below: Some(18.0) };
        assert!(fires_on_change(&cold, &temp(18.5), &temp(17.9)));
    }

    #[test]
    fn conditions() {
        let d = devices();
        let window = |after: &str, before: &str| Condition::Time { after: Some(after.into()), before: Some(before.into()), days: vec![] };
        assert!(conditions_hold(&[window("18:00", "23:00")], &now(20 * 60, Weekday::Mon), &d));
        assert!(!conditions_hold(&[window("18:00", "23:00")], &now(23 * 60, Weekday::Mon), &d));
        /* Across midnight. */
        assert!(conditions_hold(&[window("22:00", "06:00")], &now(60, Weekday::Mon), &d));
        assert!(!conditions_hold(&[window("22:00", "06:00")], &now(12 * 60, Weekday::Mon), &d));
        assert!(conditions_hold(&[Condition::Sun { is: DayOrNight::Night }], &now(20 * 60, Weekday::Mon), &d));
        assert!(!conditions_hold(&[Condition::Sun { is: DayOrNight::Night }], &now(12 * 60, Weekday::Mon), &d));
        let tv_on = Condition::State { device: "tv".into(), capability: "switch".into(), field: "on".into(), is: json!(true) };
        assert!(!conditions_hold(&[tv_on], &now(12 * 60, Weekday::Mon), &d));
        let low_power = Condition::Value { device: "plug".into(), capability: "energy".into(), field: "power_w".into(), above: None, below: Some(5.0) };
        assert!(conditions_hold(&[low_power], &now(12 * 60, Weekday::Mon), &d));
        assert!(conditions_hold(&[], &now(0, Weekday::Mon), &d), "no conditions: always");
    }

    #[test]
    fn saving_checks_everything() {
        let d = devices();
        let scenes = vec![Scene { id: "movie".into(), name: "Movie".into(), steps: vec![] }];
        let ctx = CheckContext { devices: &d, scenes: &scenes, has_location: false };
        let cmd = |device: &str, cap: &str, value: Value| Step {
            device: Some(device.into()), capability: Some(cap.into()), value: Some(value), action: None, args: None, scene: None,
        };
        let scene_step = Step { device: None, capability: None, value: None, action: None, args: None, scene: Some("movie".into()) };
        assert_eq!(ctx.step(&cmd("tv", "switch", json!({"on": true})), true), Ok(()));
        assert!(ctx.step(&cmd("tv", "switch", json!({"on": 1})), true).is_err(), "checked like a tap");
        assert!(ctx.step(&cmd("ghost", "switch", json!({"on": true})), true).unwrap_err().contains("no device"));
        assert!(ctx.step(&cmd("tv", "dimmer", json!({"level": 3})), true).unwrap_err().contains("no capability"));
        /* The unlock rule holds here too. */
        assert!(ctx.step(&cmd("door", "lock", json!({"state": "unlocked"})), false).unwrap_err().contains("confirm"));
        assert_eq!(ctx.step(&cmd("door", "lock", json!({"state": "unlocked", "confirmed": true})), false), Ok(()));
        assert!(ctx.step(&scene_step, true).unwrap_err().contains("can't run another scene"));
        assert_eq!(ctx.step(&scene_step, false), Ok(()));
        let mixed = Step { scene: Some("movie".into()), ..cmd("tv", "switch", json!({"on": true})) };
        assert!(ctx.step(&mixed, false).is_err());

        assert!(ctx.trigger(&Trigger::Time { at: "25:00".into(), days: vec![] }).is_err());
        assert!(ctx.trigger(&Trigger::Time { at: "07:30".into(), days: vec![Weekday::Mon, Weekday::Mon] }).is_err());
        assert!(ctx.trigger(&Trigger::Sun { event: SunEvent::Sunset, offset_min: 0, days: vec![] }).unwrap_err().contains("location"));
        let both = Trigger::Threshold { device: "plug".into(), capability: "energy".into(), field: "power_w".into(), above: Some(1.0), below: Some(9.0) };
        assert!(ctx.trigger(&both).is_err());
        assert!(ctx.condition(&Condition::Time { after: None, before: None, days: vec![] }).is_err());
    }

    #[test]
    fn capturing_the_current_state() {
        let lamp = device("lamp", json!({"switch": {"on": true}, "dimmer": {"level": 30}, "color": {"kelvin": 2700}}));
        let what: Vec<(String, Value)> = capture_steps(&lamp)
            .into_iter()
            .map(|s| (s.capability.unwrap(), s.value.unwrap()))
            .collect();
        assert_eq!(
            what,
            vec![
                ("switch".into(), json!({"on": true})),
                ("color".into(), json!({"kelvin": 2700})),
                ("dimmer".into(), json!({"level": 30}))
            ]
        );
        /* Off: just off. */
        let off = device("lamp", json!({"switch": {"on": false}, "dimmer": {"level": 30}}));
        assert_eq!(capture_steps(&off).len(), 1);
        /* Never a lock; nothing for sensors. */
        assert!(capture_steps(&device("door", json!({"lock": {"state": "unlocked"}}))).is_empty());
        assert!(capture_steps(&device("t", json!({"sensor": {"readings": {}}}))).is_empty());
        /* A blind that can go anywhere: its position; a garage door: nothing. */
        let blind = device("b", json!({"cover": {"position": 40, "can_position": true}}));
        assert_eq!(capture_steps(&blind)[0].value, Some(json!({"position": 40})));
        assert!(capture_steps(&device("g", json!({"cover": {"position": 0}}))).is_empty());
    }

    #[test]
    fn ids_come_from_names() {
        assert_eq!(id_from_name("Movie night!", &[]), "movie-night");
        assert_eq!(id_from_name("Movie night", &["movie-night"]), "movie-night-2");
        assert_eq!(id_from_name("Ăăă", &[]), "item");
    }

    /* ---- the engine, against the real state actor (test_hub.rs) ---- */

    async fn engine_with(devices: Vec<Device>) -> (Arc<Automations>, crate::adapters::test_hub::TestHub) {
        let mut devices = devices.into_iter();
        let first = devices.next().unwrap();
        let hub = crate::adapters::test_hub::TestHub::start(first, Box::new(crate::adapters::wled::Wled)).await;
        for d in devices {
            hub.control.add(d).await.unwrap();
        }
        let settings = Arc::new(Settings::new(HubSettings::default(), watch::channel(Vec::new()).0));
        let engine = Arc::new(Automations::new(Book::default(), watch::channel(Vec::new()).0, hub.control.clone(), settings));
        (engine, hub)
    }

    fn virtual_device(id: &str, caps: Value) -> Device {
        serde_json::from_value(json!({"id": id, "name": id, "source": "virtual", "capabilities": caps})).unwrap()
    }

    #[tokio::test]
    async fn scenes_run_their_steps_and_log() {
        let (engine, hub) = engine_with(vec![
            virtual_device("lamp", json!({"switch": {"on": false}, "dimmer": {"level": 100}})),
            virtual_device("blind", json!({"cover": {"position": 100, "can_position": true}})),
        ])
        .await;
        let scene = engine
            .save_scene(serde_json::from_value(json!({"name": "Movie", "steps": [
                {"device": "lamp", "capability": "dimmer", "value": {"level": 20}},
                {"device": "blind", "capability": "cover", "action": "close"}
            ]})).unwrap())
            .await
            .unwrap();
        assert_eq!(scene.id, "movie");
        engine.run_scene("movie", Asker::Screen).await.unwrap();
        let lamp = hub.control.get("lamp").await.unwrap().unwrap();
        assert_eq!(lamp.capabilities.dimmer.unwrap().level, 20);
        assert_eq!(hub.control.get("blind").await.unwrap().unwrap().capabilities.cover.unwrap().position, Some(0));
        let log = engine.log();
        assert_eq!((log[0].name.as_str(), log[0].cause.as_str(), log[0].ok), ("Movie", "touchscreen", true));

        /* A device removed afterwards: the scene still runs the rest, and
         * says what failed -- once per device, however many steps it had. */
        engine
            .save_scene(serde_json::from_value(json!({"id": "movie", "name": "Movie", "steps": [
                {"device": "blind", "capability": "cover", "action": "close"},
                {"device": "lamp", "capability": "dimmer", "value": {"level": 40}},
                {"device": "blind", "capability": "cover", "action": "open"}
            ]})).unwrap())
            .await
            .unwrap();
        hub.control.remove("blind").await.unwrap();
        let err = engine.run_scene("movie", Asker::App).await.unwrap_err();
        assert!(!err.contains(';'), "one sentence: {err}");
        assert_eq!(hub.control.get("lamp").await.unwrap().unwrap().capabilities.dimmer.unwrap().level, 40);
        assert!(!engine.log()[1].ok);
    }

    #[tokio::test]
    async fn state_triggers_run_and_loops_are_stopped() {
        let (engine, hub) = engine_with(vec![
            virtual_device("a", json!({"switch": {"on": false}})),
            virtual_device("b", json!({"switch": {"on": false}})),
        ])
        .await;
        /* Four automations that flip each other forever: a on -> b on ->
         * a off -> b off -> a on -> ... */
        let rule = |when: &str, to: bool, then: &str, set: bool| -> Automation {
            serde_json::from_value(json!({"name": format!("{when} {to} -> {then} {set}"),
                "triggers": [{"type": "state", "device": when, "capability": "switch", "field": "on", "to": to}],
                "steps": [{"device": then, "capability": "switch", "value": {"on": set}}]})).unwrap()
        };
        engine.save_automation(rule("a", true, "b", true)).await.unwrap();
        engine.save_automation(rule("b", true, "a", false)).await.unwrap();
        engine.save_automation(rule("a", false, "b", false)).await.unwrap();
        engine.save_automation(rule("b", false, "a", true)).await.unwrap();
        tokio::spawn(run(engine.clone(), hub.events()));
        tokio::time::sleep(Duration::from_millis(100)).await;

        hub.control.command("a", "switch", json!({"on": true})).await.unwrap();
        /* The ring runs MAX_CHAIN + 1 links (0..=MAX_CHAIN), then the
         * next one is stopped -- and nothing more happens. */
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let log = engine.log();
        /* (In any order: a stop is logged at once, a run when its steps
         * are done.) */
        assert_eq!(log.len(), MAX_CHAIN as usize + 2, "{log:#?}");
        assert_eq!(log.iter().filter(|e| e.ok).count(), MAX_CHAIN as usize + 1);
        assert!(log.iter().any(|e| e.detail.contains("in a row")), "{log:#?}");
        assert_eq!(log[0].cause, "a: switch.on is true");
    }

    #[tokio::test]
    async fn an_automation_running_too_often_is_switched_off() {
        let (engine, _hub) = engine_with(vec![virtual_device("x", json!({"switch": {"on": false}}))]).await;
        let a: Automation = serde_json::from_value(json!({"name": "Busy",
            "triggers": [{"type": "time", "at": "07:30"}],
            "steps": [{"device": "x", "capability": "switch", "value": {"on": true}}]})).unwrap();
        let a = engine.save_automation(a).await.unwrap();
        let d = devices();
        for _ in 0..=MAX_RUNS_PER_MINUTE {
            engine.fire(a.clone(), "test".into(), 0, &now(0, Weekday::Mon), &d);
        }
        assert!(!engine.book().automations[0].enabled);
        let last = engine.log().last().cloned().unwrap();
        assert!(last.detail.contains("switched off"), "{last:?}");
        /* Too long a chain: stopped. */
        engine.fire(a, "test".into(), MAX_CHAIN + 1, &now(0, Weekday::Mon), &d);
        assert!(engine.log().last().unwrap().detail.contains("in a row"));
    }

    #[tokio::test]
    async fn a_scene_in_use_cant_be_deleted() {
        let (engine, _hub) = engine_with(vec![virtual_device("x", json!({"switch": {"on": false}}))]).await;
        engine
            .save_scene(serde_json::from_value(json!({"name": "Night", "steps": [{"device": "x", "capability": "switch", "value": {"on": false}}]})).unwrap())
            .await
            .unwrap();
        engine
            .save_automation(serde_json::from_value(json!({"name": "At 23", "triggers": [{"type": "time", "at": "23:00"}], "steps": [{"scene": "night"}]})).unwrap())
            .await
            .unwrap();
        assert!(engine.delete_scene("night").unwrap_err().contains("At 23"));
        engine.delete_automation("at-23").unwrap();
        engine.delete_scene("night").unwrap();
        assert_eq!(engine.book(), Book::default());
    }
}
