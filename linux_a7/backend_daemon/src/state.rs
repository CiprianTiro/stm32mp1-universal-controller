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

use crate::device::{self, Device, Health, Origin};

/* A device's id, e.g. "lamp-1" (also its AWS shadow's name). A type alias:
 * just a String with a name that says what it's for. */
pub type DeviceId = String;

/* What happened to a device, for event subscribers (ws.rs). */
#[derive(Debug, Clone, PartialEq)]
// Changed carries a whole Device, Removed only an id: clippy calls the
// size difference wasteful, but events are rare (a device changing), so
// boxing would only make the code noisier.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /* Added, or changed in any way: the device as it is now. */
    Changed(Device),
    Removed(DeviceId),
}

/* Every request the actor understands. Each carries its reply envelope;
 * a sender that doesn't care about the answer can drop the receiving half
 * (the actor ignores a failed reply). */
// AddDevice carries a whole Device: the same trade-off as Event above.
#[allow(clippy::large_enum_variant)]
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
    /* Rename it, move it to another room, star it (issue #95) (None =
     * leave as is). */
    UpdateInfo {
        id: DeviceId,
        name: Option<String>,
        room: Option<String>,
        favourite: Option<bool>,
        reply: oneshot::Sender<Result<Device, String>>,
    },
    /* Change some of an adapter's settings (issue #40), e.g. the new
     * address of a device discovery found elsewhere. Only the given keys
     * change. */
    SetConfig {
        id: DeviceId,
        config: BTreeMap<String, String>,
        reply: oneshot::Sender<Result<Device, String>>,
    },
    /* Issue #99: forget some of them (a plug link that's undone). Keys
     * the device doesn't have are fine. */
    RemoveConfig {
        id: DeviceId,
        keys: Vec<String>,
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
    /* Adds these capabilities (with their neutral starting values) where
     * the device lacks them; replies with the ones added. See
     * Control::add_missing_capabilities. */
    AddMissingCapabilities {
        id: DeviceId,
        names: Vec<String>,
        reply: oneshot::Sender<Result<Vec<String>, String>>,
    },
    /* Issue #85: removes these capabilities (an IR device that stops
     * being used as a light loses its switch and colour); replies with
     * the ones removed. A device keeps at least one. */
    RemoveCapabilities {
        id: DeviceId,
        names: Vec<String>,
        reply: oneshot::Sender<Result<Vec<String>, String>>,
    },
    /* A hardware device became reachable or not (issue #40), reported by
     * its adapter. Screens and the cloud hear about it; the flash doesn't
     * (it only describes this run of the hub, see Device::online). */
    SetOnline {
        id: DeviceId,
        online: Health,
        reply: oneshot::Sender<Result<Device, String>>,
    },
    /* Issue #72: a battery device was heard from (Device::last_seen). */
    Seen {
        id: DeviceId,
        at: u64,
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
                    changed(&out, &devices, Event::Changed(device.clone()), true);
                }
                let _ = reply.send(result);
            }
            Msg::UpdateInfo { id, name, room, favourite, reply } => {
                let result = update_info(&mut devices, &id, name, room, favourite);
                if let Ok((device, true)) = &result {
                    changed(&out, &devices, Event::Changed(device.clone()), true);
                }
                let _ = reply.send(result.map(|(device, _)| device));
            }
            Msg::SetConfig { id, config, reply } => {
                let result = set_config(&mut devices, &id, config);
                if let Ok((device, true)) = &result {
                    changed(&out, &devices, Event::Changed(device.clone()), true);
                }
                let _ = reply.send(result.map(|(device, _)| device));
            }
            Msg::RemoveConfig { id, keys, reply } => {
                let result = remove_config(&mut devices, &id, &keys);
                if let Ok((device, true)) = &result {
                    changed(&out, &devices, Event::Changed(device.clone()), true);
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
                    changed(&out, &devices, Event::Changed(device.clone()), true);
                }
                let _ = reply.send(result.map(|(device, _)| device));
            }
            Msg::AddMissingCapabilities { id, names, reply } => {
                let result = add_missing(&mut devices, &id, &names);
                if let Ok(added) = &result {
                    if !added.is_empty() {
                        let device = devices[&id].clone();
                        changed(&out, &devices, Event::Changed(device), true);
                    }
                }
                let _ = reply.send(result);
            }
            Msg::RemoveCapabilities { id, names, reply } => {
                let result = remove_capabilities(&mut devices, &id, &names);
                if let Ok(removed) = &result {
                    if !removed.is_empty() {
                        let device = devices[&id].clone();
                        changed(&out, &devices, Event::Changed(device), true);
                    }
                }
                let _ = reply.send(result);
            }
            Msg::Seen { id, at } => {
                if let Some(device) = devices.get_mut(&id) {
                    device.last_seen = Some(at);
                    let device = device.clone();
                    /* Announced (screens show "seen just now"), not saved. */
                    changed(&out, &devices, Event::Changed(device), false);
                }
            }
            Msg::SetOnline { id, online, reply } => {
                let result = set_online(&mut devices, &id, online);
                if let Ok((device, true)) = &result {
                    changed(&out, &devices, Event::Changed(device.clone()), false);
                }
                let _ = reply.send(result.map(|(device, _)| device));
            }
            Msg::RemoveDevice { id, reply } => {
                let result = remove(&mut devices, &id);
                if result.is_ok() {
                    changed(&out, &devices, Event::Removed(id), true);
                }
                let _ = reply.send(result);
            }
        }
    }
}

