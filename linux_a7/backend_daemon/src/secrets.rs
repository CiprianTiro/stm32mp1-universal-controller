/*
 * secrets.rs -- devices' secrets (issue #40): the LG TV's pairing key, a
 * device password typed in the wizard, a vendor account token (#74).
 *
 * WHERE: /usr/local/etc/universal-controller/secrets/secrets.json -- a
 * folder only user hubd can open (mode 700, see backend-daemon-tmpfiles
 * .conf), a file only hubd can read (600), written through store.rs
 * (checksum, previous copy, safe against power loss). Apart from the
 * device registry on purpose: the registry's content goes to the cloud
 * (device shadows) and to every client; secrets go nowhere.
 *
 * THE RULES:
 *   - never logged: a secret's value is a `Secret`, whose Debug/Display
 *     print "***" -- a `println!("{secret}")` by mistake leaks nothing;
 *   - never sent to any client: no WebSocket request reads them (they're
 *     only written, by the wizard) and they're never part of a Device;
 *   - removed with their device.
 *
 * #50 moves this one file into OP-TEE secure storage (keys the normal
 * Linux can't read at all); nothing outside this file changes then.
 *
 * Shape: { "<device id>": { "<name>": "<value>", ... }, ... }, e.g.
 * { "tv": { "client_key": "..." } }.
 */
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::watch;

use crate::state::DeviceId;

const DEFAULT_SECRETS_DIR: &str = "/usr/local/etc/universal-controller/secrets";

/* Overridable with HUB_SECRETS_DIR (running the daemon on the PC). */
pub fn secrets_dir() -> PathBuf {
    std::env::var_os("HUB_SECRETS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SECRETS_DIR))
}

pub const SECRETS_SCHEMA: u32 = 1;

/* One secret value. Printing it (Debug, Display) shows "***"; the value
 * itself only through `expose()`, so every place that really uses it is
 * easy to find. */
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/* One device's secrets, by name. */
pub type DeviceSecrets = BTreeMap<String, Secret>;

pub struct Secrets {
    /* std Mutex: only held for quick map operations, never across .await. */
    all: Mutex<BTreeMap<DeviceId, DeviceSecrets>>,
    /* store::writer's channel: whatever is sent is saved. */
    save_tx: watch::Sender<Vec<u8>>,
}

impl Secrets {
    pub fn new(loaded: BTreeMap<DeviceId, DeviceSecrets>, save_tx: watch::Sender<Vec<u8>>) -> Self {
        Secrets {
            all: Mutex::new(loaded),
            save_tx,
        }
    }

    /* A device's secrets (a copy; empty if it has none). */
    pub fn get(&self, device: &str) -> DeviceSecrets {
        self.all.lock().unwrap().get(device).cloned().unwrap_or_default()
    }

    /* Sets (adds or replaces) some of a device's secrets. */
    pub fn set(&self, device: &str, values: DeviceSecrets) {
        let mut all = self.all.lock().unwrap();
        all.entry(device.to_string()).or_default().extend(values);
        self.save(&all);
    }

    /* Every device that has a secret of this name, with its value (issue
     * #72: the webhook tokens, looked up by token). */
    pub fn all_named(&self, name: &str) -> Vec<(DeviceId, Secret)> {
        self.all
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(device, secrets)| secrets.get(name).map(|s| (device.clone(), s.clone())))
            .collect()
    }

    /* Forgets all of a device's secrets (the device was removed). */
    pub fn remove(&self, device: &str) {
        let mut all = self.all.lock().unwrap();
        if all.remove(device).is_some() {
            self.save(&all);
        }
    }

    fn save(&self, all: &BTreeMap<DeviceId, DeviceSecrets>) {
        self.save_tx.send_replace(encode(all));
    }
}

fn encode(all: &BTreeMap<DeviceId, DeviceSecrets>) -> Vec<u8> {
    serde_json::to_vec(all).expect("secrets are always valid JSON")
}

/* For store::load_or_default. */
pub fn decode(schema: u32, payload: &[u8]) -> Result<BTreeMap<DeviceId, DeviceSecrets>, String> {
    if schema != SECRETS_SCHEMA {
        return Err(format!("unsupported secrets schema {schema}"));
    }
    serde_json::from_slice(payload).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets() -> (Secrets, watch::Receiver<Vec<u8>>) {
        let (save_tx, save_rx) = watch::channel(Vec::new());
        (Secrets::new(BTreeMap::new(), save_tx), save_rx)
    }

    #[test]
    fn a_secret_never_prints_its_value() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s}"), "***");
        assert_eq!(format!("{s:?}"), "***");
        let mut map = DeviceSecrets::new();
        map.insert("client_key".into(), s.clone());
        assert!(!format!("{map:?}").contains("hunter2"));
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn set_get_remove_and_every_change_is_saved() {
        let (s, mut saved) = secrets();
        assert!(s.get("tv").is_empty());
        s.set("tv", [("client_key".to_string(), Secret::new("abc"))].into());
        assert_eq!(s.get("tv")["client_key"].expose(), "abc");
        assert!(saved.has_changed().unwrap());
        let on_disk = decode(SECRETS_SCHEMA, &saved.borrow_and_update()).unwrap();
        assert_eq!(on_disk["tv"]["client_key"].expose(), "abc");

        /* Adding a second one keeps the first. */
        s.set("tv", [("pin".to_string(), Secret::new("1234"))].into());
        assert_eq!(s.get("tv").len(), 2);

        s.remove("tv");
        assert!(s.get("tv").is_empty());
        assert!(decode(SECRETS_SCHEMA, &saved.borrow_and_update()).unwrap().is_empty());

        /* Removing a device without secrets doesn't write. */
        s.remove("lamp");
        assert!(!saved.has_changed().unwrap());
    }
}
