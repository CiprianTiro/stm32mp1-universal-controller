//! automation_text.rs -- automations in words, and the simple editor's
//! automations as JSON (issue #47). Pure: no Slint, no network -- unit
//! tested on the PC through tools/ui_preview (like zones.rs), since this
//! crate itself only builds for the board.
//!
//! backend_daemon's automations.rs is the definition of the format; this
//! file only reads it (for the one-line summaries the Automations page
//! shows) and writes the few shapes the touchscreen's editor can make:
//!   "At a time" / "At sunrise" / "At sunset" (+ days)  -> run a scene
//!   "When <device> turns on / off"                      -> run a scene
//! Anything else (conditions, several steps, thresholds) is made with the
//! app or hub_ws.py, and only shown here.

use serde_json::{json, Value};

/// The simple editor's "When", as app.slint's numbers say it.
pub const WHEN_TIME: i32 = 0;
pub const WHEN_SUNRISE: i32 = 1;
pub const WHEN_SUNSET: i32 = 2;
pub const WHEN_TURNS_ON: i32 = 3;
pub const WHEN_TURNS_OFF: i32 = 4;

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Days as the editor keeps them: bit 0 = Monday ... bit 6 = Sunday.
/// All seven (or none) = every day, which the format writes as no days.
fn days_json(mask: i32) -> Vec<&'static str> {
    let days: Vec<&str> = (0..7).filter(|i| mask & (1 << i) != 0).map(|i| DAYS[i]).collect();
    if days.len() == 7 {
        Vec::new()
    } else {
        days
    }
}

/// "" (every day), "Mon–Fri", "Sat, Sun", "Mon, Wed, Fri".
pub fn days_text(days: &[String]) -> String {
    let mut on: Vec<usize> = days.iter().filter_map(|d| DAYS.iter().position(|x| x == d)).collect();
    on.sort();
    on.dedup();
    if on.is_empty() || on.len() == 7 {
        return String::new();
    }
    // A run of 3 or more days in a row: "Mon–Fri".
    let consecutive = on.windows(2).all(|w| w[1] == w[0] + 1);
    if on.len() >= 3 && consecutive {
        return format!("{}\u{2013}{}", DAY_NAMES[on[0]], DAY_NAMES[*on.last().unwrap()]);
    }
    on.iter().map(|&i| DAY_NAMES[i]).collect::<Vec<_>>().join(", ")
}

/// "15 min before sunset", "sunrise", "10 min after sunrise".
pub fn sun_text(event: &str, offset_min: i64) -> String {
    match offset_min {
        0 => event.to_string(),
        o if o < 0 => format!("{} min before {event}", -o),
        o => format!("{o} min after {event}"),
    }
}

/// One trigger in words. `device_name` turns a device id into its name.
fn trigger_text(trigger: &Value, device_name: &dyn Fn(&str) -> String) -> String {
    let text = |key: &str| trigger[key].as_str().unwrap_or_default().to_string();
    let days = |t: &Value| {
        let days: Vec<String> = t["days"].as_array().map_or(vec![], |a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect());
        match days_text(&days) {
            d if d.is_empty() => String::new(),
            d => format!(", {d}"),
        }
    };
    match trigger["type"].as_str() {
        Some("time") => format!("At {}{}", text("at"), days(trigger)),
        Some("sun") => {
            let mut what = sun_text(&text("event"), trigger["offset_min"].as_i64().unwrap_or(0));
            if let Some(first) = what.get(..1) {
                what = first.to_uppercase() + &what[1..];
            }
            format!("{what}{}", days(trigger))
        }
        Some("state") => {
            let name = device_name(&text("device"));
            match (text("field").as_str(), trigger.get("to")) {
                ("on", Some(Value::Bool(true))) => format!("When {name} turns on"),
                ("on", Some(Value::Bool(false))) => format!("When {name} turns off"),
                (field, Some(to)) => format!("When {name}'s {field} becomes {to}"),
                (field, None) => format!("When {name}'s {field} changes"),
            }
        }
        Some("threshold") => {
            let name = device_name(&text("device"));
            let field = text("field");
            match (trigger["above"].as_f64(), trigger["below"].as_f64()) {
                (Some(a), _) => format!("When {name}'s {field} goes above {a}"),
                (_, Some(b)) => format!("When {name}'s {field} goes below {b}"),
                _ => format!("When {name}'s {field} changes"),
            }
        }
        _ => "When something happens".into(),
    }
}

