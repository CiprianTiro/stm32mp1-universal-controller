/*
 * device.rs -- what a device IS (issue #34): the capability-based device
 * model, the rules a command must follow, and the conversion of devices
 * stored in the old format.
 *
 * BEFORE: a device was a free-form bag of values, {"on": true,
 * "brightness": 80}. Anything could write anything into it, and a screen
 * or app had to know per device what the values meant.
 *
 * NOW: a device describes what it CAN DO, as a set of CAPABILITIES -- small,
 * reusable building blocks, each with typed state:
 *
 *   {
 *     "id": "lamp-1", "name": "Living room lamp", "room": "Living room",
 *     "template": "dimmable-light", "source": "virtual",
 *     "capabilities": { "switch": {"on": true}, "dimmer": {"level": 80} }
 *   }
 *
 * A smart bulb is switch + dimmer + color; a thermometer is a sensor; later
 * a TV is switch + media, a vacuum is vacuum + sensor. Any UI renders a
 * device from its capabilities alone: switch -> toggle, dimmer -> slider.
 *
 * WHAT'S HERE: switch, dimmer, color and sensor -- the general ones almost
 * every device type uses. The specialised ones (media, vacuum,
 * camera_stream, ir_remote) come with their devices (#42-#45). Adding one
 * means: a struct for its state, `impl Capability` (its rules), a field in
 * `Capabilities`, and one line in `set_capability`. Nothing else changes.
 *
 * Everything here is pure (no I/O, no async) and unit-tested below.
 */
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::shadow;
use crate::state::DeviceId;

/* ------------------------------------------------------------------ */
/* The model                                                           */
/* ------------------------------------------------------------------ */

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Device {
    /* Stable id, also the AWS shadow's name (so: shadow::valid_name). */
    pub id: DeviceId,
    /* What people call it ("Living room lamp"). */
    pub name: String,
    /* Where it is; empty = not assigned to a room. */
    #[serde(default)]
    pub room: String,
    /* Which template created it (#40), e.g. "dimmable-light"; informative
     * only -- the capabilities are what count. */
    #[serde(default)]
    pub template: String,
    /* Where the device's state really lives (see Source). */
    #[serde(default)]
    pub source: Source,
    pub capabilities: Capabilities,
}

/* Where a device's truth is. For a VIRTUAL device the hub's own record is
 * the truth (test devices, and devices whose real driver doesn't exist
 * yet). For hardware, the hub only reports what the hardware confirms:
 * a command goes to the hardware first, and the state changes when it
 * answers. More sources come with their drivers (Zigbee #46, the ESP32 IR
 * blaster #42, ...). */
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Virtual,
    /* The board's LED LD7, driven by the Cortex-M4 (rpmsg.rs). */
    M4Led,
}

impl Source {
    /* Part of the hub itself: always there, can't be removed (deleting
     * its cloud shadow just recreates it). */
    pub fn is_builtin(self) -> bool {
        matches!(self, Source::M4Led)
    }
}

/* A device's capabilities: each one present or not. A struct of Options
 * (rather than a list or a map) makes the JSON exactly
 * {"switch": {...}, "dimmer": {...}}, gives every capability a real Rust
 * type, and -- with deny_unknown_fields -- makes serde itself reject a
 * capability that doesn't exist ("colour", "switchh"). */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch: Option<Switch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimmer: Option<Dimmer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor: Option<Sensor>,
}

/* The names, e.g. for error messages and the protocol's "hello". */
pub const CAPABILITY_NAMES: [&str; 4] = ["switch", "dimmer", "color", "sensor"];

/* On/off: lamps, plugs, relays, a TV's power. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Switch {
    pub on: bool,
}

/* A level from 0 to 100 (%): dimmable lamps, LED strips, blinds. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Dimmer {
    pub level: u8,
}

/* A colour OR a white temperature -- bulbs are in one mode or the other,
 * so exactly one of the two is set:
 *   {"hex": "#FF8800"}   a colour, as red/green/blue in hex (like CSS)
 *   {"kelvin": 2700}     white light; 2700 = warm, 6500 = cold daylight */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Color {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kelvin: Option<u16>,
}

/* Readings: {"temperature": {"value": 21.5, "unit": "°C"}, ...}. Only
 * the device itself reports them -- a client can't "set" a temperature. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Sensor {
    pub readings: BTreeMap<String, Reading>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    pub value: f64,
    #[serde(default)]
    pub unit: String,
}

/* ------------------------------------------------------------------ */
/* The rules                                                           */
/* ------------------------------------------------------------------ */

