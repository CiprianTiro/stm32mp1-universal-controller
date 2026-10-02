/*
 * ir_light.rs -- an IR remote device used as a LIGHT (issue #85): which of
 * its buttons switch it on and off, make it brighter or dimmer, and which
 * colour each colour button gives. The IR blaster's task
 * (adapters/ir_blaster.rs) turns that into the `switch` and `color`
 * capabilities, and the remote's `brightness` buttons.
 *
 * WHERE THE MEANING COMES FROM, for each button, first match wins:
 *
 *   1. ITS NAME, as the person taught it or the code library has it:
 *      "On", "Off", "Power", "Brighter", "Dimmer", "Red", "Light blue"...
 *      The person's word counts most: they see the remote's labels.
 *   2. ITS CODE, for a button whose name says nothing ("Key 7", "A"), if
 *      the device's buttons are one of the two standard RGB strip remotes
 *      most cheap strips come with:
 *        44 keys: NEC address 0x00 (Power 0x40, 0x58 0x59 0x45 0x44 the
 *                 top colour row, ...)
 *        24 keys: NEC address 0xEF00 (On 0x03, Off 0x02, Red 0x04, ...)
 *      "One of them" = at least ANCHORS of that remote's main buttons
 *      (power, brightness, top colour row) are among the device's codes:
 *      other remotes use address 0x00 too (a Sylvania strip: 0x40 =
 *      orange). Not first, because the 44-key remotes don't even agree
 *      among themselves on which of 0x58 / 0x59 / 0x45 is red, green and
 *      blue (the code library has both; the test strip's remote a third
 *      way).
 *   3. The standard remote's POWER key is the power toggle even when it's
 *      named "Off" (the library's "Led Remote" set) -- if nothing else
 *      could switch the light on.
 *
 * THE STATE IS ASSUMED. IR goes one way: the strip never says what it's
 * doing. The hub remembers what it last sent (and a Power toggle flips
 * that). Someone using the original remote makes it wrong until the next
 * press; #84 will follow the real remote ("heard" events).
 *
 * COLOUR ORDER. Cheap strips are often wired with two colours swapped:
 * the remote's Green shows red. A device's "colour order" (device.rs
 * COLOR_ORDERS, its config "colour_order") says what the strip REALLY
 * shows for the remote's Red, Green and Blue, so the palette shows the
 * real colours and a picked colour presses the button that gives it.
 */
use serde_json::Value;

use crate::ir_codes::LearnedButton;

/* An RGB colour, 0-255 per channel. */
pub type Rgb = [u8; 3];

/* What pressing a button does to a light. */
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    On,
    Off,
    /* One button for both: on if off, off if on. */
    Toggle,
    Brighter,
    Dimmer,
    /* This colour, as the REMOTE means it (before the colour order). */
    Color(Rgb),
}

/* A device's buttons, sorted by what they do for a light. Each role gets
 * the first button that has it (the order the buttons were taught). */
#[derive(Debug, Default, PartialEq)]
pub struct LightMap {
    pub on: Option<String>,
    pub off: Option<String>,
    pub toggle: Option<String>,
    pub brighter: Option<String>,
    pub dimmer: Option<String>,
    /* (button, colour as the remote means it), each colour once. */
    pub colors: Vec<(String, Rgb)>,
}

/* ------------------------------------------------------------------ */
/* The two standard remotes                                            */
/* ------------------------------------------------------------------ */

/* Both remotes have the same 4 columns of 5 colour keys: Red, Green,
 * Blue, White at the top, then shades. The shades are what the printed
 * keys look like; real strips differ a little, which is why a palette
 * colour is only "nearest". */
const SHADES: [[Rgb; 5]; 4] = [
    /* red -> orange -> yellow */
    [[255, 0, 0], [255, 64, 0], [255, 128, 0], [255, 176, 0], [255, 255, 0]],
    /* green -> cyan */
    [[0, 255, 0], [0, 255, 64], [0, 255, 160], [0, 255, 255], [0, 160, 160]],
    /* blue -> purple -> magenta */
    [[0, 0, 255], [0, 64, 255], [64, 0, 255], [128, 0, 255], [255, 0, 255]],
    /* white -> tinted whites */
    [[255, 255, 255], [255, 192, 192], [255, 160, 192], [192, 192, 255], [160, 192, 255]],
];

