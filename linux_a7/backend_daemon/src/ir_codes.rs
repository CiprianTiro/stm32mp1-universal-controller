/*
 * ir_codes.rs -- the IR codes learned for each IR blaster device (issue
 * #42): which button sends which code.
 *
 * WHY ON THE HUB, NOT ON THE BLASTER. The blaster is replaceable (a new
 * one, or a factory reset): the hub keeps what was taught, so a new blaster
 * only needs pairing, not re-teaching every button. And the hub's backups
 * (#56) include it.
 *
 * WHY NOT IN THE DEVICE'S CONFIG. A config value is at most 256 characters
 * (device.rs), and the registry goes to every client and the cloud. A raw
 * code from an air-conditioner remote is several KB, and nobody outside the
 * hub needs the codes: clients only see the button NAMES (the `remote`
 * capability).
 *
 * WHERE: devices' data dir, "ir-codes.json", written through store.rs
 * (checksum, previous copy, safe against power loss) -- the same way as
 * secrets.rs, and removed with its device the same way.
 *
 * Shape: { "<device id>": [ {"button": "Power", "code": {...}}, ... ] }.
 * A LIST, not a map: the order is the order the buttons were taught, which
 * is the order a screen shows them in. `code` is the blaster's own JSON
 * (firmware_ir_blaster/PROTOCOL.md, "Codes") for a taught button, kept as
 * it came; for a button from the code library (#82) it's the library's
 * code, e.g. {"proto": "rc5", "address": 0, "command": 12}, which
 * ir_encode.rs turns into the blaster's at every press.
 */
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::sync::watch;

use crate::device::{valid_button_name, MAX_REMOTE_BUTTONS};
use crate::state::DeviceId;

pub const IR_CODES_SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LearnedButton {
    pub button: String,
    pub code: serde_json::Value,
}

pub struct IrCodes {
    /* std Mutex: only held for quick list operations, never across .await. */
    all: Mutex<BTreeMap<DeviceId, Vec<LearnedButton>>>,
    /* store::writer's channel: whatever is sent is saved. */
    save_tx: watch::Sender<Vec<u8>>,
}

impl IrCodes {
    pub fn new(loaded: BTreeMap<DeviceId, Vec<LearnedButton>>, save_tx: watch::Sender<Vec<u8>>) -> Self {
        IrCodes {
            all: Mutex::new(loaded),
            save_tx,
        }
    }

    /* Kept in memory only (tests, and a Control made without the real
     * store): the receiving end of the channel is simply dropped. */
    pub fn in_memory() -> Self {
        let (save_tx, _) = watch::channel(Vec::new());
        IrCodes::new(BTreeMap::new(), save_tx)
    }

    /* A device's buttons, in the order they were taught (a copy). */
    pub fn get(&self, device: &str) -> Vec<LearnedButton> {
        self.all.lock().unwrap().get(device).cloned().unwrap_or_default()
    }

    /* The code of one button, if taught. */
    pub fn code(&self, device: &str, button: &str) -> Option<serde_json::Value> {
        self.all
            .lock()
            .unwrap()
            .get(device)?
            .iter()
            .find(|b| b.button == button)
            .map(|b| b.code.clone())
    }

    /* Teaches (or re-teaches) a button: an existing one keeps its place in
     * the list, a new one goes at the end. Returns the button names. */
    pub fn learn(&self, device: &str, button: &str, code: serde_json::Value) -> Vec<String> {
        let mut all = self.all.lock().unwrap();
        let buttons = all.entry(device.to_string()).or_default();
        match buttons.iter_mut().find(|b| b.button == button) {
            Some(existing) => existing.code = code,
            None => buttons.push(LearnedButton {
                button: button.to_string(),
                code,
            }),
        }
        let names = names_of(buttons);
        self.save(&all);
        names
    }

    /* Adds a code set's buttons from the library (issue #82) after the
     * device's own. A button already there keeps its code (what was
     * taught wins), names a remote can't have are left out, and at most
     * MAX_REMOTE_BUTTONS in all. Returns the button names and how many
     * were added. */
    pub fn add_set(&self, device: &str, set: Vec<(String, serde_json::Value)>) -> (Vec<String>, usize) {
        let mut all = self.all.lock().unwrap();
        let buttons = all.entry(device.to_string()).or_default();
        let mut added = 0;
        for (button, code) in set {
            if buttons.len() >= MAX_REMOTE_BUTTONS {
                break;
            }
            if !valid_button_name(&button) || buttons.iter().any(|b| b.button == button) {
                continue;
            }
            buttons.push(LearnedButton { button, code });
            added += 1;
        }
        let names = names_of(buttons);
        if added > 0 {
            self.save(&all);
        }
        (names, added)
    }

    /* Forgets one button. Err if the device has no such button. */
    pub fn forget(&self, device: &str, button: &str) -> Result<Vec<String>, String> {
        let mut all = self.all.lock().unwrap();
        let buttons = all.get_mut(device).ok_or_else(|| format!("no button {button:?}"))?;
        let before = buttons.len();
        buttons.retain(|b| b.button != button);
        if buttons.len() == before {
            return Err(format!("no button {button:?}"));
        }
        let names = names_of(buttons);
        self.save(&all);
        Ok(names)
    }

