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
 * every device type uses -- media (a TV's volume, mute and input, issue
 * #40), remote (#44), and cover, climate, lock and energy (issue #77:
 * blinds, air conditioners, door locks, metering plugs). The other specialised ones (vacuum, camera_stream,
 * ir_remote) come with their devices (#42-#45). Adding one
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
    /* The adapter's plain settings, from setup (issue #40): {"host":
     * "192.168.1.50", ...}. Never secrets (secrets.rs keeps those), never
     * sent to the cloud (shadow.rs lists what is). */
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
    /* What stays the same when its IP address changes (issue #40): a MAC,
     * serial or UUID, from its template's "identity". Empty = none: the
     * device is only known by its address. */
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub identity: String,
    /* Can the hub reach it right now (issue #40)? Set by its adapter's
     * task; None = not known (yet), e.g. virtual devices, or hardware
     * whose task hasn't connected yet. It only describes THIS run of the
     * hub: never saved in the registry (state::encode_registry drops it),
     * and never taken from a client or a file (skip_deserializing). */
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub online: Option<Health>,
    pub capabilities: Capabilities,
}

/* A hardware device's reachability, as its adapter sees it. A screen
 * shows anything but Online greyed out, with the reason. */
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Online,
    /* Not answering: switched off, unplugged, left the WiFi. */
    Offline,
    /* Answering, but refusing us: e.g. a TV whose pairing key was
     * revoked -- it needs pairing again (the wizard's re-auth). */
    #[allow(dead_code)] /* first used by the LG TV adapter (#40 step 7) */
    Unauthorized,
}

/* Where a device's truth is, as the id of the ADAPTER that runs it
 * (issue #40, adapters/): "m4-led", "wled", "lg-webos", ... -- or
 * "virtual". For a VIRTUAL device the hub's own record is the truth (test
 * devices, and devices whose real driver doesn't exist yet). For hardware,
 * the hub only reports what the hardware confirms: a command goes to the
 * device's adapter first, and the state changes when it answers.
 *
 * Until #40 this was a fixed list (virtual, m4_led); registries and
 * clients from then still work: "m4_led" is read as "m4-led". */
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct Source(String);

/* Adapters whose devices are part of the hub itself (templates with
 * "builtin": true): always there, can't be removed (deleting the cloud
 * shadow just recreates them). A test checks this matches the templates. */
pub const BUILTIN_SOURCES: [&str; 1] = ["m4-led"];

impl Source {
    pub const VIRTUAL: &'static str = "virtual";