/* A standard remote: its NEC address, the commands of its non-colour
 * keys, and its colour keys' commands by [column][row] of SHADES. */
struct Standard {
    address: u64,
    on: Option<u64>,
    off: Option<u64>,
    toggle: Option<u64>,
    brighter: u64,
    dimmer: u64,
    colors: [[u64; 5]; 4],
}

const STANDARDS: [Standard; 2] = [
    /* The 44-key remote (IRremote's 0xFF02FD = Power, ...). */
    Standard {
        address: 0x00,
        on: None,
        off: None,
        toggle: Some(0x40),
        brighter: 0x5C,
        dimmer: 0x5D,
        colors: [
            [0x58, 0x54, 0x50, 0x1C, 0x18],
            [0x59, 0x55, 0x51, 0x1D, 0x19],
            [0x45, 0x49, 0x4D, 0x1E, 0x1A],
            [0x44, 0x48, 0x4C, 0x1F, 0x1B],
        ],
    },
    /* The 24-key remote (0xF7C03F = On, ...): extended NEC address. */
    Standard {
        address: 0xEF00,
        on: Some(0x03),
        off: Some(0x02),
        toggle: None,
        brighter: 0x00,
        dimmer: 0x01,
        colors: [
            [0x04, 0x08, 0x0C, 0x10, 0x14],
            [0x05, 0x09, 0x0D, 0x11, 0x15],
            [0x06, 0x0A, 0x0E, 0x12, 0x16],
            [0x07, 0x0B, 0x0F, 0x13, 0x17],
        ],
    },
];

/* How many of a remote's main keys must be among a device's codes for
 * the device to count as that remote (see the header). */
const ANCHORS: usize = 3;

impl Standard {
    /* Power/on/off, brightness, and R G B W: the keys every strip has. */
    fn main_keys(&self) -> Vec<u64> {
        let mut keys: Vec<u64> = [self.on, self.off, self.toggle].into_iter().flatten().collect();
        keys.extend([self.brighter, self.dimmer]);
        keys.extend(self.colors.iter().map(|column| column[0]));
        keys
    }

    fn role(&self, command: u64) -> Option<Role> {
        if Some(command) == self.on {
            return Some(Role::On);
        }
        if Some(command) == self.off {
            return Some(Role::Off);
        }
        if Some(command) == self.toggle {
            return Some(Role::Toggle);
        }
        if command == self.brighter {
            return Some(Role::Brighter);
        }
        if command == self.dimmer {
            return Some(Role::Dimmer);
        }
        for (column, commands) in self.colors.iter().enumerate() {
            if let Some(row) = commands.iter().position(|c| *c == command) {
                return Some(Role::Color(SHADES[column][row]));
            }
        }
        None
    }
}

/* (address, command) of an NEC code, as taught ({"proto": "nec", ...},
 * PROTOCOL.md) or from the library (the same shape). */
fn nec(code: &Value) -> Option<(u64, u64)> {
    if code["proto"] != "nec" {
        return None;
    }
    Some((code["address"].as_u64()?, code["command"].as_u64()?))
}

/* The standard remote these buttons are from, if any. */
fn standard_of(buttons: &[LearnedButton]) -> Option<&'static Standard> {
    let codes: Vec<(u64, u64)> = buttons.iter().filter_map(|b| nec(&b.code)).collect();
    STANDARDS.iter().find(|s| {
        s.main_keys().iter().filter(|key| codes.contains(&(s.address, **key))).count() >= ANCHORS
    })
}

/* ------------------------------------------------------------------ */
/* Names                                                               */
/* ------------------------------------------------------------------ */

/* "Bright UP" -> "brightup". Letters, digits, + and - only ("Light +"
 * and "Light -" must stay apart). */