/* What every capability's state type provides: its name, whether clients
 * may set it, and the check of its values (the ranges that a type alone
 * can't express, e.g. a u8 allows 255 but a dimmer only goes to 100). */
trait Capability: DeserializeOwned {
    const NAME: &'static str;
    /* false: only the device itself reports this state (sensors). */
    const SETTABLE: bool = true;
    fn check(&self) -> Result<(), String>;
}

impl Capability for Switch {
    const NAME: &'static str = "switch";
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

impl Capability for Dimmer {
    const NAME: &'static str = "dimmer";
    fn check(&self) -> Result<(), String> {
        if self.level > 100 {
            return Err(format!("dimmer level must be 0-100, got {}", self.level));
        }
        Ok(())
    }
}

/* White temperatures real bulbs and LED strips offer. */
const KELVIN_RANGE: std::ops::RangeInclusive<u16> = 1000..=10000;

impl Capability for Color {
    const NAME: &'static str = "color";
    fn check(&self) -> Result<(), String> {
        match (&self.hex, self.kelvin) {
            (Some(hex), None) => {
                let digits = hex.strip_prefix('#').unwrap_or("");
                if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("color hex must look like \"#FF8800\", got {hex:?}"));
                }
                Ok(())
            }
            (None, Some(kelvin)) if KELVIN_RANGE.contains(&kelvin) => Ok(()),
            (None, Some(kelvin)) => Err(format!(
                "color kelvin must be {}-{}, got {kelvin}",
                KELVIN_RANGE.start(),
                KELVIN_RANGE.end()
            )),
            _ => Err("color needs exactly one of \"hex\" or \"kelvin\"".into()),
        }
    }
}

impl Capability for Sensor {
    const NAME: &'static str = "sensor";
    const SETTABLE: bool = false;
    fn check(&self) -> Result<(), String> {
        for (name, reading) in &self.readings {
            if name.is_empty() || name.len() > 32 {
                return Err(format!("sensor reading names must be 1-32 characters, got {name:?}"));
            }
            if !reading.value.is_finite() || reading.unit.len() > 16 {
                return Err(format!("sensor reading {name:?} is invalid"));
            }
        }
        Ok(())
    }
}

/* Who is changing a capability. A client (the touchscreen, the app, a
 * cloud command) asks for a change; the device itself (the hardware, via
 * its driver) reports what IS. Only the device may report read-only
 * capabilities such as sensor readings. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Client,
    Device,
}

/* The one door through which a capability's state changes: checks that
 * the device HAS this capability, that the value has the right shape
 * (serde, deny_unknown_fields) and range (check), and that the caller may
 * set it -- then returns the device's new capabilities. The device itself
 * is not modified here; state.rs stores the result. Every rejection comes
 * with a message a person can act on. */
pub fn set_capability(
    device: &Device,
    capability: &str,
    value: serde_json::Value,
    origin: Origin,
) -> Result<Capabilities, String> {
    let mut caps = device.capabilities.clone();
    let id = &device.id;
    match capability {
        "switch" => replace(&mut caps.switch, id, value, origin)?,
        "dimmer" => replace(&mut caps.dimmer, id, value, origin)?,
        "color" => replace(&mut caps.color, id, value, origin)?,
        "sensor" => replace(&mut caps.sensor, id, value, origin)?,
        other => {
            return Err(format!(
                "unknown capability {other:?} (known: {})",
                CAPABILITY_NAMES.join(", ")
            ))
        }
    }
    Ok(caps)
}

/* The generic part of set_capability, for one capability type T. */
fn replace<T: Capability>(
    slot: &mut Option<T>,
    id: &str,
    value: serde_json::Value,
    origin: Origin,
) -> Result<(), String> {
    if slot.is_none() {
        return Err(format!("{id} has no capability {:?}", T::NAME));
    }
    if origin == Origin::Client && !T::SETTABLE {
        return Err(format!("{:?} is read-only: its values come from the device", T::NAME));
    }
    let new: T = serde_json::from_value(value).map_err(|e| format!("invalid {} value: {e}", T::NAME))?;
    new.check()?;
    *slot = Some(new);
    Ok(())
}