    pub fn new(adapter: &str) -> Self {
        Source(adapter.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_virtual(&self) -> bool {
        self.0 == Self::VIRTUAL
    }

    pub fn is_builtin(&self) -> bool {
        BUILTIN_SOURCES.contains(&self.0.as_str())
    }
}

impl Default for Source {
    fn default() -> Self {
        Source::new(Self::VIRTUAL)
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/* Hand-written instead of derived: to read the pre-#40 names. */
impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(Source(match name.as_str() {
            "m4_led" => "m4-led".to_string(),
            _ => name,
        }))
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<Cover>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub climate: Option<Climate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<Lock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy: Option<Energy>,
}

/* The names, e.g. for error messages and the protocol's "hello". */
pub const CAPABILITY_NAMES: [&str; 10] = [
    "switch", "dimmer", "color", "sensor", "media", "remote", "cover", "climate", "lock", "energy",
];

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

/* A TV's (later: a speaker's, a receiver's) sound and source:
 *   {"volume": 12, "muted": false, "input": "HDMI_1",
 *    "inputs": [{"id": "HDMI_1", "label": "PlayStation"}, ...]}
 * Power is the device's `switch`. `input` is "" while no input is shown
 * (an app, e.g. Netflix). `inputs` is what the device offers -- it only
 * comes FROM the device: in a command it's ignored (and may be left out),
 * a client sends volume, muted and input.
 *
 * Issue #44, also only FROM the device (ignored in commands): `app`, what's
 * on screen ({"id": "netflix", "label": "Netflix"}), and `channel`, the
 * channel while watching TV. The LISTS of apps and channels can be long
 * (hundreds of channels): not part of the state -- which is sent on every
 * change and kept in the cloud shadow -- but asked for with the actions
 * "apps" / "channels" (see check_action), and chosen with "launch" /
 * "tune". */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Media {
    pub volume: u8,
    pub muted: bool,
    #[serde(default)]
    pub input: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<MediaInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<MediaInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<Channel>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /* The device's own id for it (what "tune" takes). */
    pub id: String,
    /* As the remote shows it: "5", "7-1". */
    pub number: String,
    pub name: String,
}

/* A remote control's buttons (issue #44): TVs, and the IR blaster (#42).
 * There's no state to set -- a button is PRESSED, an action (see
 * check_action) -- so the state only says which buttons this device has,
 * and only the device reports it.
 *
 * Two kinds of button names:
 *   - the hub's own (REMOTE_BUTTONS), the same for every brand: a TV's
 *     adapter translates them, and a screen can lay them out as a real
 *     remote (arrows around OK, ...);
 *   - on a remote that LEARNS (an IR blaster), also names the person gave
 *     when teaching a button ("Power", "Red", "Brighter"): a screen shows
 *     those as a grid of labelled buttons. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub buttons: Vec<String>,
    /* Text can be typed into the device (a TV's on-screen keyboard). */
    #[serde(default)]
    pub keyboard: bool,
    /* Buttons are taught by pressing them on the original remote (the
     * learn / forget / rename actions), and may have any name. */
    #[serde(default)]
    pub learn: bool,
}

/* The most buttons one remote may have (a big TV remote has ~50). */
pub const MAX_REMOTE_BUTTONS: usize = 100;

/* A taught button's name: what fits on a button on a phone screen. */
const MAX_BUTTON_NAME: usize = 24;

/* A name for a taught button: 1-24 characters, no control characters,
 * no spaces around it (so "Power" and "Power " can't both exist). */
pub fn valid_button_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name.chars().count() <= MAX_BUTTON_NAME
        && !name.chars().any(char::is_control)
}

/* Every button name a remote may have. The first rows are what a screen
 * lays out as the remote itself; the digits and colours are extras. */
pub const REMOTE_BUTTONS: [&str; 34] = [
    "UP", "DOWN", "LEFT", "RIGHT", "OK", "BACK", "HOME", "MENU", "EXIT", "INFO",
    "VOLUME_UP", "VOLUME_DOWN", "MUTE", "CHANNEL_UP", "CHANNEL_DOWN",
    "PLAY", "PAUSE", "STOP", "REWIND", "FAST_FORWARD",
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
    "RED", "GREEN", "YELLOW", "BLUE",
];

/* ---- Issue #77 ---- */

/* Blinds, shutters, curtains, garage doors:
 *   {"position": 40, "moving": "stopped", "can_position": true}
 * `position`: 0 = closed, 100 = fully open; null = not known (a motor that
 * doesn't report it). `moving` and `can_position` only come FROM the
 * device: a client sends {"position": N} -- and only to a device that can
 * go to any position (can_position); others (a garage door) are only
 * opened, closed and stopped, with the ACTIONS open / close / stop (see
 * check_action), which every cover has. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cover {
    #[serde(default)]
    pub position: Option<u8>,
    #[serde(default)]
    pub moving: Moving,
    #[serde(default)]
    pub can_position: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Moving {
    Opening,
    Closing,
    #[default]
    Stopped,
}

/* Air conditioners, heat pumps, thermostats:
 *   {"mode": "heat", "target": 21.5, "current": 22.8, "fan": "auto",
 *    "modes": ["off", "heat", "cool", "auto"], "fans": ["auto", "low"],
 *    "min": 16, "max": 30, "step": 0.5}
 * A client sends mode, target and fan; the rest is what the DEVICE
 * offers (like a TV's inputs): its modes and fan speeds, its temperature
 * range and step, and the room temperature it measures (`current`, null
 * if it doesn't). A command is checked against what the device offers. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Climate {
    pub mode: String,
    /* °C. */
    pub target: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    /* null: the device has no fan setting. */
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan: Option<String>,
    #[serde(default = "Climate::default_modes")]
    pub modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fans: Vec<String>,
    #[serde(default = "Climate::default_min")]
    pub min: f64,
    #[serde(default = "Climate::default_max")]
    pub max: f64,
    #[serde(default = "Climate::default_step")]
    pub step: f64,
}

/* The modes and fan speeds the hub knows. A device offers some of them. */
pub const CLIMATE_MODES: [&str; 6] = ["off", "heat", "cool", "auto", "dry", "fan"];
pub const CLIMATE_FANS: [&str; 6] = ["auto", "quiet", "low", "medium", "high", "turbo"];

impl Climate {
    fn default_modes() -> Vec<String> {
        ["off", "heat", "cool", "auto"].map(String::from).to_vec()
    }
    fn default_min() -> f64 {
        16.0
    }
    fn default_max() -> f64 {
        30.0
    }
    fn default_step() -> f64 {
        0.5
    }
}

impl Default for Climate {
    fn default() -> Self {
        Climate {
            mode: "off".into(),
            target: 21.0,
            current: None,
            fan: None,
            modes: Climate::default_modes(),
            fans: Vec::new(),
            min: Climate::default_min(),
            max: Climate::default_max(),
            step: Climate::default_step(),
        }
    }
}

/* Door locks: {"state": "locked"}. "jammed" (it tried, something's in the
 * way) and "unknown" only come from the device; a client sends "locked" or
 * "unlocked".
 *
 * THE UNLOCK RULE (issue #77): opening a door must never happen by
 * accident -- a stray tap, a confused automation, a replayed cloud
 * message. So a command that UNLOCKS must say {"state": "unlocked",
 * "confirmed": true}: every client asks the person first ("Unlock Front
 * door?") and only then sends it; anything that doesn't is refused. An
 * automation (#47) may only unlock where the person allowed it for that
 * lock. `confirmed` belongs to the command only, never to the state. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub state: LockState,
    #[serde(default, skip_serializing)]
    pub confirmed: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LockState {
    Locked,
    Unlocked,
    Jammed,
    Unknown,
}

/* What a metering plug, an energy meter or an inverter measures:
 *   {"power_w": 40.2, "energy_kwh": 1.234, "voltage_v": 230.1, "current_a": 0.18}
 * Only power is required (a device reporting nothing else still shows
 * what it draws). Negative power is allowed: an inverter or a meter
 * measuring what goes back to the grid. Only the device reports it. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Energy {
    pub power_w: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_kwh: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voltage_v: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_a: Option<f64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MediaInput {
    pub id: String,
    pub label: String,
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
    /* A CLIENT's command, before it replaces the state (issue #77): the
     * parts only the device reports (a climate's modes and range, a
     * cover's `moving`) are kept from `current`, and rules about the
     * change itself (the unlock rule) are checked. Default: the command
     * is the new state. */
    fn from_client(current: &Self, new: Self) -> Result<Self, String>
    where
        Self: Sized,
    {
        let _ = current;
        Ok(new)
    }
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

