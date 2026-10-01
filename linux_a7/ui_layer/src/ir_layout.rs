//! An IR device's buttons laid out as a remote (issue #82).
//!
//! Buttons from the code library for a TV-like device carry the hub's
//! standard names (backend_daemon ir_library.rs: VOLUME_UP, OK, 5, ...),
//! and "Power". Those go into a remote's usual places: Power on top, the
//! arrows around OK, volume, channels, digits, colour keys, playback. The
//! remote's other buttons ("Netflix", "Subtitle") stay in the grid below.
//! A device with only a few standard buttons (an LED strip) keeps the
//! plain grid.

use crate::{IrKeyItem, IrRow};

/// The rows, top to bottom. `true`: a block whose shape matters (the
/// arrows, the digits): a missing key leaves a gap. Otherwise missing keys
/// are left out and the row closes up.
const ROWS: &[(bool, &[&str])] = &[
    (false, &["Power"]),
    (false, &["HOME", "BACK", "MENU", "EXIT", "INFO"]),
    (true, &["", "UP", ""]),
    (true, &["LEFT", "OK", "RIGHT"]),
    (true, &["", "DOWN", ""]),
    (false, &["VOLUME_DOWN", "MUTE", "VOLUME_UP"]),
    (false, &["CHANNEL_DOWN", "CHANNEL_UP"]),
    (true, &["1", "2", "3"]),
    (true, &["4", "5", "6"]),
    (true, &["7", "8", "9"]),
    (true, &["", "0", ""]),
    (false, &["RED", "GREEN", "YELLOW", "BLUE"]),
    (false, &["REWIND", "PLAY", "PAUSE", "STOP", "FAST_FORWARD"]),
];

/// Fewer standard buttons than this (not counting Power): a plain grid.
const MIN_STANDARD: usize = 3;

/// What a standard button shows (the same as the TV remote, remote.slint).
pub fn label(name: &str) -> &str {
    match name {
        "UP" => "\u{25B2}",
        "DOWN" => "\u{25BC}",
        "LEFT" => "\u{25C0}",
        "RIGHT" => "\u{25B6}",
        "OK" => "OK",
        "HOME" => "Home",
        "BACK" => "Back",
        "MENU" => "Menu",
        "EXIT" => "Exit",
        "INFO" => "Info",
        "VOLUME_DOWN" => "Vol \u{2212}",
        "VOLUME_UP" => "Vol +",
        "MUTE" => "Mute",
        "CHANNEL_DOWN" => "Ch \u{2212}",
        "CHANNEL_UP" => "Ch +",
        "RED" => "Red",
        "GREEN" => "Green",
        "YELLOW" => "Yellow",
        "BLUE" => "Blue",
        "REWIND" => "\u{25C0}\u{25C0}",
        "PLAY" => "Play",
        "PAUSE" => "Pause",
        "STOP" => "Stop",
        "FAST_FORWARD" => "\u{25B6}\u{25B6}",
        other => other,
    }
}

/// The device's buttons -> (remote rows, the buttons not in them). No
/// rows when it hasn't enough standard buttons.
pub fn layout(buttons: &[String]) -> (Vec<IrRow>, Vec<String>) {
    let has = |name: &str| buttons.iter().any(|b| b == name);
    let in_rows = |name: &str| ROWS.iter().any(|(_, keys)| keys.contains(&name));
    let standard = buttons.iter().filter(|b| *b != "Power" && in_rows(b)).count();
    if standard < MIN_STANDARD {
        return (Vec::new(), buttons.to_vec());
    }
    let key = |name: &str| IrKeyItem { name: name.into(), label: label(name).into() };
    let mut rows = Vec::new();
    for (keeps_shape, names) in ROWS {
        let keys: Vec<IrKeyItem> = if *keeps_shape {
            if !names.iter().any(|n| !n.is_empty() && has(n)) {
                continue;
            }
            // A gap is a key without a name.
            names.iter().map(|n| if has(n) { key(n) } else { key("") }).collect()
        } else {
            names.iter().filter(|n| has(n)).map(|n| key(n)).collect()
        };
        if !keys.is_empty() {
            rows.push(IrRow { keys: std::rc::Rc::new(slint::VecModel::from(keys)).into() });
        }
    }
    let others = buttons.iter().filter(|b| !in_rows(b)).cloned().collect();
    (rows, others)
}
