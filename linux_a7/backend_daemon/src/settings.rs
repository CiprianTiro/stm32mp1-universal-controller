/*
 * settings.rs -- the hub's own settings (issue #39): the design preset the
 * screen shows, and the hub's time zone.
 *
 *   mode       "dark" | "light" | "auto" (auto: light by day, in the hub's
 *              time zone -- the UI decides the hours, see its theme.rs)
 *   accent     an accent id from the UI's ui/tokens.json ("sky", "emerald",
 *              ...). Not checked against that list here: it belongs to the
 *              UI, and an id the UI doesn't know simply shows the default
 *              accent. Only its shape is checked.
 *   density    "comfortable" | "compact"
 *   time_zone  an IANA time zone name ("Europe/Bucharest"), checked against
 *              the time zone database (chrono-tz). "UTC" until chosen. The
 *              hub's CLOCK stays on UTC (logs, certificates); this setting
 *              is for what people see, and later for automations (#47).
 *
 * Who may change them: the touchscreen and any paired client (the phone
 * app). They only change how things look, nothing security-relevant.
 *
 * Saved in settings.json next to the device registry (store.rs: checksum,
 * previous copy, safe against power loss) on every change. Everyone who
 * wants to know about changes holds a `watch` receiver (subscribe()): a
 * watch channel always holds the latest value, so a slow listener never
 * gets a backlog, only the newest settings -- all that matters here.
 */
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/* The settings as saved and as sent to clients. `default`: a settings.json
 * written by an older daemon (without a newer field) still loads, the
 * missing field taking its default. */
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HubSettings {
    pub mode: String,
    pub accent: String,
    pub density: String,
    pub time_zone: String,
}

impl Default for HubSettings {
    /* The look the hub had before #39 (and the one tokens.json names as
     * default). */
    fn default() -> Self {
        HubSettings {
            mode: "dark".into(),
            accent: "sky".into(),
            density: "comfortable".into(),
            time_zone: "UTC".into(),
        }
    }
}

/* A change: only the fields given are changed. */
#[derive(Debug, Default, Deserialize)]
pub struct Change {
    pub mode: Option<String>,
    pub accent: Option<String>,
    pub density: Option<String>,
    pub time_zone: Option<String>,
}

pub const SETTINGS_SCHEMA: u32 = 1;

pub struct Settings {
    current: watch::Sender<HubSettings>,
    /* store::writer's channel: whatever is sent is saved. */
    save_tx: watch::Sender<Vec<u8>>,
}

impl Settings {
    pub fn new(loaded: HubSettings, save_tx: watch::Sender<Vec<u8>>) -> Self {
        Settings {
            current: watch::Sender::new(loaded),
            save_tx,
        }
    }

    pub fn get(&self) -> HubSettings {
        self.current.borrow().clone()
    }

    /* Told about every change (the latest settings). */
    pub fn subscribe(&self) -> watch::Receiver<HubSettings> {
        self.current.subscribe()
    }

    /* Checks every given field first; only if ALL are fine is anything
     * changed (so a request with one bad field changes nothing). Then
     * saves and tells the subscribers -- only if something really changed,
     * so setting the same value again isn't an "event". */
    pub fn update(&self, change: Change) -> Result<HubSettings, String> {
        let mut new = self.get();
        if let Some(mode) = change.mode {
            check_one_of("mode", &mode, &["dark", "light", "auto"])?;
            new.mode = mode;
        }
        if let Some(accent) = change.accent {
            check_accent(&accent)?;
            new.accent = accent;
        }
        if let Some(density) = change.density {
            check_one_of("density", &density, &["comfortable", "compact"])?;
            new.density = density;
        }
        if let Some(time_zone) = change.time_zone {
            check_time_zone(&time_zone)?;
            new.time_zone = time_zone;
        }
        if new != self.get() {
            self.save_tx.send_replace(encode(&new));
            self.current.send_replace(new.clone());
            println!(
                "settings: now {} / {} / {}, time zone {}",
                new.mode, new.accent, new.density, new.time_zone
            );
        }
        Ok(new)
    }
}

