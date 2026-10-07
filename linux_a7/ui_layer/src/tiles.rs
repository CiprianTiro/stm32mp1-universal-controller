//! tiles.rs -- what the Devices page shows since issue #95 (tiles.slint):
//! one tile per device (an icon, its name, its state in one line), the
//! filter chips above them, and which tiles a chip selects, in which order,
//! under which room headings.
//!
//! Pure logic on the devices as backend_daemon reports them (ws_client.rs):
//! no screen, no network -- so tools/ui_preview runs exactly this code on
//! its sample devices, and its unit tests run on the PC (the UI crate
//! itself only links on the board).

use crate::ws_client::{Device, Template};
use crate::{FilterChip, TileItem};

/* The filter chips' ids (app.slint's `device-filter`). A room's chip is
 * ROOM + its name ("room:Office"; "room:" = devices without a room). */
pub const ALL: &str = "all";
pub const FAVOURITES: &str = "fav";
pub const LIGHTS: &str = "lights";
pub const ENERGY: &str = "energy";
pub const OFFLINE: &str = "offline";
pub const ROOM: &str = "room:";

/// A heading ("" = none) and the tiles under it, in order. main.rs splits
/// each group's tiles into rows of the grid's width.
pub type Group = (String, Vec<TileItem>);

/// Can the hub reach it? (Unknown counts as yes: virtual devices.)
pub fn reachable(d: &Device) -> bool {
    !matches!(d.online.as_deref(), Some("offline") | Some("unauthorized"))
}

/// Why it can't be used, for the tile: "" if it can.
fn status(d: &Device) -> &'static str {
    match d.online.as_deref() {
        Some("offline") => "Offline",
        Some("unauthorized") => "Needs pairing again",
        _ => "",
    }
}

/// Which icon (icons.slint's kind). The capabilities say most; the
/// template's category tells a lamp from a plug (both are "a switch").
pub fn kind(d: &Device, templates: &[Template]) -> &'static str {
    let c = &d.capabilities;
    let category = templates.iter().find(|t| t.id == d.template).map_or("", |t| t.category.as_str());
    let ir = c.remote.as_ref().is_some_and(|r| r.learn);
    if c.media.is_some() {
        "tv"
    } else if c.cover.is_some() {
        "blind"
    } else if c.vacuum.is_some() {
        "vacuum"
    } else if category == "cameras" {
        "camera"
    } else if c.climate.is_some() {
        "climate"
    } else if let Some(lock) = &c.lock {
        if lock.state == "unlocked" {
            "unlocked"
        } else {
            "lock"
        }
    } else if (ir && c.color.is_some()) || d.template.starts_with("wled") {
        /* An IR LED strip (#85) or WLED. */
        "strip"
    } else if category == "lighting" || c.dimmer.is_some() || c.color.is_some() {
        if c.switch.as_ref().is_some_and(|s| s.on) {
            "light"
        } else {
            "light-off"
        }
    } else if category == "plugs" || c.energy.is_some() {
        "plug"
    } else if ir {
        "remote"
    } else if c.switch.is_some() {
        "switch"
    } else if c.sensor.is_some() {
        "sensor"
    } else {
        "device"
    }
}

/// A light (the "N on" chip, "Turn all off").
/// A plug (any: a Shelly, an EZVIZ, a Tasmota), or anything that measures
/// power: what the power chip lists.
pub fn is_plug(d: &Device, templates: &[Template]) -> bool {
    d.capabilities.energy.is_some() || kind(d, templates) == "plug"
}

pub fn is_light(d: &Device, templates: &[Template]) -> bool {
    matches!(kind(d, templates), "light" | "light-off" | "strip") && d.capabilities.switch.is_some()
}

fn is_on(d: &Device) -> bool {
    d.capabilities.switch.as_ref().is_some_and(|s| s.on)
}