/* Tells everyone about a change: the registry writer (the whole registry,
 * encoded -- only if `persist`: nothing it saves changed otherwise), the
 * cloud's bell, and the event subscribers. `send` on a broadcast channel
 * fails only when nobody is subscribed -- fine. */
fn changed(out: &Outputs, devices: &HashMap<DeviceId, Device>, event: Event, persist: bool) {
    if persist {
        out.save_tx.send_replace(encode_registry(devices));
    }
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
    favourite: Option<bool>,
) -> Result<(Device, bool), String> {
    let current = devices.get(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    let mut new = current.clone();
    if let Some(name) = name {
        new.name = name;
    }
    if let Some(room) = room {
        new.room = room;
    }
    if let Some(favourite) = favourite {
        new.favourite = favourite;
    }
    if new == *current {
        return Ok((new, false));
    }
    new.check()?;
    devices.insert(id.to_string(), new.clone());
    Ok((new, true))
}

fn set_config(
    devices: &mut HashMap<DeviceId, Device>,
    id: &str,
    config: BTreeMap<String, String>,
) -> Result<(Device, bool), String> {
    let current = devices.get(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    let mut new = current.clone();
    new.config.extend(config);
    if new == *current {
        return Ok((new, false));
    }
    new.check()?;
    devices.insert(id.to_string(), new.clone());
    Ok((new, true))
}

fn remove_config(devices: &mut HashMap<DeviceId, Device>, id: &str, keys: &[String]) -> Result<(Device, bool), String> {
    let device = devices.get_mut(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    let before = device.config.len();
    device.config.retain(|key, _| !keys.contains(key));
    Ok((device.clone(), device.config.len() != before))
}

fn set(
    devices: &mut HashMap<DeviceId, Device>,
    id: &str,
    capability: &str,
    value: serde_json::Value,
    origin: Origin,
) -> Result<(Device, bool), String> {
    let current = devices.get(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    if origin == Origin::Client && !current.source.is_virtual() {
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

fn add_missing(devices: &mut HashMap<DeviceId, Device>, id: &str, names: &[String]) -> Result<Vec<String>, String> {
    let device = devices.get(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    let mut capabilities = device.capabilities.clone();
    let mut added = Vec::new();
    for name in names {
        if capabilities.add_default(name)? {
            added.push(name.clone());
        }
    }
    if !added.is_empty() {
        capabilities.check()?;
        devices.get_mut(id).expect("checked above").capabilities = capabilities;
    }
    Ok(added)
}

fn remove_capabilities(devices: &mut HashMap<DeviceId, Device>, id: &str, names: &[String]) -> Result<Vec<String>, String> {
    let device = devices.get(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    let mut capabilities = device.capabilities.clone();
    let mut removed = Vec::new();
    for name in names {
        if capabilities.remove(name)? {
            removed.push(name.clone());
        }
    }
    if !removed.is_empty() {
        capabilities.check()?;
        devices.get_mut(id).expect("checked above").capabilities = capabilities;
    }
    Ok(removed)
}

fn set_online(devices: &mut HashMap<DeviceId, Device>, id: &str, online: Health) -> Result<(Device, bool), String> {
    let device = devices.get_mut(id).ok_or_else(|| format!("unknown device {id:?}"))?;
    if device.source.is_virtual() {
        return Err(format!("{id} is virtual: it has no connection to be online or not"));
    }
    let changed = device.online != Some(online);
    device.online = Some(online);
    Ok((device.clone(), changed))
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
    /* Without `online`: it only describes this run (Device::online). */
    let mut sorted: Vec<Device> = devices
        .values()
        .map(|d| Device {
            online: None,
            last_seen: None,
            favourite: false,
            ..d.clone()
        })
        .collect();
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
            for mut device in list {
                /* The LED's template was called "builtin-led" before #40
                 * gave it a real template (templates/m4-led.json). */
                if device.template == "builtin-led" {
                    device.template = "m4-led".into();
                }
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
            source: Source::default(),
            config: Default::default(),
            identity: String::new(),
            online: None,
            last_seen: None,
            favourite: false,
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
        led.source = Source::new("m4-led");
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

    /* Reachability (issue #40): announced, but never written to the flash
     * -- and not saved with the next change either. Virtual devices have
     * none. */
    #[tokio::test]
    async fn online_is_announced_but_never_saved() {
        let mut strip = lamp("strip");
        strip.source = Source::new("wled");
        let mut a = spawn(vec![strip, lamp("lamp-1")]);
        let set_online = |id: &str, online| {
            let id = id.to_string();
            move |reply| Msg::SetOnline { id, online, reply }
        };
        let d = ask(&a.tx, set_online("strip", Health::Online)).await.unwrap();
        assert_eq!(d.online, Some(Health::Online));
        assert_eq!(a.events.recv().await.unwrap(), Event::Changed(d));
        assert!(!a.saves.has_changed().unwrap());
        assert!(ask(&a.tx, set_online("lamp-1", Health::Online)).await.is_err());

        /* A change that IS saved: the saved registry has no "online". */
        set_cap(&a.tx, "lamp-1", "dimmer", json!({"level": 10}), Origin::Client).await.unwrap();
        let saved = a.saves.borrow_and_update().clone();
        assert!(!String::from_utf8(saved).unwrap().contains("online"));
    }

    /* Issue #44: a device gains the capabilities its template got since;
     * existing ones are left as they are. */
    #[tokio::test]
    async fn missing_capabilities_are_added_once() {
        let mut a = spawn(vec![lamp("lamp-1")]);
        let names = vec!["switch".to_string(), "dimmer".to_string(), "color".to_string()];
        let added = ask(&a.tx, |reply| Msg::AddMissingCapabilities { id: "lamp-1".into(), names: names.clone(), reply })
            .await
            .unwrap();
        assert_eq!(added, ["color"]);
        let Event::Changed(d) = a.events.recv().await.unwrap() else { panic!() };
        assert_eq!(d.capabilities.dimmer, Some(Dimmer { level: 50 }), "kept, not reset");
        assert!(d.capabilities.color.is_some());
        let again = ask(&a.tx, |reply| Msg::AddMissingCapabilities { id: "lamp-1".into(), names, reply }).await.unwrap();
        assert!(again.is_empty());
        assert!(a.events.try_recv().is_err());
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
            favourite: Some(true),
            reply,
        })
        .await
        .unwrap();
        assert_eq!((d.name.as_str(), d.room.as_str(), d.favourite), ("Desk lamp", "Office", true));
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