/// An automation in one line: "At 07:30, Mon–Fri → Cozy", "When TV turns
/// on → 2 steps (+1 condition)". Only the first trigger is spelled out.
pub fn summary(automation: &Value, device_name: &dyn Fn(&str) -> String, scene_name: &dyn Fn(&str) -> String) -> String {
    let triggers = automation["triggers"].as_array().cloned().unwrap_or_default();
    let mut when = triggers.first().map_or("Never".into(), |t| trigger_text(t, device_name));
    if triggers.len() > 1 {
        when += &format!(" (or {} more)", triggers.len() - 1);
    }
    let steps = automation["steps"].as_array().cloned().unwrap_or_default();
    let what = match steps.as_slice() {
        [one] if one["scene"].is_string() => scene_name(one["scene"].as_str().unwrap_or_default()),
        [one] => {
            let device = device_name(one["device"].as_str().unwrap_or_default());
            match one["capability"].as_str() {
                Some("switch") if one["value"]["on"] == true => format!("{device} on"),
                Some("switch") if one["value"]["on"] == false => format!("{device} off"),
                Some(cap) => format!("{device}: {cap}"),
                None => device,
            }
        }
        many => format!("{} steps", many.len()),
    };
    let conditions = automation["conditions"].as_array().map_or(0, Vec::len);
    let only_if = match conditions {
        0 => String::new(),
        1 => " (if 1 condition)".into(),
        n => format!(" (if {n} conditions)"),
    };
    format!("{when} \u{2192} {what}{only_if}")
}

/// What the editor saves: one trigger, the scene as the only step.
/// `hour`/`minute` for WHEN_TIME, `offset` (minutes) for sunrise/sunset,
/// `device` for turns on/off; `days` (bit mask) for the scheduled ones.
#[allow(clippy::too_many_arguments)] /* the editor's fields, one each */
pub fn simple_automation(name: &str, when: i32, hour: i32, minute: i32, offset: i32, days: i32, device: &str, scene: &str) -> Value {
    let days = days_json(days);
    let trigger = match when {
        WHEN_TIME => json!({ "type": "time", "at": format!("{hour:02}:{minute:02}"), "days": days }),
        WHEN_SUNRISE | WHEN_SUNSET => json!({
            "type": "sun",
            "event": if when == WHEN_SUNRISE { "sunrise" } else { "sunset" },
            "offset_min": offset,
            "days": days,
        }),
        _ => json!({
            "type": "state", "device": device, "capability": "switch", "field": "on",
            "to": when == WHEN_TURNS_ON,
        }),
    };
    json!({ "name": name.trim(), "triggers": [trigger], "steps": [{ "scene": scene }] })
}

/// The name the editor suggests: "Cozy at 07:30", "Cozy at sunset",
/// "Cozy when TV turns on".
pub fn suggested_name(when: i32, hour: i32, minute: i32, offset: i32, device_name: &str, scene_name: &str) -> String {
    let sun = |event: &str| match offset {
        0 => format!("at {event}"),
        _ => sun_text(event, offset.into()),
    };
    let when = match when {
        WHEN_TIME => format!("at {hour:02}:{minute:02}"),
        WHEN_SUNRISE => sun("sunrise"),
        WHEN_SUNSET => sun("sunset"),
        WHEN_TURNS_ON => format!("when {device_name} turns on"),
        _ => format!("when {device_name} turns off"),
    };
    let mut name = format!("{scene_name} {when}");
    // The backend allows 64 characters.
    if name.chars().count() > 64 {
        name = name.chars().take(64).collect();
    }
    name.trim().to_string()
}

