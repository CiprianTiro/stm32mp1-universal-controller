/*
 * state.rs -- the hub's devices, owned by exactly one task (an "actor").
 *
 * THE ACTOR IDEA. The device list lives in ONE task (`run` below); no other
 * task ever holds a reference to it. Everyone else sends a message (Msg)
 * into its mailbox -- a Tokio `mpsc` channel ("multi-producer, single-
 * consumer": many senders, one receiver) -- and, when they need an answer,
 * includes a `oneshot` channel: a return envelope good for exactly one
 * reply. Because only one task touches the data, there are no locks, and
 * messages are handled strictly in order. See ARCHITECTURE.md's "State
 * ownership" section for why.
 *
 * WHAT'S STORED (issue #34): complete devices, with their capabilities (see
 * device.rs). This file doesn't know what a dimmer is -- every change goes
 * through device.rs's rules (Device::check, set_capability), and is
 * refused with a reason if it breaks them.
 *
 * WHO HEARS ABOUT CHANGES:
 *   - `changed_tx` (issue #29): a `watch` bell without data; mqtt.rs wakes
 *     on it and reports to the cloud.
 *   - `events_tx` (issue #34): a `broadcast` channel -- every subscriber
 *     gets its own copy of every event. ws.rs subscribes once per client
 *     that asked for events, so the touchscreen and the app see a change
 *     the moment it happens instead of polling.
 *   - `save_tx` (issue #33): store.rs's writer, which puts the registry on
 *     the flash in the background.
 */
use std::collections::{BTreeMap, HashMap};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::device::{self, Device, Origin};

/* A device's id, e.g. "lamp-1" (also its AWS shadow's name). A type alias:
 * just a String with a name that says what it's for. */
pub type DeviceId = String;

/* What happened to a device, for event subscribers (ws.rs). */
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /* Added, or changed in any way: the device as it is now. */
    Changed(Device),
    Removed(DeviceId),
}

/* Every request the actor understands. Each carries its reply envelope;
 * a sender that doesn't care about the answer can drop the receiving half
 * (the actor ignores a failed reply). */
pub enum Msg {
    GetDevice {
        id: DeviceId,
        reply: oneshot::Sender<Option<Device>>,
    },
    GetAllDevices {
        reply: oneshot::Sender<HashMap<DeviceId, Device>>,
    },
    /* A new device. Refused if the id is taken or the device breaks the
     * rules. */
    AddDevice {
        device: Device,
        reply: oneshot::Sender<Result<Device, String>>,
    },
    /* Rename it or move it to another room (None = leave as is). */
    UpdateInfo {
        id: DeviceId,
        name: Option<String>,
        room: Option<String>,
        reply: oneshot::Sender<Result<Device, String>>,
    },
    /* Change one capability. `origin` says who's asking (device.rs):
     * clients may only change virtual devices this way -- hardware state
     * only changes when the hardware confirms (Origin::Device, sent by its
     * driver; see control.rs). */
    SetCapability {
        id: DeviceId,
        capability: String,
        value: serde_json::Value,
        origin: Origin,
        reply: oneshot::Sender<Result<Device, String>>,
    },
    RemoveDevice {
        id: DeviceId,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/* The channels the actor needs besides its mailbox (see the header). */
pub struct Outputs {
    pub changed_tx: watch::Sender<()>,
    pub events_tx: broadcast::Sender<Event>,
    pub save_tx: watch::Sender<Vec<u8>>,
}

/* THE ACTOR. Calling it doesn't start anything -- like every async fn, it
 * only builds a Future; main.rs spawns it (tokio::spawn), exactly once.
 * `devices` is the registry as loaded from disk. The loop ends when every
 * Sender of the mailbox is gone (the daemon shutting down). */
pub async fn run(mut rx: mpsc::Receiver<Msg>, mut devices: HashMap<DeviceId, Device>, out: Outputs) {
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::GetDevice { id, reply } => {
                /* .cloned(): the answer travels to another task, so it gets
                 * its own copy, never a reference into our map. */
                let _ = reply.send(devices.get(&id).cloned());
            }
            Msg::GetAllDevices { reply } => {
                let _ = reply.send(devices.clone());
            }
            Msg::AddDevice { device, reply } => {
                let result = add(&mut devices, device);
                if let Ok(device) = &result {
                    changed(&out, &devices, Event::Changed(device.clone()));
                }
                let _ = reply.send(result);
            }
            Msg::UpdateInfo { id, name, room, reply } => {
                let result = update_info(&mut devices, &id, name, room);
                if let Ok((device, true)) = &result {
                    changed(&out, &devices, Event::Changed(device.clone()));
                }
                let _ = reply.send(result.map(|(device, _)| device));
            }
            Msg::SetCapability {
                id,
                capability,
                value,
                origin,
                reply,
            } => {
                let result = set(&mut devices, &id, &capability, value, origin);
                if let Ok((device, true)) = &result {
                    changed(&out, &devices, Event::Changed(device.clone()));
                }
                let _ = reply.send(result.map(|(device, _)| device));
            }
            Msg::RemoveDevice { id, reply } => {
                let result = remove(&mut devices, &id);
                if result.is_ok() {
                    changed(&out, &devices, Event::Removed(id));
                }
                let _ = reply.send(result);
            }
        }
    }
}

