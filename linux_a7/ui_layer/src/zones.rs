// zones.rs -- the time zone lists and the hub's local time (issue #39).
//
// The hub's clock runs on UTC; its time zone is a setting (backend_daemon's
// settings.rs) used for what people see: "auto" light/dark by the time of
// day, the time on the Settings page, and later automations (#47). The time
// zone database is compiled into this program (chrono-tz) -- the board has
// no /usr/share/zoneinfo, and doesn't need one.
//
// On the touchscreen a list of ~400 zones would be unusable, so the choice
// has two steps: a region ("Europe"), then a place in it ("Bucharest").

use chrono::{Timelike, Utc};
use chrono_tz::{Tz, TZ_VARIANTS};

/// The IANA regions offered. Zones outside them are legacy aliases
/// ("US/Eastern", "GB", "Etc/GMT+3") that the canonical names already
/// cover -- except "UTC", offered on its own.
const REGIONS: [&str; 10] = [
    "Africa", "America", "Antarctica", "Arctic", "Asia", "Atlantic", "Australia", "Europe", "Indian", "Pacific",
];

/// A row of the lists: label, value, and whether it's the current zone
/// (or the region holding it).
pub struct Zone {
    pub label: String,
    pub value: String,
    pub marked: bool,
}

/// Step 1: "UTC", then the regions. A region's value ends in "/", which is
/// how main.rs tells a region from a zone when one is tapped.
pub fn regions(current: &str) -> Vec<Zone> {
    let mut list = vec![Zone {
        label: "UTC (no time zone)".into(),
        value: "UTC".into(),
        marked: current == "UTC",
    }];
    list.extend(REGIONS.iter().map(|region| {
        let value = format!("{region}/");
        Zone {
            label: region.to_string(),
            marked: current.starts_with(&value),
            value,
        }
    }));
    list
}

/// Step 2: the zones in a region ("Europe/"), sorted by name, labelled
/// without the region and with spaces: "America/Argentina/Buenos_Aires"
/// shows as "Argentina / Buenos Aires".
pub fn places(region: &str, current: &str) -> Vec<Zone> {
    let mut list: Vec<Zone> = TZ_VARIANTS
        .iter()
        .map(|tz| tz.name())
        .filter_map(|name| {
            let place = name.strip_prefix(region)?;
            Some(Zone {
                label: place.replace('_', " ").replace('/', " / "),
                value: name.to_string(),
                marked: name == current,
            })
        })
        .collect();
    list.sort_by(|a, b| a.label.cmp(&b.label));
    list
}

/// The current hour (0-23) and time ("14:05") in a time zone. An unknown
/// name (which the backend's check makes impossible) counts as UTC.
pub fn local_time(time_zone: &str) -> (u32, String) {
    let tz: Tz = time_zone.parse().unwrap_or(Tz::UTC);
    let now = Utc::now().with_timezone(&tz);
    (now.hour(), now.format("%H:%M").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_mark_the_current_one() {
        let list = regions("Europe/Bucharest");
        assert_eq!(list[0].value, "UTC");
        assert!(list.iter().any(|z| z.value == "Europe/" && z.marked));
        assert_eq!(list.iter().filter(|z| z.marked).count(), 1);
    }

    #[test]
    fn places_are_labelled_without_the_region() {
        let list = places("America/", "America/Argentina/Buenos_Aires");
        let ba = list.iter().find(|z| z.value == "America/Argentina/Buenos_Aires").unwrap();
        assert_eq!(ba.label, "Argentina / Buenos Aires");
        assert!(ba.marked);
        assert!(places("Europe/", "").iter().any(|z| z.label == "Bucharest"));
    }

    #[test]
    fn local_time_uses_the_zone() {
        let (hour, text) = local_time("UTC");
        assert_eq!(text.len(), 5);
        assert!(hour < 24);
        // Unknown: treated as UTC, not a crash.
        assert_eq!(local_time("Mars/Olympus").0, local_time("UTC").0);
    }
}