fn normalise(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/* A button's role from its name. Only names that can't mean anything
 * else: "Up" alone could be an arrow, so it isn't one. */
fn role_by_name(name: &str) -> Option<Role> {
    let n = normalise(name);
    Some(match n.as_str() {
        "on" | "poweron" | "lighton" | "ledon" | "turnon" => Role::On,
        "off" | "poweroff" | "lightoff" | "ledoff" | "turnoff" => Role::Off,
        "power" | "pwr" | "onoff" | "poweronoff" | "powertoggle" | "ledonoff" => Role::Toggle,
        "brighter" | "brighten" | "brightup" | "brightnessup" | "brightness+" | "bright+" | "light+" | "lightup"
        | "uplight" | "dimmer+" | "dim+" | "+" => Role::Brighter,
        "darker" | "dim" | "dimmer" | "dimmer-" | "dim-" | "brightdown" | "brightdn" | "brightdwn" | "brightnessdown"
        | "brightnessdn" | "brightness-" | "bright-" | "light-" | "lightdown" | "downlight" | "-" => Role::Dimmer,
        _ => return color_by_name(&n).map(Role::Color),
    })
}

/* The colour keys' usual names (normalised). */
fn color_by_name(n: &str) -> Option<Rgb> {
    Some(match n {
        "red" | "r" => [255, 0, 0],
        "green" | "g" => [0, 255, 0],
        "blue" | "b" => [0, 0, 255],
        "white" | "w" => [255, 255, 255],
        "yellow" => [255, 255, 0],
        "orange" | "tangerine" => [255, 128, 0],
        "darkorange" => [255, 64, 0],
        "lightorange" | "apricot" => [255, 176, 0],
        "pink" | "carnation" => [255, 96, 160],
        "magenta" => [255, 0, 255],
        "purple" | "violet" | "purp" => [128, 0, 255],
        "lavender" | "lightpurple" | "lightpurp" => [176, 128, 255],
        "cyan" | "aqua" | "aquamarine" => [0, 255, 255],
        "turquoise" | "turqouise" => [64, 224, 208],
        "teal" => [0, 128, 128],
        "lightblue" | "babyblue" | "skyblue" | "azure" => [128, 192, 255],
        "lightgreen" | "lime" => [128, 255, 0],
        "seagreen" => [46, 139, 87],
        "lightred" => [255, 96, 96],
        "warmwhite" | "warm" | "ww" => [255, 216, 160],
        "coldwhite" | "coolwhite" | "cold" | "cw" => [208, 224, 255],
        _ => return None,
    })
}

/* ------------------------------------------------------------------ */
/* The map                                                             */
/* ------------------------------------------------------------------ */

impl LightMap {
    pub fn from_buttons(buttons: &[LearnedButton]) -> LightMap {
        let standard = standard_of(buttons);
        let mut map = LightMap::default();
        let by_code = |b: &LearnedButton| standard.and_then(|s| nec(&b.code).filter(|(a, _)| *a == s.address).and_then(|(_, c)| s.role(c)));
        for b in buttons {
            let Some(role) = role_by_name(&b.button).or_else(|| by_code(b)) else {
                continue;
            };
            let first = |slot: &mut Option<String>| {
                if slot.is_none() {
                    *slot = Some(b.button.clone());
                }
            };
            match role {
                Role::On => first(&mut map.on),
                Role::Off => first(&mut map.off),
                Role::Toggle => first(&mut map.toggle),
                Role::Brighter => first(&mut map.brighter),
                Role::Dimmer => first(&mut map.dimmer),
                Role::Color(rgb) => {
                    if !map.colors.iter().any(|(_, c)| *c == rgb) {
                        map.colors.push((b.button.clone(), rgb));
                    }
                }
            }
        }
        /* 3. (header): the power key, whatever it's called. */
        if !map.can_switch() {
            if let Some(power) = buttons.iter().find(|b| by_code(b) == Some(Role::Toggle)) {
                for slot in [&mut map.on, &mut map.off] {
                    if slot.as_deref() == Some(&power.button) {
                        *slot = None;
                    }
                }
                map.toggle = Some(power.button.clone());
            }
        }
        map
    }

    /* Can it be switched on AND off? (An "On" alone can't.) */
    pub fn can_switch(&self) -> bool {
        self.toggle.is_some() || (self.on.is_some() && self.off.is_some())
    }

    pub fn has_brightness(&self) -> bool {
        self.brighter.is_some() && self.dimmer.is_some()
    }

    /* What pressing this button does (for the assumed state). */
    pub fn role(&self, button: &str) -> Option<Role> {
        let is = |slot: &Option<String>| slot.as_deref() == Some(button);
        if is(&self.on) {
            Some(Role::On)
        } else if is(&self.off) {
            Some(Role::Off)
        } else if is(&self.toggle) {
            Some(Role::Toggle)
        } else if is(&self.brighter) {
            Some(Role::Brighter)
        } else if is(&self.dimmer) {
            Some(Role::Dimmer)
        } else {
            self.colors.iter().find(|(b, _)| b == button).map(|(_, rgb)| Role::Color(*rgb))
        }
    }

    /* The buttons to press to switch it on or off, given what the hub
     * assumes it is now. Empty = nothing to do (a toggle-only strip
     * already in that state). */
    pub fn presses_for_switch(&self, on: bool, assumed_on: bool) -> Vec<String> {
        let direct = if on { &self.on } else { &self.off };
        match (direct, &self.toggle) {
            (Some(button), _) => vec![button.clone()],
            (None, Some(toggle)) if on != assumed_on => vec![toggle.clone()],
            _ => Vec::new(),
        }
    }

    /* The colours it can really show (after the colour order), in button
     * order, each once: the `color` capability's palette. */
    pub fn palette(&self, order: &str) -> Vec<String> {
        let mut shown: Vec<String> = Vec::new();
        for (_, rgb) in &self.colors {
            let hex = to_hex(shown_color(*rgb, order));
            if !shown.contains(&hex) {
                shown.push(hex);
            }
        }
        shown
    }

    /* The colour button whose REAL colour is nearest to `want`, and that
     * real colour. */
    pub fn nearest(&self, want: Rgb, order: &str) -> Option<(String, Rgb)> {
        self.colors
            .iter()
            .map(|(button, rgb)| (button.clone(), shown_color(*rgb, order)))
            .min_by_key(|(_, shown)| distance(*shown, want))
    }
}

/* ------------------------------------------------------------------ */
/* Colours                                                             */
/* ------------------------------------------------------------------ */

/* What the strip really shows for a colour the remote means: the
 * remote's red channel drives the strip's order[0] colour, and so on.
 * An unknown order is taken as wired right. */
pub fn shown_color(remote: Rgb, order: &str) -> Rgb {
    let index = |letter: u8| match letter {
        b'R' => Some(0),
        b'G' => Some(1),
        b'B' => Some(2),
        _ => None,
    };
    let targets: Vec<usize> = order.bytes().filter_map(index).collect();
    /* Each of R, G, B exactly once. */
    if targets.len() != 3 || !(0..3).all(|t| targets.contains(&t)) {
        return remote;
    }
    let mut out = [0u8; 3];
    for (channel, target) in targets.into_iter().enumerate() {
        out[target] = remote[channel];
    }
    out
}

/* Squared distance, weighted the usual way (the eye is most sensitive to
 * green, least to blue), so "orange" finds orange, not red. */
fn distance(a: Rgb, b: Rgb) -> u32 {
    let d = |i: usize| (a[i] as i32 - b[i] as i32).unsigned_abs();
    3 * d(0).pow(2) + 4 * d(1).pow(2) + 2 * d(2).pow(2)
}

pub fn to_hex(rgb: Rgb) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

/* "#FF8800" -> [255, 136, 0] (already checked by device.rs). */
pub fn from_hex(hex: &str) -> Option<Rgb> {
    let digits = hex.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/* A white temperature, roughly, as RGB (a client may ask an IR strip for
 * "2700 K": it gets the nearest of its colours, usually White). */
pub fn kelvin_to_rgb(kelvin: u16) -> Rgb {
    match kelvin {
        0..=3500 => [255, 216, 160],
        3501..=5000 => [255, 240, 224],
        _ => [255, 255, 255],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn button(name: &str, code: Value) -> LearnedButton {
        LearnedButton {
            button: name.into(),
            code,
        }
    }

    fn nec44(name: &str, command: u64) -> LearnedButton {
        button(name, json!({"proto": "nec", "address": 0, "command": command}))
    }

    #[test]
    fn the_44_key_remote_is_known_by_its_codes_whatever_the_names() {
        /* The test strip as taught in #42: lowercase names. */
        let buttons = [nec44("power", 0x40), nec44("red", 0x58), nec44("green", 0x59), nec44("blue", 0x45)];
        let map = LightMap::from_buttons(&buttons);
        assert_eq!(map.toggle.as_deref(), Some("power"));
        assert!(map.can_switch());
        assert_eq!(map.colors.len(), 3);

        /* Names that mean nothing: the codes still do. */
        let buttons = [nec44("A", 0x40), nec44("B1", 0x58), nec44("C", 0x5C), nec44("D", 0x5D), nec44("E", 0x50)];
        let map = LightMap::from_buttons(&buttons);
        assert_eq!(map.toggle.as_deref(), Some("A"));
        assert_eq!(map.brighter.as_deref(), Some("C"));
        assert_eq!(map.dimmer.as_deref(), Some("D"));
        assert_eq!(map.colors, vec![("B1".into(), [255, 0, 0]), ("E".into(), [255, 128, 0])]);
    }

    #[test]
    fn names_win_over_codes() {
        /* The test strip, exactly as taught on the board: its remote's Red
         * is 0x59, which the usual 44-key table calls green. */
        let buttons = [nec44("power", 0x40), nec44("red", 0x59), nec44("green", 0x45), nec44("blue", 0x58)];
        let map = LightMap::from_buttons(&buttons);
        assert_eq!(
            map.colors,
            vec![("red".into(), [255, 0, 0]), ("green".into(), [0, 255, 0]), ("blue".into(), [0, 0, 255])]
        );
        assert_eq!(map.toggle.as_deref(), Some("power"));
    }

    #[test]
    fn the_library_led_remote_set_power_is_a_toggle() {
        /* The library's "Led Remote" set calls 0x40 "Off": it's the 44-key
         * Power, a toggle; the codes win over the name. */
        let buttons = [nec44("Red", 0x58), nec44("Green", 0x59), nec44("Off", 0x40), nec44("Light +", 0x5C)];
        let map = LightMap::from_buttons(&buttons);
        assert_eq!(map.toggle.as_deref(), Some("Off"));
        assert_eq!(map.off, None);
        assert_eq!(map.brighter.as_deref(), Some("Light +"));
    }

    #[test]
    fn the_24_key_remote_is_known_by_its_codes() {
        let k24 = |name: &str, c: u64| button(name, json!({"proto": "nec", "address": 0xEF00, "command": c}));
        let buttons = [k24("x1", 0x03), k24("x2", 0x02), k24("x3", 0x04), k24("x4", 0x00), k24("x5", 0x01), k24("x6", 0x14)];
        let map = LightMap::from_buttons(&buttons);
        assert_eq!((map.on.as_deref(), map.off.as_deref(), map.toggle.as_deref()), (Some("x1"), Some("x2"), None));
        assert!(map.has_brightness());
        assert_eq!(map.colors, vec![("x3".into(), [255, 0, 0]), ("x6".into(), [255, 255, 0])]);
    }

    #[test]
    fn another_remote_on_address_0_goes_by_names() {
        /* Sylvania's 24 keys: address 0 too, but 0x40 = Orange there. */
        let buttons = [nec44("POWER", 0x0D), nec44("Off", 0x1F), nec44("Orange", 0x40), nec44("Red", 0x19)];
        let map = LightMap::from_buttons(&buttons);
        assert_eq!(map.toggle.as_deref(), Some("POWER"));
        assert_eq!(map.off.as_deref(), Some("Off"));
        assert_eq!(map.colors, vec![("Orange".into(), [255, 128, 0]), ("Red".into(), [255, 0, 0])]);
    }

    #[test]
    fn names_for_taught_raw_codes() {
        let raw = || json!({"proto": "raw", "carrier_hz": 38000, "timings": [9000, 4500]});
        let names = ["On", "Off", "Bright UP", "Bright DWN", "Light blue", "Up", "Flash", "Red", "R"];
        let buttons: Vec<LearnedButton> = names.iter().map(|n| button(n, raw())).collect();
        let map = LightMap::from_buttons(&buttons);
        assert_eq!(map.on.as_deref(), Some("On"));
        assert_eq!(map.off.as_deref(), Some("Off"));
        assert_eq!(map.brighter.as_deref(), Some("Bright UP"));
        assert_eq!(map.dimmer.as_deref(), Some("Bright DWN"));
        /* "Up" is an arrow, "Flash" an effect; "R" repeats Red: once. */
        assert_eq!(map.colors, vec![("Light blue".into(), [128, 192, 255]), ("Red".into(), [255, 0, 0])]);
        assert_eq!(map.role("R"), None);
        assert_eq!(map.role("Red"), Some(Role::Color([255, 0, 0])));
        assert_eq!(map.role("Bright UP"), Some(Role::Brighter));
    }

    #[test]
    fn switching_with_on_off_or_a_toggle() {
        let both = LightMap {
            on: Some("On".into()),
            off: Some("Off".into()),
            ..Default::default()
        };
        assert_eq!(both.presses_for_switch(true, true), vec!["On"]); /* sent anyway: the state is assumed */
        assert_eq!(both.presses_for_switch(false, true), vec!["Off"]);
        let toggle = LightMap {
            toggle: Some("Power".into()),
            ..Default::default()
        };
        assert_eq!(toggle.presses_for_switch(true, false), vec!["Power"]);
        assert!(toggle.presses_for_switch(true, true).is_empty());
        let only_on = LightMap {
            on: Some("On".into()),
            ..Default::default()
        };
        assert!(!only_on.can_switch());
    }

    #[test]
    fn colour_order_swaps_what_the_strip_shows() {
        /* The test strip: Green shows red, Red shows green. */
        assert_eq!(shown_color([0, 255, 0], "GRB"), [255, 0, 0]);
        assert_eq!(shown_color([255, 0, 0], "GRB"), [0, 255, 0]);
        assert_eq!(shown_color([255, 128, 0], "GRB"), [128, 255, 0]);
        assert_eq!(shown_color([0, 0, 255], "BRG"), [0, 255, 0]);
        assert_eq!(shown_color([1, 2, 3], "RGB"), [1, 2, 3]);
        assert_eq!(shown_color([1, 2, 3], "RRB"), [1, 2, 3]); /* nonsense: as is */

        let buttons = [nec44("power", 0x40), nec44("red", 0x58), nec44("green", 0x59), nec44("blue", 0x45)];
        let map = LightMap::from_buttons(&buttons);
        assert_eq!(map.palette("RGB"), vec!["#FF0000", "#00FF00", "#0000FF"]);
        assert_eq!(map.palette("GRB"), vec!["#00FF00", "#FF0000", "#0000FF"]);
        /* Asking for red presses the button that SHOWS red. */
        assert_eq!(map.nearest([255, 0, 0], "GRB"), Some(("green".into(), [255, 0, 0])));
        assert_eq!(map.nearest([255, 0, 0], "RGB"), Some(("red".into(), [255, 0, 0])));
    }

    #[test]
    fn nearest_colour() {
        let map = LightMap {
            colors: vec![("Red".into(), [255, 0, 0]), ("Orange".into(), [255, 128, 0]), ("White".into(), [255, 255, 255])],
            ..Default::default()
        };
        assert_eq!(map.nearest([255, 140, 20], "RGB").unwrap().0, "Orange");
        assert_eq!(map.nearest([230, 10, 10], "RGB").unwrap().0, "Red");
        assert_eq!(map.nearest(kelvin_to_rgb(2700), "RGB").unwrap().0, "White");
        assert_eq!(LightMap::default().nearest([0, 0, 0], "RGB"), None);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(from_hex("#FF8800"), Some([255, 136, 0]));
        assert_eq!(from_hex("#ff8800"), Some([255, 136, 0]));
        assert_eq!(from_hex("FF8800"), None);
        assert_eq!(to_hex([255, 136, 0]), "#FF8800");
    }
}