/// 21.5 -> "21.5", 22.0 -> "22".
fn number(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

/// 18.42 -> "18.4 W", 1860 -> "1.86 kW".
pub fn watts(w: f64) -> String {
    if w.abs() >= 1000.0 {
        format!("{:.2} kW", w / 1000.0)
    } else {
        format!("{} W", number(w))
    }
}

/// "just now", "3 min ago"... (issue #72's battery devices).
pub fn seen_ago(at: u64, now: u64) -> String {
    match now.saturating_sub(at) {
        0..=59 => "just now".into(),
        s @ 60..=3599 => format!("{} min ago", s / 60),
        s @ 3600..=86399 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

/// "heat" -> "Heat".
fn capital(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or(String::new(), |first| first.to_uppercase().chain(chars).collect())
}

/// A vacuum's one line: what's wrong, else what it does and its battery:
/// "Cleaning \u{2022} 84 %", "Charging \u{2022} 100 %", "Stuck: move it...".
pub fn vacuum_line(vacuum: &crate::ws_client::Vacuum) -> String {
    if !vacuum.error.is_empty() {
        return vacuum.error.clone();
    }
    let doing = if vacuum.detail.is_empty() { capital(&vacuum.state) } else { capital(&vacuum.detail) };
    match vacuum.battery {
        Some(battery) => format!("{doing} \u{2022} {battery} %"),
        None => doing,
    }
}

/// A vacuum's run: "This run: 14.5 m² \u{2022} 21 min" ("Last run" when
/// it's back); "" before it ever cleaned.
pub fn vacuum_run(vacuum: &crate::ws_client::Vacuum) -> String {
    match (vacuum.area_m2, vacuum.minutes) {
        (Some(area), Some(minutes)) if area > 0.0 || minutes > 0 => {
            let which = if vacuum.state == "cleaning" || vacuum.state == "paused" { "This run" } else { "Last run" };
            format!("{which}: {area:.1} m² \u{2022} {minutes} min")
        }
        _ => String::new(),
    }
}

/// The tile's one line: what matters about it now.
pub fn state_line(d: &Device, now: u64) -> String {
    let c = &d.capabilities;
    let dot = " \u{2022} ";
    let mut line = if let Some(media) = &c.media {
        /* A TV: what's on, if on. */
        let playing = match (&media.app, &media.channel) {
            (_, Some(channel)) => format!("{} {}", channel.number, channel.name).trim().to_string(),
            (Some(app), None) => app.label.clone(),
            _ => String::new(),
        };
        match (is_on(d), playing.is_empty()) {
            (false, _) => "Off".into(),
            (true, true) => "On".into(),
            (true, false) => playing,
        }
    } else if let Some(cover) = &c.cover {
        match (cover.moving.as_str(), cover.position) {
            ("opening", _) => "Opening\u{2026}".into(),
            ("closing", _) => "Closing\u{2026}".into(),
            (_, Some(0)) => "Closed".into(),
            (_, Some(100)) => "Open".into(),
            (_, Some(p)) => format!("{p} % open"),
            (_, None) => "Position not known".into(),
        }
    } else if let Some(climate) = &c.climate {
        let set = if climate.mode == "off" {
            "Off".to_string()
        } else {
            format!("{} to {} °C", capital(&climate.mode), number(climate.target))
        };
        match climate.current {
            Some(now) => format!("{set}{dot}{} °C", number(now)),
            None => set,
        }
    } else if let Some(lock) = &c.lock {
        capital(&lock.state)
    } else if let Some(vacuum) = &c.vacuum {
        vacuum_line(vacuum)
    } else if let (Some(switch), true) = (&c.switch, d.template.contains("camera")) {
        /* A camera's switch is its privacy mode (#74, tapo.rs). */
        if switch.on { "Watching" } else { "Privacy mode" }.to_string()
    } else if let Some(switch) = &c.switch {
        let mut parts = vec![if switch.on { "On" } else { "Off" }.to_string()];
        if let (true, Some(dimmer)) = (switch.on, &c.dimmer) {
            parts.push(format!("{} %", dimmer.level));
        }
        if let Some(energy) = &c.energy {
            if switch.on || energy.power_w.abs() >= 0.05 {
                parts.push(watts(energy.power_w));
            }
        }
        parts.join(dot)
    } else if c.camera.as_ref().is_some_and(|c| c.snapshot) && c.switch.is_none() {
        /* A camera with pictures only (RTSP, ESP32-CAM, #43); its ONVIF
         * motion, when it reports some. */
        let motion = c.sensor.as_ref().and_then(|s| s.readings.get("motion")).is_some_and(|r| r.value >= 1.0);
        if motion { "Motion now" } else { "Tap for its picture" }.into()
    } else if let Some(sensor) = &c.sensor {
        /* Temperature and humidity first, at most two values. */
        let mut readings: Vec<(&String, &crate::ws_client::Reading)> = sensor.readings.iter().collect();
        readings.sort_by_key(|(name, _)| match name.as_str() {
            "temperature" => 0,
            "humidity" => 1,
            _ => 2,
        });
        readings
            .iter()
            .take(2)
            .map(|(_, r)| format!("{} {}", number(r.value), r.unit).trim().to_string())
            .collect::<Vec<_>>()
            .join(dot)
    } else if let Some(remote) = &c.remote {
        match remote.buttons.len() {
            0 => "No buttons yet".into(),
            1 => "1 button".into(),
            n => format!("{n} buttons"),
        }
    } else {
        String::new()
    };
    /* A battery device (issue #72): when it last reported. */
    if let Some(at) = d.last_seen {
        if !line.is_empty() {
            line.push_str(dot);
        }
        line.push_str(&format!("seen {}", seen_ago(at, now)));
    }
    line
}

/// The colour a tile glows in while on: a light's own colour, else the
/// accent. A white temperature as a warm-to-cold white.
fn tint(d: &Device, accent: slint::Color) -> slint::Color {
    let Some(color) = &d.capabilities.color else { return accent };
    if let Some(hex) = &color.hex {
        let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0xFFFFFF);
        return slint::Color::from_rgb_u8((value >> 16) as u8, (value >> 8) as u8, value as u8);
    }
    let kelvin = f32::from(color.kelvin.unwrap_or(4000));
    let t = ((kelvin - 2000.0) / 4500.0).clamp(0.0, 1.0);
    let mix = |warm: f32, cold: f32| (warm + (cold - warm) * t).round() as u8;
    slint::Color::from_rgb_u8(mix(255.0, 201.0), mix(167.0, 218.0), mix(87.0, 255.0))
}

/// Dark text on a light colour, white on a dark one (perceived brightness,
/// the usual 0.299 / 0.587 / 0.114 weights).
fn readable_on(c: slint::Color) -> slint::Color {
    let luma = 0.299 * f32::from(c.red()) + 0.587 * f32::from(c.green()) + 0.114 * f32::from(c.blue());
    if luma > 150.0 {
        slint::Color::from_rgb_u8(0x1E, 0x29, 0x3B)
    } else {
        slint::Color::from_rgb_u8(0xFF, 0xFF, 0xFF)
    }
}

/// The bar along a tile's bottom: a light's brightness (while on), a
/// blind's position; -1 = none.
fn level(d: &Device) -> i32 {
    let c = &d.capabilities;
    if let (Some(dimmer), true) = (&c.dimmer, is_on(d)) {
        return i32::from(dimmer.level);
    }
    c.cover.as_ref().and_then(|cover| cover.position).map_or(-1, i32::from)
}

pub fn tile(d: &Device, templates: &[Template], accent: slint::Color, now: u64) -> TileItem {
    let tint = tint(d, accent);
    let climate_on = d.capabilities.climate.as_ref().is_some_and(|c| c.mode != "off");
    /* A vacuum at work (#74) is "on" like a running AC: the tile lights up. */
    let vacuum_busy = d.capabilities.vacuum.as_ref().is_some_and(|v| v.state == "cleaning" || v.state == "returning");
    TileItem {
        id: d.id.clone().into(),
        name: d.name.clone().into(),
        state: state_line(d, now).into(),
        icon: kind(d, templates).into(),
        on: is_on(d) || climate_on || vacuum_busy,
        can_toggle: d.capabilities.switch.is_some() && reachable(d),
        tint,
        on_tint: readable_on(tint),
        status: status(d).into(),
        level: level(d),
        favourite: d.favourite,
    }
}

/// The chips: summaries (only when there's something to say), then All,
/// Favourites (if any), and every room.
pub fn chips(devices: &[&Device], templates: &[Template]) -> Vec<FilterChip> {
    let chip = |id: &str, label: String, icon: &str, warn: bool| FilterChip {
        id: id.into(),
        label: label.into(),
        icon: icon.into(),
        warn,
    };
    let mut chips = Vec::new();
    let lights_on = devices.iter().filter(|d| is_light(d, templates) && is_on(d) && reachable(d)).count();
    if lights_on > 0 {
        chips.push(chip(LIGHTS, format!("{lights_on} on"), "light", false));
    }
    /* Power: what's measured, in all; with plugs but nothing measuring
     * (EZVIZ plugs only say on/off), how many plugs. */
    let metered: Vec<f64> = devices.iter().filter_map(|d| d.capabilities.energy.as_ref()).map(|e| e.power_w).collect();
    let plugs = devices.iter().filter(|d| is_plug(d, templates)).count();
    if !metered.is_empty() {
        chips.push(chip(ENERGY, watts(metered.iter().sum()), "power", false));
    } else if plugs > 0 {
        chips.push(chip(ENERGY, if plugs == 1 { "1 plug".into() } else { format!("{plugs} plugs") }, "power", false));
    }
    let offline = devices.iter().filter(|d| !reachable(d)).count();
    if offline > 0 {
        chips.push(chip(OFFLINE, format!("{offline} offline"), "alert", true));
    }
    chips.push(chip(ALL, "All".into(), "home", false));
    if devices.iter().any(|d| d.favourite) {
        chips.push(chip(FAVOURITES, "Favourites".into(), "star", false));
    }
    let mut rooms: Vec<&str> = devices.iter().map(|d| d.room.as_str()).filter(|r| !r.is_empty()).collect();
    rooms.sort_unstable();
    rooms.dedup();
    for room in &rooms {
        chips.push(chip(&format!("{ROOM}{room}"), room.to_string(), "", false));
    }
    if !rooms.is_empty() && devices.iter().any(|d| d.room.is_empty()) {
        chips.push(chip(ROOM, "No room".into(), "", false));
    }
    chips
}

/// The tiles a chip selects, grouped under headings. "All" groups by room
/// (rooms A-Z, devices without one last; no headings if there are no
/// rooms at all); the others are one group. Within a group: reachable
/// devices first, then by name.
pub fn groups(devices: &[&Device], filter: &str, templates: &[Template], accent: slint::Color, now: u64) -> Vec<Group> {
    let mut picked: Vec<&Device> = devices
        .iter()
        .copied()
        .filter(|d| match filter {
            FAVOURITES => d.favourite,
            LIGHTS => is_light(d, templates) && is_on(d) && reachable(d),
            ENERGY => is_plug(d, templates),
            OFFLINE => !reachable(d),
            f => f.strip_prefix(ROOM).is_none_or(|room| d.room == room),
        })
        .collect();
    let by_name = |a: &&Device, b: &&Device| {
        (!reachable(a), a.name.to_lowercase(), &a.id).cmp(&(!reachable(b), b.name.to_lowercase(), &b.id))
    };
    let tiles = |list: Vec<&Device>| list.into_iter().map(|d| tile(d, templates, accent, now)).collect::<Vec<_>>();
    if filter == ENERGY {
        /* What measures first, the biggest consumers on top; then the
         * plugs that don't measure. */
        let power = |d: &Device| d.capabilities.energy.as_ref().map(|e| e.power_w);
        picked.sort_by(|a, b| {
            (power(b).is_some(), power(b).unwrap_or(0.0))
                .partial_cmp(&(power(a).is_some(), power(a).unwrap_or(0.0)))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| by_name(a, b))
        });
        return vec![(String::new(), tiles(picked))];
    }
    picked.sort_by(by_name);
    let has_rooms = picked.iter().any(|d| !d.room.is_empty());
    if filter != ALL || !has_rooms {
        return if picked.is_empty() { Vec::new() } else { vec![(String::new(), tiles(picked))] };
    }
    let mut rooms: Vec<&str> = picked.iter().map(|d| d.room.as_str()).collect();
    /* A-Z, "" (no room) last. */
    rooms.sort_by_key(|r| (r.is_empty(), r.to_lowercase()));
    rooms.dedup();
    rooms
        .into_iter()
        .map(|room| {
            let heading = if room.is_empty() { "No room" } else { room };
            (heading.to_string(), tiles(picked.iter().copied().filter(|d| d.room == room).collect()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(json: serde_json::Value) -> Device {
        serde_json::from_value(json).unwrap()
    }

    fn accent() -> slint::Color {
        slint::Color::from_rgb_u8(0x38, 0xBD, 0xF8)
    }

    fn sample() -> Vec<Device> {
        vec![
            device(serde_json::json!({"id": "bulb", "name": "Hall bulb", "room": "Hall", "template": "wiz", "online": "online",
                "capabilities": {"switch": {"on": true}, "dimmer": {"level": 42}, "color": {"hex": "#FF0000"}}})),
            device(serde_json::json!({"id": "plug", "name": "Kettle", "room": "Kitchen", "template": "shelly-plug-gen3", "favourite": true,
                "online": "online", "capabilities": {"switch": {"on": true}, "energy": {"power_w": 1860.0}}})),
            device(serde_json::json!({"id": "strip", "name": "Shelf strip", "room": "Hall", "template": "ir-blaster", "online": "offline",
                "capabilities": {"switch": {"on": true}, "color": {"hex": "#00FF00", "palette": ["#00FF00"]},
                                 "remote": {"buttons": ["power"], "learn": true}}})),
            device(serde_json::json!({"id": "sensor", "name": "Bedroom air", "template": "tasmota-sensor-battery", "last_seen": 1000,
                "capabilities": {"sensor": {"readings": {"humidity": {"value": 48.0, "unit": "%"},
                                                          "temperature": {"value": 21.46, "unit": "°C"}}}}})),
        ]
    }

    #[test]
    fn state_lines() {
        let all = sample();
        assert_eq!(state_line(&all[0], 0), "On \u{2022} 42 %");
        assert_eq!(state_line(&all[1], 0), "On \u{2022} 1.86 kW");
        assert_eq!(state_line(&all[3], 1200), "21.5 °C \u{2022} 48 % \u{2022} seen 3 min ago");
    }

    #[test]
    fn kinds() {
        let all = sample();
        let kinds: Vec<&str> = all.iter().map(|d| kind(d, &[])).collect();
        assert_eq!(kinds, ["light", "plug", "strip", "sensor"]);
    }

    /* Issue #74: the power chip lists every plug -- the ones that
     * measure first (biggest on top), then cloud plugs that only switch. */
    #[test]
    fn the_power_chip_lists_all_plugs() {
        let mut all = sample();
        all.push(device(serde_json::json!({"id": "ez", "name": "Desk plug", "template": "ezviz-plug", "online": "online",
            "capabilities": {"switch": {"on": true}}})));
        let templates: Vec<Template> = vec![serde_json::from_value(serde_json::json!(
            {"id": "ezviz-plug", "name": "EZVIZ smart plug", "category": "plugs", "variants": []})).unwrap()];
        let refs: Vec<&Device> = all.iter().collect();
        let groups = groups(&refs, ENERGY, &templates, accent(), 0);
        let names: Vec<String> = groups[0].1.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["Kettle", "Desk plug"]);
        /* Without anything measuring: "N plugs". */
        let only_ezviz: Vec<&Device> = all.iter().filter(|d| d.id == "ez").collect();
        let chip = chips(&only_ezviz, &templates).into_iter().find(|c| c.id == ENERGY).unwrap();
        assert_eq!(chip.label, "1 plug");
    }

    #[test]
    fn chips_summarise_and_list_rooms() {
        let all = sample();
        let refs: Vec<&Device> = all.iter().collect();
        let ids: Vec<String> = chips(&refs, &[]).iter().map(|c| c.id.to_string()).collect();
        /* The offline strip doesn't count as "on". */
        assert_eq!(ids, ["lights", "energy", "offline", "all", "fav", "room:Hall", "room:Kitchen", "room:"]);
        assert_eq!(chips(&refs, &[])[0].label, "1 on");
    }

    #[test]
    fn all_groups_by_room_reachable_first() {
        let all = sample();
        let refs: Vec<&Device> = all.iter().collect();
        let g = groups(&refs, ALL, &[], accent(), 0);
        let shape: Vec<(String, Vec<String>)> =
            g.iter().map(|(h, t)| (h.clone(), t.iter().map(|t| t.id.to_string()).collect())).collect();
        assert_eq!(
            shape,
            [
                ("Hall".to_string(), vec!["bulb".to_string(), "strip".to_string()]),
                ("Kitchen".to_string(), vec!["plug".to_string()]),
                ("No room".to_string(), vec!["sensor".to_string()]),
            ]
        );
        let fav = groups(&refs, FAVOURITES, &[], accent(), 0);
        assert_eq!(fav.len(), 1);
        assert_eq!(fav[0].1[0].id, "plug");
        assert!(groups(&refs, "room:Nowhere", &[], accent(), 0).is_empty());
        /* The offline strip can't be switched from its tile. */
        assert!(!g[0].1[1].can_toggle);
    }

    #[test]
    fn text_on_a_tint_stays_readable() {
        assert_eq!(readable_on(slint::Color::from_rgb_u8(255, 255, 0)), slint::Color::from_rgb_u8(0x1E, 0x29, 0x3B));
        assert_eq!(readable_on(slint::Color::from_rgb_u8(0, 0, 255)), slint::Color::from_rgb_u8(255, 255, 255));
    }
}