impl Capability for Media {
    const NAME: &'static str = "media";
    fn check(&self) -> Result<(), String> {
        if self.volume > 100 {
            return Err(format!("media volume must be 0-100, got {}", self.volume));
        }
        let text_ok = |t: &str| t.chars().count() <= 64 && !t.chars().any(char::is_control);
        if !text_ok(&self.input) {
            return Err("media input: at most 64 characters, no control characters".into());
        }
        if self.inputs.len() > 32 || !self.inputs.iter().all(|i| text_ok(&i.id) && text_ok(&i.label)) {
            return Err("media inputs: at most 32, each id and label at most 64 characters".into());
        }
        if let Some(app) = &self.app {
            if !text_ok(&app.id) || !text_ok(&app.label) {
                return Err("media app: id and label at most 64 characters".into());
            }
        }
        if let Some(c) = &self.channel {
            if !text_ok(&c.id) || !text_ok(&c.number) || !text_ok(&c.name) {
                return Err("media channel: id, number and name at most 64 characters".into());
            }
        }
        Ok(())
    }
}

impl Capability for Remote {
    const NAME: &'static str = "remote";
    /* Buttons are pressed (an action), not set. */
    const SETTABLE: bool = false;
    fn check(&self) -> Result<(), String> {
        if self.buttons.len() > MAX_REMOTE_BUTTONS {
            return Err(format!("a remote has at most {MAX_REMOTE_BUTTONS} buttons"));
        }
        for (i, button) in self.buttons.iter().enumerate() {
            let known = REMOTE_BUTTONS.contains(&button.as_str());
            if !known && !(self.learn && valid_button_name(button)) {
                return Err(format!("unknown remote button {button:?}"));
            }
            if self.buttons[..i].contains(button) {
                return Err(format!("remote button {button:?} listed twice"));
            }
        }
        Ok(())
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

impl Capability for Cover {
    const NAME: &'static str = "cover";
    fn check(&self) -> Result<(), String> {
        match self.position {
            Some(p) if p > 100 => Err(format!("cover position must be 0-100, got {p}")),
            _ => Ok(()),
        }
    }
    fn from_client(current: &Self, new: Self) -> Result<Self, String> {
        let Some(position) = new.position else {
            return Err("cover: send {\"position\": 0-100}, or use the actions open, close, stop".into());
        };
        if !current.can_position {
            return Err("this cover can't go to a position: use the actions open, close, stop".into());
        }
        Ok(Cover {
            position: Some(position),
            ..current.clone()
        })
    }
}

impl Capability for Climate {
    const NAME: &'static str = "climate";
    fn check(&self) -> Result<(), String> {
        let known = |list: &[&str], name: &str| list.contains(&name);
        if self.modes.is_empty() || self.modes.len() > CLIMATE_MODES.len() {
            return Err("climate modes: 1-6 of off, heat, cool, auto, dry, fan".into());
        }
        for (i, mode) in self.modes.iter().enumerate() {
            if !known(&CLIMATE_MODES, mode) || self.modes[..i].contains(mode) {
                return Err(format!("climate modes: {mode:?} unknown or listed twice"));
            }
        }
        if !self.modes.contains(&self.mode) {
            return Err(format!("climate mode {:?}: this device offers {}", self.mode, self.modes.join(", ")));
        }
        for (i, fan) in self.fans.iter().enumerate() {
            if !known(&CLIMATE_FANS, fan) || self.fans[..i].contains(fan) {
                return Err(format!("climate fans: {fan:?} unknown or listed twice ({})", CLIMATE_FANS.join(", ")));
            }
        }
        match &self.fan {
            Some(fan) if !self.fans.contains(fan) => {
                return Err(format!("climate fan {fan:?}: this device offers {}", self.fans.join(", ")));
            }
            None if !self.fans.is_empty() => return Err("climate: a device with fan speeds reports one".into()),
            _ => {}
        }
        let all_finite = [self.target, self.min, self.max, self.step].iter().all(|v| v.is_finite())
            && self.current.is_none_or(f64::is_finite);
        if !all_finite || !(-50.0..=100.0).contains(&self.min) || !(-50.0..=100.0).contains(&self.max) || self.min >= self.max {
            return Err("climate: min and max must be numbers from -50 to 100, min below max".into());
        }
        if !(0.1..=5.0).contains(&self.step) {
            return Err("climate step must be 0.1-5".into());
        }
        if !(self.min..=self.max).contains(&self.target) {
            return Err(format!("climate target must be {}-{} °C, got {}", self.min, self.max, self.target));
        }
        Ok(())
    }
    fn from_client(current: &Self, new: Self) -> Result<Self, String> {
        Ok(Climate {
            mode: new.mode,
            target: new.target,
            /* Not every client knows the fan: left out = unchanged. */
            fan: new.fan.or_else(|| current.fan.clone()),
            ..current.clone()
        })
    }
}

impl Capability for Lock {
    const NAME: &'static str = "lock";
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
    fn from_client(current: &Self, new: Self) -> Result<Self, String> {
        match new.state {
            LockState::Locked => {}
            LockState::Unlocked if new.confirmed || current.state == LockState::Unlocked => {}
            LockState::Unlocked => {
                return Err("unlocking needs a confirmation: ask the person, then send \"confirmed\": true".into())
            }
            LockState::Jammed | LockState::Unknown => return Err("a lock can only be set to locked or unlocked".into()),
        }
        Ok(Lock {
            state: new.state,
            confirmed: false,
        })
    }
}

impl Capability for Energy {
    const NAME: &'static str = "energy";
    const SETTABLE: bool = false;
    fn check(&self) -> Result<(), String> {
        let ok = |v: Option<f64>| v.is_none_or(|v| v.is_finite() && v >= 0.0);
        if !self.power_w.is_finite() || !ok(self.energy_kwh) || !ok(self.voltage_v) || !ok(self.current_a) {
            return Err("energy: numbers only, and only power may be negative".into());
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
        "media" => replace(&mut caps.media, id, value, origin)?,
        "remote" => replace(&mut caps.remote, id, value, origin)?,
        "cover" => replace(&mut caps.cover, id, value, origin)?,
        "climate" => replace(&mut caps.climate, id, value, origin)?,
        "lock" => replace(&mut caps.lock, id, value, origin)?,
        "energy" => replace(&mut caps.energy, id, value, origin)?,
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
    let mut new: T = serde_json::from_value(value).map_err(|e| format!("invalid {} value: {e}", T::NAME))?;
    if let (Origin::Client, Some(current)) = (origin, slot.as_ref()) {
        new = T::from_client(current, new)?;
    }
    new.check()?;
    *slot = Some(new);
    Ok(())
}

/* ------------------------------------------------------------------ */
/* Actions (issue #44)                                                 */
/* ------------------------------------------------------------------ */

/* An ACTION is a one-off request that changes no state by itself (it may
 * lead to a state change the device then reports): press a remote's
 * button, type text, list a TV's channels, launch an app. Later also a
 * vacuum's "dock", a blind's "stop". Like set_capability for commands,
 * this is the one place that says which actions exist and what their
 * arguments must look like -- control.rs checks every action here before
 * the device's adapter sees it, whoever sent it.
 *
 *   remote  press {"button": "UP"}          one of the device's buttons
 *           type {"text": "netflix"}        into the active text field
 *           delete {"count": 1}             characters before the cursor
 *           submit {}                       the keyboard's Enter
 *     on a remote that learns (IR blaster, #42):
 *           learn {"button": "Power", "timeout_s"?: 5-60}
 *                -> {"code": {...}} once the original remote's button was
 *                   pressed; a new name adds a button, an existing one is
 *                   taught again
 *           forget {"button": "Power"}
 *           rename {"button": "Power", "to": "On/Off"}
 *     and the IR code library (#82, ir_library.rs), for a lost remote:
 *           library {}            -> {"types": [{"id", "name"}]}
 *           library {"type": "tv"} -> {"brands": [{"name", "sets"}]}
 *           finder {"type", "brand"?}
 *                -> {"candidates": [{"button", "sets": [{"id", "name",
 *                   "brand", "button", "check"}]}]}  sets grouped by the
 *                   code of their test button, most common first
 *           try {"type", "set", "button"}   send one button of a set
 *           use_set {"type", "set"} -> {"added": N}  copy its buttons
 *   media   apps {}      -> {"apps": [{"id", "label"}]}
 *           launch {"app": "<id>"}
 *           channels {"query"?, "offset"?, "limit"?}
 *                -> {"channels": [{"id", "number", "name"}], "total": N}
 *                   one page of the matching channels (adapters/channels.rs)
 *           tune {"channel": "<id>"}
 *   cover   open {}  close {}  stop {}       (issue #77)                */
pub fn check_action(device: &Device, capability: &str, name: &str, args: &serde_json::Value) -> Result<(), String> {
    let id = &device.id;
    let caps = &device.capabilities;
    let no_args = || match args {
        serde_json::Value::Null => Ok(()),
        serde_json::Value::Object(o) if o.is_empty() => Ok(()),
        _ => Err(format!("{capability} {name} takes no arguments")),
    };
    let text_arg = |key: &str, max: usize| -> Result<(), String> {
        let text = args[key].as_str().ok_or_else(|| format!("{capability} {name} needs {{\"{key}\": \"...\"}}"))?;
        if text.is_empty() || text.chars().count() > max || text.chars().any(char::is_control) {
            return Err(format!("{capability} {name}: {key} must be 1-{max} characters, no control characters"));
        }
        Ok(())
    };
    /* An IR library argument: text, 1-max characters, no control
     * characters (an empty brand means "any brand"). */
    let library_arg = |key: &str, max: usize| -> Result<(), String> {
        let text = args[key].as_str().ok_or_else(|| format!("{capability} {name} needs {{\"{key}\": \"...\"}}"))?;
        let empty_ok = key == "brand";
        if (text.is_empty() && !empty_ok) || text.chars().count() > max || text.chars().any(char::is_control) {
            return Err(format!("{capability} {name}: {key} must be 1-{max} characters, no control characters"));
        }
        Ok(())
    };
    match capability {
        "remote" => {
            let remote = caps.remote.as_ref().ok_or_else(|| format!("{id} has no capability \"remote\""))?;
            match name {
                "press" => {
                    let button = args["button"].as_str().ok_or("remote press needs {\"button\": \"UP\"}")?;
                    if !remote.buttons.iter().any(|b| b == button) {
                        return Err(format!("{id} has no button {button:?}"));
                    }
                    Ok(())
                }
                "type" | "delete" | "submit" if !remote.keyboard => Err(format!("{id} can't take typed text")),
                "type" => text_arg("text", 256),
                "delete" => match args["count"].as_u64() {
                    Some(1..=256) => Ok(()),
                    _ => Err("remote delete needs {\"count\": 1-256}".into()),
                },
                "submit" => no_args(),
                "learn" | "forget" | "rename" | "library" | "finder" | "try" | "use_set" if !remote.learn => {
                    Err(format!("{id} can't learn buttons"))
                }
                /* The IR code library (issue #82, ir_library.rs). Only the
                 * shape is checked here; the library itself says whether a
                 * type, brand or set exists. */
                "library" => match args.get("type") {
                    None => no_args(),
                    Some(_) => library_arg("type", 32),
                },
                "finder" => {
                    library_arg("type", 32)?;
                    match args.get("brand") {
                        None => Ok(()),
                        Some(_) => library_arg("brand", 64),
                    }
                }
                "try" => {
                    library_arg("type", 32)?;
                    library_arg("set", 200)?;
                    library_arg("button", 64)
                }
                "use_set" => {
                    library_arg("type", 32)?;
                    library_arg("set", 200)
                }
                "learn" => {
                    let button = args["button"].as_str().ok_or("remote learn needs {\"button\": \"Power\"}")?;
                    if !valid_button_name(button) {
                        return Err(format!("button names: 1-{MAX_BUTTON_NAME} characters, no spaces around them"));
                    }
                    if !remote.buttons.iter().any(|b| b == button) && remote.buttons.len() >= MAX_REMOTE_BUTTONS {
                        return Err(format!("{id} has {MAX_REMOTE_BUTTONS} buttons already"));
                    }
                    match args.get("timeout_s") {
                        None => Ok(()),
                        Some(t) if t.as_u64().is_some_and(|t| (5..=60).contains(&t)) => Ok(()),
                        Some(_) => Err("remote learn: timeout_s must be 5-60".into()),
                    }
                }
                "forget" | "rename" => {
                    let button = args["button"].as_str().ok_or_else(|| format!("remote {name} needs {{\"button\": \"...\"}}"))?;
                    if !remote.buttons.iter().any(|b| b == button) {
                        return Err(format!("{id} has no button {button:?}"));
                    }
                    if name == "rename" {
                        let to = args["to"].as_str().ok_or("remote rename needs {\"to\": \"new name\"}")?;
                        if !valid_button_name(to) {
                            return Err(format!("button names: 1-{MAX_BUTTON_NAME} characters, no spaces around them"));
                        }
                        if to != button && remote.buttons.iter().any(|b| b == to) {
                            return Err(format!("{id} has a button {to:?} already"));
                        }
                    }
                    Ok(())
                }
                other => Err(format!(
                    "remote has no action {other:?} (press, type, delete, submit, learn, forget, rename, library, finder, try, use_set)"
                )),
            }
        }
        "media" => {
            if caps.media.is_none() {
                return Err(format!("{id} has no capability \"media\""));
            }
            match name {
                "apps" => no_args(),
                "channels" => {
                    if let Some(query) = args.get("query") {
                        let query = query.as_str().ok_or("media channels: query must be text")?;
                        if query.chars().count() > 64 || query.chars().any(char::is_control) {
                            return Err("media channels: query at most 64 characters".into());
                        }
                    }
                    for key in ["offset", "limit"] {
                        if args.get(key).is_some_and(|v| v.as_u64().is_none()) {
                            return Err(format!("media channels: {key} must be a whole number"));
                        }
                    }
                    if args.get("limit").and_then(|v| v.as_u64()) == Some(0) {
                        return Err("media channels: limit must be at least 1".into());
                    }
                    Ok(())
                }
                "launch" => text_arg("app", 128),
                "tune" => text_arg("channel", 128),
                other => Err(format!("media has no action {other:?} (apps, launch, channels, tune)")),
            }
        }
        "cover" => {
            if caps.cover.is_none() {
                return Err(format!("{id} has no capability \"cover\""));
            }
            match name {
                "open" | "close" | "stop" => no_args(),
                other => Err(format!("cover has no action {other:?} (open, close, stop)")),
            }
        }
        other if CAPABILITY_NAMES.contains(&other) => Err(format!("{other} has no actions")),
        other => Err(format!("unknown capability {other:?}")),
    }
}

impl Capabilities {
    /* A new device's capabilities, from its template's list of names:
     * each with a neutral starting value (off, full brightness, white).
     * Its adapter reports the real state as soon as it's connected. */
    pub fn with_defaults(names: &[String]) -> Result<Capabilities, String> {
        let mut caps = Capabilities::default();
        for name in names {
            caps.add_default(name)?;
        }
        caps.check()?;
        Ok(caps)
    }

    /* Adds one capability with its neutral starting value, if missing;
     * true if it was added. Also for devices created before their
     * template gained a capability (issue #44: TVs added in #40 get
     * `remote`, see state::Msg::AddMissingCapabilities). */
    pub fn add_default(&mut self, name: &str) -> Result<bool, String> {
        fn fill<T>(slot: &mut Option<T>, value: T) -> bool {
            if slot.is_some() {
                return false;
            }
            *slot = Some(value);
            true
        }
        Ok(match name {
            "switch" => fill(&mut self.switch, Switch { on: false }),
            "dimmer" => fill(&mut self.dimmer, Dimmer { level: 100 }),
            "color" => fill(
                &mut self.color,
                Color {
                    hex: Some("#FFFFFF".into()),
                    kelvin: None,
                },
            ),
            "sensor" => fill(&mut self.sensor, Sensor::default()),
            "media" => fill(&mut self.media, Media::default()),
            "remote" => fill(&mut self.remote, Remote::default()),
            /* Not known yet: position null, until the device says. */
            "cover" => fill(
                &mut self.cover,
                Cover {
                    position: None,
                    moving: Moving::Stopped,
                    can_position: false,
                },
            ),
            "climate" => fill(&mut self.climate, Climate::default()),
            "lock" => fill(
                &mut self.lock,
                Lock {
                    state: LockState::Unknown,
                    confirmed: false,
                },
            ),
            "energy" => fill(&mut self.energy, Energy::default()),
            other => return Err(format!("unknown capability {other:?}")),
        })
    }

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
        if let Some(c) = &self.media {
            c.check()?;
            any = true;
        }
        if let Some(c) = &self.remote {
            c.check()?;
            any = true;
        }
        if let Some(c) = &self.cover {
            c.check()?;
            any = true;
        }
        if let Some(c) = &self.climate {
            c.check()?;
            any = true;
        }
        if let Some(c) = &self.lock {
            c.check()?;
            any = true;
        }
        if let Some(c) = &self.energy {
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
        for (key, value) in &self.config {
            if key.is_empty() || key.len() > 40 || !key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                return Err(format!("config key {key:?}: use a-z, 0-9 and _"));
            }
            if value.len() > 256 || value.chars().any(char::is_control) {
                return Err(format!("config {key:?}: at most 256 characters, no control characters"));
            }
        }
        if self.identity.len() > 128 || self.identity.chars().any(char::is_control) {
            return Err("identity: at most 128 characters, no control characters".into());
        }
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
        source: Source::default(),
        config: Default::default(),
        identity: String::new(),
        online: None,
        capabilities: caps,
    };
    device.check().map_err(|e| format!("{id}: {e} (had: {})", dropped.join(", ")))?;
    Ok((device, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /* ---- Issue #77 ---- */

    fn with(caps: serde_json::Value) -> Device {
        serde_json::from_value(json!({"id": "d", "name": "D", "capabilities": caps})).unwrap()
    }

    #[test]
    fn covers_move_to_positions_only_if_they_can() {
        let blind = with(json!({"cover": {"position": 0, "moving": "stopped", "can_position": true}}));
        let caps = set_capability(&blind, "cover", json!({"position": 40}), Origin::Client).unwrap();
        /* What the device reports is kept. */
        assert_eq!(caps.cover.unwrap(), Cover { position: Some(40), moving: Moving::Stopped, can_position: true });
        assert!(set_capability(&blind, "cover", json!({"position": 101}), Origin::Client).is_err());
        assert!(set_capability(&blind, "cover", json!({}), Origin::Client).unwrap_err().contains("position"));
        /* A client can't claim it's moving, or make it positionable. */
        let caps = set_capability(&blind, "cover", json!({"position": 10, "moving": "opening", "can_position": false}), Origin::Client).unwrap();
        assert_eq!(caps.cover.unwrap(), Cover { position: Some(10), moving: Moving::Stopped, can_position: true });

        let garage = with(json!({"cover": {"position": 100}}));
        assert!(set_capability(&garage, "cover", json!({"position": 0}), Origin::Client).unwrap_err().contains("open, close, stop"));
        /* The device itself reports anything valid. */
        let caps = set_capability(&garage, "cover", json!({"position": null, "moving": "closing"}), Origin::Device).unwrap();
        assert_eq!(caps.cover.unwrap().moving, Moving::Closing);

        for action in ["open", "close", "stop"] {
            assert_eq!(check_action(&garage, "cover", action, &json!({})), Ok(()));
        }
        assert!(check_action(&garage, "cover", "tilt", &json!({})).is_err());
        assert!(check_action(&garage, "cover", "open", &json!({"fast": true})).is_err());
    }

    #[test]
    fn climate_commands_are_checked_against_what_the_device_offers() {
        let ac = with(json!({"climate": {"mode": "off", "target": 21, "current": 23.4, "fan": "auto",
            "modes": ["off", "cool", "dry"], "fans": ["auto", "low", "high"], "min": 17, "max": 30, "step": 1}}));
        let caps = set_capability(&ac, "climate", json!({"mode": "cool", "target": 24}), Origin::Client).unwrap();
        let c = caps.climate.unwrap();
        assert_eq!((c.mode.as_str(), c.target, c.fan.as_deref()), ("cool", 24.0, Some("auto")), "fan left out = unchanged");
        assert_eq!((c.current, c.min, c.step, c.modes.len()), (Some(23.4), 17.0, 1.0, 3), "the device's offer is kept");

        let err = |v| set_capability(&ac, "climate", v, Origin::Client).unwrap_err();
        assert!(err(json!({"mode": "heat", "target": 24})).contains("offers off, cool, dry"));
        assert!(err(json!({"mode": "cool", "target": 31})).contains("17-30"));
        assert!(err(json!({"mode": "cool", "target": 24, "fan": "turbo"})).contains("offers auto, low, high"));
        /* A client can't widen the range. */
        let c = set_capability(&ac, "climate", json!({"mode": "cool", "target": 20, "min": 5}), Origin::Client).unwrap().climate.unwrap();
        assert_eq!(c.min, 17.0);

        /* What a device may report. */
        let bad = |v| set_capability(&ac, "climate", v, Origin::Device).is_err();
        assert!(bad(json!({"mode": "cool", "target": 20, "modes": ["cool", "warp"]})));
        assert!(bad(json!({"mode": "cool", "target": 20, "modes": ["cool"], "min": 30, "max": 16})));
        assert!(bad(json!({"mode": "cool", "target": 20, "modes": ["cool"], "fans": ["low"]})), "fan speeds but no fan");
        assert!(!bad(json!({"mode": "cool", "target": 20, "modes": ["cool"]})));
    }

    #[test]
    fn unlocking_needs_a_confirmation() {
        let door = with(json!({"lock": {"state": "locked"}}));
        let err = set_capability(&door, "lock", json!({"state": "unlocked"}), Origin::Client).unwrap_err();
        assert!(err.contains("confirmation"), "{err}");
        let caps = set_capability(&door, "lock", json!({"state": "unlocked", "confirmed": true}), Origin::Client).unwrap();
        assert_eq!(caps.lock.as_ref().unwrap().state, LockState::Unlocked);
        /* "confirmed" is never part of the state. */
        assert_eq!(serde_json::to_value(&caps).unwrap(), json!({"lock": {"state": "unlocked"}}));
        /* Locking needs none; a client can't set "jammed". */
        assert!(set_capability(&door, "lock", json!({"state": "locked"}), Origin::Client).is_ok());
        assert!(set_capability(&door, "lock", json!({"state": "jammed"}), Origin::Client).is_err());
        /* The lock itself reports anything, and opened by hand needs no confirmation. */
        assert!(set_capability(&door, "lock", json!({"state": "unlocked"}), Origin::Device).is_ok());
        let open = with(json!({"lock": {"state": "unlocked"}}));
        assert!(set_capability(&open, "lock", json!({"state": "unlocked"}), Origin::Client).is_ok());
    }

    #[test]
    fn energy_is_read_only_and_checked() {
        let plug = with(json!({"energy": {"power_w": 0}}));
        assert!(set_capability(&plug, "energy", json!({"power_w": 5}), Origin::Client).unwrap_err().contains("read-only"));
        let caps = set_capability(&plug, "energy", json!({"power_w": -1200.5, "energy_kwh": 3.2}), Origin::Device).unwrap();
        assert_eq!(caps.energy.unwrap().power_w, -1200.5, "an inverter feeding the grid");
        assert!(set_capability(&plug, "energy", json!({"power_w": 1, "voltage_v": -230}), Origin::Device).is_err());
    }

    #[test]
    fn new_capabilities_have_defaults() {
        let names: Vec<String> = ["cover", "climate", "lock", "energy"].map(String::from).to_vec();
        let caps = Capabilities::with_defaults(&names).unwrap();
        assert_eq!(caps.cover.unwrap().position, None);
        assert_eq!(caps.climate.unwrap().mode, "off");
        assert_eq!(caps.lock.unwrap().state, LockState::Unknown);
        assert_eq!(caps.energy.unwrap().power_w, 0.0);
    }

    /* Issue #44: which actions exist, and their arguments. */
    #[test]
    fn actions_are_checked() {
        let mut tv = lamp();
        tv.capabilities = Capabilities::with_defaults(&["switch".into(), "media".into(), "remote".into()]).unwrap();
        tv.capabilities.remote = Some(Remote {
            buttons: vec!["UP".into(), "OK".into()],
            keyboard: true,
            learn: false,
        });
        let ok = |cap: &str, name: &str, args: serde_json::Value| check_action(&tv, cap, name, &args);
        assert_eq!(ok("remote", "press", json!({"button": "UP"})), Ok(()));
        assert!(ok("remote", "press", json!({"button": "HOME"})).unwrap_err().contains("no button"));
        assert!(ok("remote", "press", json!({})).is_err());
        assert_eq!(ok("remote", "type", json!({"text": "netflix"})), Ok(()));
        assert!(ok("remote", "type", json!({"text": "line\nbreak"})).is_err());
        assert!(ok("remote", "delete", json!({"count": 0})).is_err());
        assert_eq!(ok("remote", "submit", json!({})), Ok(()));
        assert!(ok("remote", "submit", json!({"x": 1})).is_err());
        assert_eq!(ok("media", "channels", serde_json::Value::Null), Ok(()));
        assert_eq!(ok("media", "tune", json!({"channel": "ch-5"})), Ok(()));
        assert!(ok("media", "tune", json!({})).is_err());
        assert!(ok("switch", "press", json!({})).unwrap_err().contains("no actions"));
        assert!(ok("toaster", "pop", json!({})).is_err());
        /* No keyboard: no typing. */
        tv.capabilities.remote.as_mut().unwrap().keyboard = false;
        assert!(check_action(&tv, "remote", "type", &json!({"text": "a"})).is_err());
        /* A lamp has no remote. */
        assert!(check_action(&lamp(), "remote", "press", &json!({"button": "UP"})).is_err());
        /* A TV's remote doesn't learn. */
        assert!(check_action(&tv, "remote", "learn", &json!({"button": "Power"})).unwrap_err().contains("can't learn"));
    }

    /* Issue #42: an IR blaster's remote learns buttons with any name. */
    #[test]
    fn learning_remote_actions_are_checked() {
        let mut ir = lamp();
        ir.capabilities.remote = Some(Remote {
            buttons: vec!["Power".into(), "Red".into()],
            keyboard: false,
            learn: true,
        });
        let ok = |name: &str, args: serde_json::Value| check_action(&ir, "remote", name, &args);
        assert_eq!(ok("press", json!({"button": "Power"})), Ok(()));
        assert_eq!(ok("learn", json!({"button": "Brighter"})), Ok(()));
        assert_eq!(ok("learn", json!({"button": "Power", "timeout_s": 30})), Ok(()));
        assert!(ok("learn", json!({"button": "Power", "timeout_s": 600})).is_err());
        assert!(ok("learn", json!({"button": " Power"})).is_err());
        assert!(ok("learn", json!({"button": ""})).is_err());
        assert!(ok("learn", json!({"button": "a very long button name, too long"})).is_err());
        assert_eq!(ok("forget", json!({"button": "Red"})), Ok(()));
        assert!(ok("forget", json!({"button": "Blue"})).is_err());
        assert_eq!(ok("rename", json!({"button": "Red", "to": "Colour"})), Ok(()));
        assert!(ok("rename", json!({"button": "Red", "to": "Power"})).unwrap_err().contains("already"));
        assert!(ok("type", json!({"text": "a"})).is_err());
        /* Issue #82: the code library's actions. */
        assert_eq!(ok("library", json!({})), Ok(()));
        assert_eq!(ok("library", json!({"type": "tv"})), Ok(()));
        assert!(ok("library", json!({"type": ""})).is_err());
        assert_eq!(ok("finder", json!({"type": "tv", "brand": ""})), Ok(()));
        assert_eq!(ok("finder", json!({"type": "tv", "brand": "LG"})), Ok(()));
        assert!(ok("finder", json!({})).is_err());
        assert_eq!(ok("try", json!({"type": "tv", "set": "TVs/LG/x", "button": "Power"})), Ok(()));
        assert!(ok("try", json!({"type": "tv", "set": "TVs/LG/x"})).is_err());
        assert_eq!(ok("use_set", json!({"type": "tv", "set": "TVs/LG/x"})), Ok(()));
        assert!(ok("use_set", json!({"type": "tv", "set": "a\nb"})).is_err());
        /* Taught names are fine in its state; twice the same isn't. */
        let state = |buttons: serde_json::Value| {
            set_capability(&ir, "remote", json!({"buttons": buttons, "learn": true}), Origin::Device)
        };
        assert!(state(json!(["Power", "UP", "Brighter"])).is_ok());
        assert!(state(json!(["Power", "Power"])).unwrap_err().contains("twice"));
    }

    /* A remote's buttons come from the device; clients can't set them. */
    #[test]
    fn remote_is_read_only_and_checked() {
        let mut tv = lamp();
        tv.capabilities.remote = Some(Remote::default());
        let err = set_capability(&tv, "remote", json!({"buttons": ["UP"]}), Origin::Client).unwrap_err();
        assert!(err.contains("read-only"), "{err}");
        assert!(set_capability(&tv, "remote", json!({"buttons": ["UP"], "keyboard": true}), Origin::Device).is_ok());
        assert!(set_capability(&tv, "remote", json!({"buttons": ["WARP"]}), Origin::Device).is_err());
    }

    fn lamp() -> Device {
        Device {
            id: "lamp-1".into(),
            name: "Living room lamp".into(),
            room: "Living room".into(),
            template: "dimmable-light".into(),
            source: Source::default(),
            config: Default::default(),
            identity: String::new(),
            online: None,
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
        assert_eq!(parsed.source, Source::default());
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