fn check_one_of(what: &str, value: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(format!("unknown {what} {value:?} (one of: {})", allowed.join(", ")))
    }
}

/* An accent id: 1-24 lowercase letters (see the header for why not the
 * list itself). */
fn check_accent(accent: &str) -> Result<(), String> {
    if (1..=24).contains(&accent.len()) && accent.bytes().all(|b| b.is_ascii_lowercase()) {
        Ok(())
    } else {
        Err(format!("bad accent {accent:?} (an id like \"sky\")"))
    }
}

fn check_time_zone(time_zone: &str) -> Result<(), String> {
    time_zone
        .parse::<chrono_tz::Tz>()
        .map(|_| ())
        .map_err(|_| format!("unknown time zone {time_zone:?} (an IANA name like \"Europe/Bucharest\")"))
}

fn encode(settings: &HubSettings) -> Vec<u8> {
    serde_json::to_vec(settings).expect("HubSettings is always valid JSON")
}

/* For store::load_or_default. A value that no longer passes the checks
 * (e.g. a time zone removed from the database in a later version) is reset
 * to its default rather than making the whole file unusable. */
pub fn decode(schema: u32, payload: &[u8]) -> Result<HubSettings, String> {
    if schema != SETTINGS_SCHEMA {
        return Err(format!("unsupported settings schema {schema}"));
    }
    let mut settings: HubSettings = serde_json::from_slice(payload).map_err(|e| e.to_string())?;
    let defaults = HubSettings::default();
    if check_one_of("mode", &settings.mode, &["dark", "light", "auto"]).is_err() {
        settings.mode = defaults.mode;
    }
    if check_accent(&settings.accent).is_err() {
        settings.accent = defaults.accent;
    }
    if check_one_of("density", &settings.density, &["comfortable", "compact"]).is_err() {
        settings.density = defaults.density;
    }
    if check_time_zone(&settings.time_zone).is_err() {
        settings.time_zone = defaults.time_zone;
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> (Settings, watch::Receiver<Vec<u8>>) {
        let (save_tx, save_rx) = watch::channel(Vec::new());
        (Settings::new(HubSettings::default(), save_tx), save_rx)
    }

    #[test]
    fn a_valid_change_is_applied_saved_and_announced() {
        let (s, mut saved) = settings();
        let mut rx = s.subscribe();
        let new = s
            .update(Change {
                mode: Some("light".into()),
                time_zone: Some("Europe/Bucharest".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(new.mode, "light");
        assert_eq!(new.accent, "sky");
        assert_eq!(new.time_zone, "Europe/Bucharest");
        assert!(rx.has_changed().unwrap());
        assert!(saved.has_changed().unwrap());
        assert_eq!(decode(SETTINGS_SCHEMA, &saved.borrow_and_update()).unwrap(), new);
    }

    #[test]
    fn one_bad_field_changes_nothing() {
        let (s, _saved) = settings();
        let err = s
            .update(Change {
                mode: Some("light".into()),
                time_zone: Some("Mars/Olympus_Mons".into()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.contains("time zone"), "{err}");
        assert_eq!(s.get(), HubSettings::default());
        assert!(s.update(Change { density: Some("tiny".into()), ..Default::default() }).is_err());
        assert!(s.update(Change { accent: Some("Sky Blue!".into()), ..Default::default() }).is_err());
    }

    #[test]
    fn the_same_value_again_is_not_an_event() {
        let (s, _saved) = settings();
        let mut rx = s.subscribe();
        rx.borrow_and_update();
        s.update(Change { mode: Some("dark".into()), ..Default::default() }).unwrap();
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn a_saved_file_with_bad_or_missing_values_still_loads() {
        let loaded = decode(SETTINGS_SCHEMA, br#"{"mode":"sepia","time_zone":"Europe/Atlantis"}"#).unwrap();
        assert_eq!(loaded, HubSettings::default());
        assert!(decode(2, b"{}").is_err());
    }
}