    /* Renames a button, keeping its code and place. */
    pub fn rename(&self, device: &str, button: &str, to: &str) -> Result<Vec<String>, String> {
        let mut all = self.all.lock().unwrap();
        let buttons = all.get_mut(device).ok_or_else(|| format!("no button {button:?}"))?;
        if button != to && buttons.iter().any(|b| b.button == to) {
            return Err(format!("there is already a button {to:?}"));
        }
        let found = buttons
            .iter_mut()
            .find(|b| b.button == button)
            .ok_or_else(|| format!("no button {button:?}"))?;
        found.button = to.to_string();
        let names = names_of(buttons);
        self.save(&all);
        Ok(names)
    }

    /* Forgets all of a device's codes (the device was removed). */
    pub fn remove(&self, device: &str) {
        let mut all = self.all.lock().unwrap();
        if all.remove(device).is_some() {
            self.save(&all);
        }
    }

    fn save(&self, all: &BTreeMap<DeviceId, Vec<LearnedButton>>) {
        self.save_tx.send_replace(encode(all));
    }
}

fn names_of(buttons: &[LearnedButton]) -> Vec<String> {
    buttons.iter().map(|b| b.button.clone()).collect()
}

fn encode(all: &BTreeMap<DeviceId, Vec<LearnedButton>>) -> Vec<u8> {
    serde_json::to_vec(all).expect("IR codes are always valid JSON")
}

/* For store::load_or_default. */
pub fn decode(schema: u32, payload: &[u8]) -> Result<BTreeMap<DeviceId, Vec<LearnedButton>>, String> {
    if schema != IR_CODES_SCHEMA {
        return Err(format!("unsupported IR codes schema {schema}"));
    }
    serde_json::from_slice(payload).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn codes() -> (IrCodes, watch::Receiver<Vec<u8>>) {
        let (save_tx, save_rx) = watch::channel(Vec::new());
        (IrCodes::new(BTreeMap::new(), save_tx), save_rx)
    }

    fn nec(command: u8) -> serde_json::Value {
        json!({"proto": "nec", "address": 0, "command": command})
    }

    #[test]
    fn learn_keeps_order_and_relearn_keeps_place() {
        let (c, mut saved) = codes();
        assert_eq!(c.learn("astro", "Power", nec(0x45)), vec!["Power"]);
        assert_eq!(c.learn("astro", "Red", nec(0x16)), vec!["Power", "Red"]);
        /* Teaching "Power" again replaces its code, keeps its place. */
        assert_eq!(c.learn("astro", "Power", nec(0x40)), vec!["Power", "Red"]);
        assert_eq!(c.code("astro", "Power"), Some(nec(0x40)));
        assert_eq!(c.code("astro", "Blue"), None);
        /* Every change is saved, and reads back the same. */
        assert!(saved.has_changed().unwrap());
        let on_disk = decode(IR_CODES_SCHEMA, &saved.borrow_and_update()).unwrap();
        assert_eq!(on_disk["astro"], c.get("astro"));
    }

    #[test]
    fn forget_and_rename() {
        let (c, _saved) = codes();
        c.learn("astro", "Power", nec(0x45));
        c.learn("astro", "Red", nec(0x16));
        assert_eq!(c.rename("astro", "Red", "Colour"), Ok(vec!["Power".to_string(), "Colour".to_string()]));
        assert!(c.rename("astro", "Colour", "Power").unwrap_err().contains("already"));
        assert!(c.rename("astro", "Blue", "Green").is_err());
        assert_eq!(c.forget("astro", "Power"), Ok(vec!["Colour".to_string()]));
        assert!(c.forget("astro", "Power").is_err());
        assert!(c.forget("lamp", "Power").is_err());
    }

    #[test]
    fn add_set_keeps_taught_buttons() {
        let (c, mut saved) = codes();
        c.learn("strip", "Power", nec(0x45));
        saved.borrow_and_update();
        let set = vec![
            ("Power".to_string(), nec(0x40)),
            ("Red".to_string(), nec(0x58)),
            (" bad name".to_string(), nec(0x01)),
        ];
        assert_eq!(c.add_set("strip", set), (vec!["Power".to_string(), "Red".to_string()], 1));
        /* The taught Power keeps its code. */
        assert_eq!(c.code("strip", "Power"), Some(nec(0x45)));
        assert!(saved.has_changed().unwrap());
        /* Nothing new: nothing saved. */
        saved.borrow_and_update();
        assert_eq!(c.add_set("strip", vec![("Red".to_string(), nec(0x58))]).1, 0);
        assert!(!saved.has_changed().unwrap());
        /* The limit holds. */
        let many = (0..120).map(|i| (format!("B{i}"), nec(0))).collect();
        assert_eq!(c.add_set("strip", many).0.len(), MAX_REMOTE_BUTTONS);
    }

    #[test]
    fn remove_forgets_everything_and_only_saves_on_change() {
        let (c, mut saved) = codes();
        c.learn("astro", "Power", nec(0x45));
        saved.borrow_and_update();
        c.remove("astro");
        assert!(c.get("astro").is_empty());
        assert!(saved.has_changed().unwrap());
        saved.borrow_and_update();
        c.remove("lamp");
        assert!(!saved.has_changed().unwrap());
    }
}