impl Capabilities {
    /* Checks every capability that's present, and that there is at least
     * one (a device that can't do anything can't be shown or used). */
    pub fn check(&self) -> Result<(), String> {
        let mut any = false;
        if let Some(c) = &self.switch {
            c.check()?;
            any = true;
        }
        if let Some(c) = &self.dimmer {
            c.check()?;
            any = true;
        }
        if let Some(c) = &self.color {
            c.check()?;
            any = true;
        }
        if let Some(c) = &self.sensor {
            c.check()?;
            any = true;
        }
        if any {
            Ok(())
        } else {
            Err("a device needs at least one capability".into())
        }
    }
}

impl Device {
    /* Everything about a device that must hold before it's stored:
     * used for new devices and for devices loaded from disk. */
    pub fn check(&self) -> Result<(), String> {
        if !shadow::valid_name(&self.id) {
            return Err(format!(
                "invalid device id {:?}: use 1-64 letters, digits, '-', '_' or ':'",
                self.id
            ));
        }
        check_text("name", &self.name, 1)?;
        check_text("room", &self.room, 0)?;
        check_text("template", &self.template, 0)?;
        self.capabilities.check()
    }
}

/* Names and rooms: shown on screens and in the cloud, so no control
 * characters (line breaks, escape codes) and a sane length. */
fn check_text(what: &str, text: &str, min: usize) -> Result<(), String> {
    let chars = text.chars().count();
    if chars < min || chars > 64 {
        return Err(format!("{what} must be {min}-64 characters"));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("{what} can't contain control characters"));
    }
    Ok(())
}

/* ------------------------------------------------------------------ */
/* The old format (registry schema 1, before #34)                      */
/* ------------------------------------------------------------------ */

/* Turns a device stored as a property bag into a Device. The mapping:
 *   "on": true/false                  -> switch
 *   "brightness" or "level": 0..100   -> dimmer
 * The device keeps its id; its name becomes the id (rename it afterwards);
 * source "virtual" (hardware was never in the old registry). Returns the
 * device and the property names that had no meaning and were dropped, for
 * the log -- or Err if nothing usable was left. */