/// "44.43, 26.10" (or "44.43 26.10") -> (44.43, 26.10), checked.
pub fn parse_location(text: &str) -> Result<(f64, f64), String> {
    let parts: Vec<&str> = text.split([',', ' ']).filter(|p| !p.is_empty()).collect();
    let [lat, lon] = parts.as_slice() else {
        return Err("Type the two numbers, latitude then longitude, e.g. 44.43, 26.10".into());
    };
    let number = |t: &str| t.parse::<f64>().ok().filter(|v| v.is_finite());
    match (number(lat), number(lon)) {
        (Some(lat), Some(lon)) if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) => Ok((lat, lon)),
        _ => Err("Latitude is -90 to 90, longitude -180 to 180 (south and west are negative)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(id: &str) -> String {
        match id {
            "tv" => "LG TV".into(),
            "cozy" => "Cozy".into(),
            other => other.into(),
        }
    }

    #[test]
    fn days_in_words() {
        let d = |days: &[&str]| days_text(&days.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(d(&[]), "");
        assert_eq!(d(&["mon", "tue", "wed", "thu", "fri"]), "Mon\u{2013}Fri");
        assert_eq!(d(&["sat", "sun"]), "Sat, Sun");
        assert_eq!(d(&["fri", "mon", "wed"]), "Mon, Wed, Fri");
        assert_eq!(d(&DAYS), "");
    }

    #[test]
    fn summaries() {
        let s = |a: Value| summary(&a, &names, &names);
        assert_eq!(
            s(json!({"triggers": [{"type": "time", "at": "07:30", "days": ["mon", "tue", "wed", "thu", "fri"]}], "steps": [{"scene": "cozy"}]})),
            "At 07:30, Mon\u{2013}Fri \u{2192} Cozy"
        );
        assert_eq!(
            s(json!({"triggers": [{"type": "sun", "event": "sunset", "offset_min": -15}], "steps": [{"scene": "cozy"}]})),
            "15 min before sunset \u{2192} Cozy"
        );
        assert_eq!(
            s(json!({"triggers": [{"type": "state", "device": "tv", "capability": "switch", "field": "on", "to": true}],
                     "conditions": [{"type": "sun", "is": "night"}],
                     "steps": [{"device": "lamp", "capability": "switch", "value": {"on": false}}]})),
            "When LG TV turns on \u{2192} lamp off (if 1 condition)"
        );
        assert_eq!(
            s(json!({"triggers": [{"type": "threshold", "device": "plug", "capability": "energy", "field": "power_w", "above": 5.0}],
                     "steps": [{"scene": "a"}, {"scene": "b"}]})),
            "When plug's power_w goes above 5 \u{2192} 2 steps"
        );
    }

    #[test]
    fn the_editor_makes_valid_shapes() {
        let a = simple_automation(" Cozy at 07:05 ", WHEN_TIME, 7, 5, 0, 0b0011111, "", "cozy");
        assert_eq!(a["name"], "Cozy at 07:05");
        assert_eq!(a["triggers"][0], json!({"type": "time", "at": "07:05", "days": ["mon", "tue", "wed", "thu", "fri"]}));
        assert_eq!(a["steps"], json!([{"scene": "cozy"}]));
        /* Every day: no days at all. */
        let a = simple_automation("x", WHEN_SUNSET, 0, 0, -15, 0b1111111, "", "cozy");
        assert_eq!(a["triggers"][0], json!({"type": "sun", "event": "sunset", "offset_min": -15, "days": []}));
        let a = simple_automation("x", WHEN_TURNS_OFF, 0, 0, 0, 0, "tv", "cozy");
        assert_eq!(a["triggers"][0], json!({"type": "state", "device": "tv", "capability": "switch", "field": "on", "to": false}));
    }

    #[test]
    fn suggested_names() {
        assert_eq!(suggested_name(WHEN_TIME, 7, 5, 0, "", "Cozy"), "Cozy at 07:05");
        assert_eq!(suggested_name(WHEN_SUNSET, 0, 0, 0, "", "Cozy"), "Cozy at sunset");
        assert_eq!(suggested_name(WHEN_SUNRISE, 0, 0, 10, "", "Cozy"), "Cozy 10 min after sunrise");
        assert_eq!(suggested_name(WHEN_TURNS_ON, 0, 0, 0, "LG TV", "Cozy"), "Cozy when LG TV turns on");
    }

    #[test]
    fn locations() {
        assert_eq!(parse_location("44.43, 26.10"), Ok((44.43, 26.1)));
        assert_eq!(parse_location(" -33.9 18.4 "), Ok((-33.9, 18.4)));
        assert!(parse_location("44.43").is_err());
        assert!(parse_location("95, 10").is_err());
        assert!(parse_location("a, b").is_err());
    }
}