/* Tells everyone about a change: the registry writer (the whole registry,
 * encoded), the cloud's bell, and the event subscribers. `send` on a
 * broadcast channel fails only when nobody is subscribed -- fine. */
fn changed(out: &Outputs, devices: &HashMap<DeviceId, Device>, event: Event) {
    out.save_tx.send_replace(encode_registry(devices));
    out.changed_tx.send_replace(());
    let _ = out.events_tx.send(event);
}

fn add(devices: &mut HashMap<DeviceId, Device>, device: Device) -> Result<Device, String> {
    device.check()?;
    if devices.contains_key(&device.id) {
        return Err(format!("a device with id {:?} already exists", device.id));
    }
    devices.insert(device.id.clone(), device.clone());
    Ok(device)
}

/* The functions below return the device and whether anything actually
 * changed: setting a lamp that's already on to "on" costs no flash write
 * and sends no event. */

fn update_info(
    devices: &mut HashMap<DeviceId, Device>,
    id: &str,
    name: Option<String>,
    room: Option<String>,
) -> Result<(Device, bool), String> {
    let current = devices.get(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    let mut new = current.clone();
    if let Some(name) = name {
        new.name = name;
    }
    if let Some(room) = room {
        new.room = room;
    }
    if new == *current {
        return Ok((new, false));
    }
    new.check()?;
    devices.insert(id.to_string(), new.clone());
    Ok((new, true))
}

fn set(
    devices: &mut HashMap<DeviceId, Device>,
    id: &str,
    capability: &str,
    value: serde_json::Value,
    origin: Origin,
) -> Result<(Device, bool), String> {
    let current = devices.get(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    if origin == Origin::Client && current.source != device::Source::Virtual {
        /* Hardware only changes when it confirms (control.rs sends the
         * command to it). Reaching this means a caller skipped that. */
        return Err(format!("{id} is hardware: commands go through its driver"));
    }
    let capabilities = device::set_capability(current, capability, value, origin)?;
    if capabilities == current.capabilities {
        return Ok((current.clone(), false));
    }
    let mut new = current.clone();
    new.capabilities = capabilities;
    devices.insert(id.to_string(), new.clone());
    Ok((new, true))
}

fn remove(devices: &mut HashMap<DeviceId, Device>, id: &str) -> Result<(), String> {
    let device = devices.get(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    if device.source.is_builtin() {
        return Err(format!("{id} is built into the hub and can't be removed"));
    }
    devices.remove(id);
    Ok(())
}

/* THE REGISTRY FILE (issue #33) -- the devices, as saved by store.rs.
 *
 * Schema 2 (issue #34): a JSON list of devices (device.rs's format),
 * sorted by id so the file reads the same every time.
 * Schema 1 (issue #33): an object id -> property bag,
 * {"lamp-1": {"on": true}} -- still read, and converted on load
 * (device::migrate_v1). The file itself is rewritten in schema 2 on the
 * next change; until then the old file stays as it is, and after that it
 * is the ".prev" fallback copy. */
pub const REGISTRY_SCHEMA: u32 = 2;

pub fn encode_registry(devices: &HashMap<DeviceId, Device>) -> Vec<u8> {
    let mut sorted: Vec<&Device> = devices.values().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    /* Serializing plain data structures can't fail. */
    serde_json::to_vec_pretty(&sorted).expect("devices serialize")
}

/* Payload bytes -> devices; handed to store.rs's load (see there for what
 * happens on Err). A single device that breaks the rules doesn't throw away
 * all the others: it's skipped, with a log line saying why. */
pub fn decode_registry(schema: u32, payload: &[u8]) -> Result<HashMap<DeviceId, Device>, String> {
    let mut devices = HashMap::new();
    match schema {
        1 => {
            let old: BTreeMap<DeviceId, HashMap<String, serde_json::Value>> =
                serde_json::from_slice(payload).map_err(|e| format!("invalid registry (schema 1): {e}"))?;
            for (id, properties) in old {
                match device::migrate_v1(&id, &properties) {
                    Ok((device, dropped)) => {
                        if dropped.is_empty() {
                            println!("state: {id} converted to the capability model");
                        } else {
                            println!(
                                "state: {id} converted to the capability model, without: {}",
                                dropped.join(", ")
                            );
                        }
                        devices.insert(id, device);
                    }
                    Err(e) => println!("state: could not convert {e}; device left out"),
                }
            }
        }
        2 => {
            let list: Vec<Device> =
                serde_json::from_slice(payload).map_err(|e| format!("invalid registry: {e}"))?;
            for device in list {
                match device.check() {
                    Ok(()) => {
                        devices.insert(device.id.clone(), device);
                    }
                    Err(e) => println!("state: stored device {} is invalid ({e}); left out", device.id),
                }
            }
        }
        other => {
            return Err(format!(
                "registry schema {other} is newer than this daemon understands ({REGISTRY_SCHEMA})"
            ))
        }
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Capabilities, Dimmer, Source, Switch};
    use serde_json::json;

    fn lamp(id: &str) -> Device {
        Device {
            id: id.into(),
            name: format!("Lamp {id}"),
            room: String::new(),
            template: "dimmable-light".into(),
            source: Source::Virtual,
            capabilities: Capabilities {
                switch: Some(Switch { on: false }),
                dimmer: Some(Dimmer { level: 50 }),
                ..Default::default()
            },
        }
    }

    /* The actor with the outputs the tests look at. */
    struct Actor {
        tx: mpsc::Sender<Msg>,
        events: broadcast::Receiver<Event>,
        saves: watch::Receiver<Vec<u8>>,
    }

    fn spawn(devices: Vec<Device>) -> Actor {
        let (tx, rx) = mpsc::channel(8);
        let (changed_tx, _) = watch::channel(());
        let (events_tx, events) = broadcast::channel(16);
        let (save_tx, saves) = watch::channel(Vec::new());
        let devices = devices.into_iter().map(|d| (d.id.clone(), d)).collect();
        tokio::spawn(run(rx, devices, Outputs { changed_tx, events_tx, save_tx }));
        Actor { tx, events, saves }
    }

    /* One request/reply round trip. */
    async fn ask<T>(tx: &mpsc::Sender<Msg>, make: impl FnOnce(oneshot::Sender<T>) -> Msg) -> T {
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(make(reply_tx)).await.unwrap();
        reply_rx.await.unwrap()
    }

    async fn set_cap(
        tx: &mpsc::Sender<Msg>,
        id: &str,
        cap: &str,
        value: serde_json::Value,
        origin: Origin,
    ) -> Result<Device, String> {
        ask(tx, |reply| Msg::SetCapability {
            id: id.into(),
            capability: cap.into(),
            value,
            origin,
            reply,
        })
        .await
    }

    #[tokio::test]
    async fn set_changes_notify_save_and_skip_no_ops() {
        let mut a = spawn(vec![lamp("lamp-1")]);
        let d = set_cap(&a.tx, "lamp-1", "dimmer", json!({"level": 30}), Origin::Client).await.unwrap();
        assert_eq!(d.capabilities.dimmer, Some(Dimmer { level: 30 }));
        assert_eq!(a.events.recv().await.unwrap(), Event::Changed(d.clone()));
        assert!(a.saves.has_changed().unwrap());
        a.saves.mark_unchanged();

        /* The same value again: answered, but no event and no save. */
        set_cap(&a.tx, "lamp-1", "dimmer", json!({"level": 30}), Origin::Client).await.unwrap();
        assert!(a.events.try_recv().is_err());
        assert!(!a.saves.has_changed().unwrap());
    }

    #[tokio::test]
    async fn invalid_commands_are_refused_and_change_nothing() {
        let mut a = spawn(vec![lamp("lamp-1")]);
        let err = set_cap(&a.tx, "lamp-1", "color", json!({"hex": "#FF0000"}), Origin::Client).await.unwrap_err();
        assert_eq!(err, "lamp-1 has no capability \"color\"");
        let err = set_cap(&a.tx, "nope", "switch", json!({"on": true}), Origin::Client).await.unwrap_err();
        assert_eq!(err, "unknown device \"nope\"");
        assert!(a.events.try_recv().is_err());
    }

    #[tokio::test]
    async fn hardware_only_changes_through_its_driver() {
        let mut led = lamp("ld7");
        led.source = Source::M4Led;
        let a = spawn(vec![led]);
        let err = set_cap(&a.tx, "ld7", "switch", json!({"on": true}), Origin::Client).await.unwrap_err();
        assert!(err.contains("hardware"));
        /* The driver reporting what the hardware did is accepted. */
        let d = set_cap(&a.tx, "ld7", "switch", json!({"on": true}), Origin::Device).await.unwrap();
        assert_eq!(d.capabilities.switch, Some(Switch { on: true }));
        /* And built-in hardware can't be removed. */
        let err = ask(&a.tx, |reply| Msg::RemoveDevice { id: "ld7".into(), reply }).await.unwrap_err();
        assert!(err.contains("built into the hub"));
    }

    #[tokio::test]
    async fn add_rename_remove() {
        let mut a = spawn(vec![]);
        ask(&a.tx, |reply| Msg::AddDevice { device: lamp("lamp-1"), reply }).await.unwrap();
        assert!(matches!(a.events.recv().await.unwrap(), Event::Changed(_)));
        /* Same id twice, or an invalid device: refused. */
        assert!(ask(&a.tx, |reply| Msg::AddDevice { device: lamp("lamp-1"), reply }).await.is_err());
        let mut bad = lamp("lamp-2");
        bad.capabilities = Capabilities::default();
        assert!(ask(&a.tx, |reply| Msg::AddDevice { device: bad, reply }).await.is_err());

        let d = ask(&a.tx, |reply| Msg::UpdateInfo {
            id: "lamp-1".into(),
            name: Some("Desk lamp".into()),
            room: Some("Office".into()),
            reply,
        })
        .await
        .unwrap();
        assert_eq!((d.name.as_str(), d.room.as_str()), ("Desk lamp", "Office"));
        assert!(matches!(a.events.recv().await.unwrap(), Event::Changed(_)));

        ask(&a.tx, |reply| Msg::RemoveDevice { id: "lamp-1".into(), reply }).await.unwrap();
        assert_eq!(a.events.recv().await.unwrap(), Event::Removed("lamp-1".into()));
        let all = ask(&a.tx, |reply| Msg::GetAllDevices { reply }).await;
        assert!(all.is_empty());
    }

    #[test]
    fn registry_round_trip_sorted() {
        let devices: HashMap<DeviceId, Device> =
            [lamp("b"), lamp("a")].into_iter().map(|d| (d.id.clone(), d)).collect();
        let bytes = encode_registry(&devices);
        assert_eq!(decode_registry(REGISTRY_SCHEMA, &bytes).unwrap(), devices);
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.find("\"a\"").unwrap() < text.find("\"b\"").unwrap());
        /* Same devices -> same bytes. */
        assert_eq!(encode_registry(&devices), bytes);
    }

    #[test]
    fn schema_1_registry_is_migrated() {
        /* The shape of the DK2's registry before #34, plus a device with
         * nothing convertible. */
        let v1 = br#"{"lamp-1": {"brightness": 80, "on": true}, "tv": {"on": false}, "fan": {"speed": 2}}"#;
        let devices = decode_registry(1, v1).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices["lamp-1"].capabilities.dimmer, Some(Dimmer { level: 80 }));
        assert_eq!(devices["tv"].capabilities.switch, Some(Switch { on: false }));
        assert!(!devices.contains_key("fan"));
    }

    #[test]
    fn registry_rejects_unknown_schema_and_bad_json() {
        assert!(decode_registry(3, b"[]").unwrap_err().contains("newer"));
        assert!(decode_registry(2, b"{}").is_err());
        /* One invalid device is left out, the rest kept. */
        let mut bad = lamp("x");
        bad.name = String::new();
        let bytes = serde_json::to_vec(&vec![lamp("ok"), bad]).unwrap();
        assert_eq!(decode_registry(2, &bytes).unwrap().len(), 1);
    }
}