pub fn migrate_v1(id: &str, properties: &HashMap<String, serde_json::Value>) -> Result<(Device, Vec<String>), String> {
    let mut caps = Capabilities::default();
    let mut dropped = Vec::new();
    /* Sorted, so the dropped list (and the log) is always in the same order. */
    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort();
    for key in keys {
        let value = &properties[key];
        match (key.as_str(), value) {
            ("on", serde_json::Value::Bool(on)) => caps.switch = Some(Switch { on: *on }),
            ("brightness" | "level", v) if v.as_f64().is_some_and(|l| (0.0..=100.0).contains(&l)) => {
                /* 0.0..=100.0 was checked just above, so this fits a u8. */
                caps.dimmer = Some(Dimmer {
                    level: v.as_f64().unwrap_or(0.0).round() as u8,
                })
            }
            _ => dropped.push(key.clone()),
        }
    }
    let device = Device {
        id: id.to_string(),
        name: id.to_string(),
        room: String::new(),
        template: "migrated".into(),
        source: Source::Virtual,
        capabilities: caps,
    };
    device.check().map_err(|e| format!("{id}: {e} (had: {})", dropped.join(", ")))?;
    Ok((device, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lamp() -> Device {
        Device {
            id: "lamp-1".into(),
            name: "Living room lamp".into(),
            room: "Living room".into(),
            template: "dimmable-light".into(),
            source: Source::Virtual,
            capabilities: Capabilities {
                switch: Some(Switch { on: true }),
                dimmer: Some(Dimmer { level: 80 }),
                ..Default::default()
            },
        }
    }

    #[test]
    fn json_shape() {
        let text = serde_json::to_value(lamp()).unwrap();
        assert_eq!(
            text,
            json!({
                "id": "lamp-1", "name": "Living room lamp", "room": "Living room",
                "template": "dimmable-light", "source": "virtual",
                "capabilities": {"switch": {"on": true}, "dimmer": {"level": 80}}
            })
        );
        /* And back: optional fields may be left out. */
        let parsed: Device = serde_json::from_value(json!({
            "id": "t1", "name": "T", "capabilities": {"sensor": {"readings": {"temperature": {"value": 21.5, "unit": "°C"}}}}
        }))
        .unwrap();
        assert_eq!(parsed.source, Source::Virtual);
        assert!(parsed.check().is_ok());
    }

    #[test]
    fn unknown_capabilities_and_fields_are_rejected_by_serde() {
        assert!(serde_json::from_value::<Capabilities>(json!({"colour": {"hex": "#fff"}})).is_err());
        assert!(serde_json::from_value::<Capabilities>(json!({"switch": {"on": true, "extra": 1}})).is_err());
    }

    #[test]
    fn set_applies_valid_values() {
        let caps = set_capability(&lamp(), "dimmer", json!({"level": 30}), Origin::Client).unwrap();
        assert_eq!(caps.dimmer, Some(Dimmer { level: 30 }));
        /* The other capability is untouched. */
        assert_eq!(caps.switch, Some(Switch { on: true }));
    }

    #[test]
    fn set_rejects_with_clear_messages() {
        let d = lamp();
        let err = |cap: &str, v: serde_json::Value| set_capability(&d, cap, v, Origin::Client).unwrap_err();
        assert_eq!(err("color", json!({"hex": "#FF0000"})), "lamp-1 has no capability \"color\"");
        assert!(err("colour", json!({})).starts_with("unknown capability \"colour\""));
        assert_eq!(err("dimmer", json!({"level": 101})), "dimmer level must be 0-100, got 101");
        assert!(err("dimmer", json!({"level": -1})).starts_with("invalid dimmer value"));
        assert!(err("switch", json!({"on": "yes"})).starts_with("invalid switch value"));
        assert!(err("switch", json!({"on": true, "brightness": 5})).starts_with("invalid switch value"));
    }

    #[test]
    fn color_rules() {
        let ok = |c: Color| c.check().is_ok();
        assert!(ok(Color { hex: Some("#ff8800".into()), kelvin: None }));
        assert!(ok(Color { hex: None, kelvin: Some(2700) }));
        assert!(!ok(Color { hex: Some("ff8800".into()), kelvin: None }));
        assert!(!ok(Color { hex: Some("#ff88".into()), kelvin: None }));
        assert!(!ok(Color { hex: None, kelvin: Some(500) }));
        assert!(!ok(Color { hex: Some("#ff8800".into()), kelvin: Some(2700) }));
        assert!(!ok(Color { hex: None, kelvin: None }));
    }

    #[test]
    fn sensors_are_read_only_for_clients() {
        let mut d = lamp();
        d.capabilities.sensor = Some(Sensor::default());
        let reading = json!({"readings": {"temperature": {"value": 21.5, "unit": "°C"}}});
        assert!(set_capability(&d, "sensor", reading.clone(), Origin::Client)
            .unwrap_err()
            .contains("read-only"));
        let caps = set_capability(&d, "sensor", reading, Origin::Device).unwrap();
        assert_eq!(caps.sensor.unwrap().readings["temperature"].value, 21.5);
    }

    #[test]
    fn device_checks() {
        assert!(lamp().check().is_ok());
        let mut d = lamp();
        d.id = "bad/id".into();
        assert!(d.check().unwrap_err().contains("invalid device id"));
        let mut d = lamp();
        d.name = String::new();
        assert!(d.check().is_err());
        let mut d = lamp();
        d.room = "line\nbreak".into();
        assert!(d.check().unwrap_err().contains("control characters"));
        let mut d = lamp();
        d.capabilities = Capabilities::default();
        assert_eq!(d.check().unwrap_err(), "a device needs at least one capability");
    }

    #[test]
    fn old_devices_are_migrated() {
        let old = HashMap::from([
            ("on".to_string(), json!(true)),
            ("brightness".to_string(), json!(80)),
            ("speed".to_string(), json!(2)),
        ]);
        let (device, dropped) = migrate_v1("lamp-1", &old).unwrap();
        assert_eq!(device.name, "lamp-1");
        assert_eq!(device.capabilities.switch, Some(Switch { on: true }));
        assert_eq!(device.capabilities.dimmer, Some(Dimmer { level: 80 }));
        assert_eq!(dropped, vec!["speed".to_string()]);

        /* A value out of range isn't guessed at: dropped. */
        let old = HashMap::from([("on".to_string(), json!(false)), ("level".to_string(), json!(250))]);
        let (device, dropped) = migrate_v1("tv", &old).unwrap();
        assert_eq!(device.capabilities.dimmer, None);
        assert_eq!(dropped, vec!["level".to_string()]);

        /* Nothing usable left: not migrated, with the reason. */
        let old = HashMap::from([("speed".to_string(), json!(2))]);
        assert!(migrate_v1("fan", &old).unwrap_err().contains("at least one capability"));
    }
}
