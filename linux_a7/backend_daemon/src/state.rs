/* `use` pulls in types defined elsewhere so we can refer to them by their
 * short name instead of the full path every time.
 * - HashMap: Rust's dictionary/map type (key -> value lookups).
 * - mpsc: "multi-producer, single-consumer" channel -- this is the mailbox
 *   from the clerk/mailbox analogy. Many tasks can hold a Sender (mpsc =
 *   many producers), but only ONE task ever owns the Receiver (single
 *   consumer) -- that receiver is what makes this task the sole owner of
 *   whatever it's guarding.
 * - oneshot: a channel built for exactly one message, then it's done. This
 *   is the "return envelope" -- used only for sending a single reply back
 *   to whoever asked a question. */
use std::collections::{BTreeMap, HashMap};
use tokio::sync::{mpsc, oneshot, watch};

/* `type X = Y;` just gives an existing type a new, more meaningful name --
 * it does NOT create a new type. DeviceId is really just a String
 * underneath (e.g. "living-room-lamp"), and DeviceState is really just a
 * HashMap underneath. Naming them like this makes the code below read as
 * "a DeviceId maps to a DeviceState" instead of "a String maps to a
 * HashMap<String, Value>", which says nothing about what it's actually for. */
pub type DeviceId = String;

/* serde_json::Value is a type that can hold ANY JSON-shaped value: true/
 * false, a number, a string, etc. So DeviceState is a flexible bag of
 * named properties -- {"on": true, "brightness": 80} -- where we never
 * hardcode which properties a device has. This is what makes the design
 * work for any device category (lamp, camera, sensor, ...) without state.rs
 * needing to know anything about any specific one. */
pub type DeviceState = HashMap<String, serde_json::Value>;

/* This enum is the complete list of "notes" anyone is allowed to drop in
 * the mailbox -- every possible request this actor understands, and
 * nothing else. `enum` means a value of type Msg is always EXACTLY ONE of
 * these four shapes at a time, never more than one, never a mix. */
pub enum Msg {
    /* "Set/merge these properties onto this device (creating it if it
     * doesn't exist yet)." No reply needed -- this is fire-and-forget. */
    UpdateDevice {
        id: DeviceId,
        properties: DeviceState,
    },
    /* "Tell me this one device's current properties." `reply` is the return
     * envelope: whoever sends this message creates a fresh oneshot channel,
     * keeps one half for themselves, and hands the other half (the
     * Sender<...>) to us inside the message -- so we know exactly where to
     * write the answer back to. */
    GetDevice {
        id: DeviceId,
        reply: oneshot::Sender<Option<DeviceState>>,
    },
    /* Same idea as GetDevice, but "give me every device" instead of just
     * one. */
    GetAllDevices {
        reply: oneshot::Sender<HashMap<DeviceId, DeviceState>>,
    },
    /* "Forget this device entirely." No reply needed. */
    RemoveDevice {
        id: DeviceId,
    },
}

// The sole owner of hub state -- every other task talks to it only through
// Msg, never a shared reference, so no lock is ever held across an .await.
// See ARCHITECTURE.md's "State ownership" section for why.
/*
 * THIS FUNCTION IS THE ACTOR ITSELF.
 *
 * Calling `run(...)` does NOT start it running. Like any async fn,
 * calling it just builds an inert Future -- nothing executes until
 * something spawns it onto the Tokio runtime. In the real daemon that
 * happens in exactly one place, main.rs (`tokio::spawn(state::run(...))`);
 * in the tests, in `spawn_actor()` below.
 *
 * `mut rx: mpsc::Receiver<Msg>` -- this function takes ownership of the
 * RECEIVING half of a mailbox (created elsewhere, handed in as a
 * parameter). `mut` because pulling messages out of it below changes its
 * internal read position each time.
 *
 * `changed_tx` (issue #29): the sending half of a `watch` channel that
 * carries no data at all -- `()` -- only the fact that something changed.
 * Every UpdateDevice/RemoveDevice "rings the bell", and mqtt.rs, which
 * holds the receiving half, wakes up and reports the new state to the cloud
 * right away instead of at its next periodic tick. (Same idea as rpmsg.rs's
 * LED watch channel, just without a value, because mqtt.rs asks for the
 * full device list anyway.)
 */
/*
 * `devices` (issue #33): the registry as loaded from disk at start (see
 * load_registry), so devices survive restarts. `save_tx`: store.rs's
 * writer for the registry file -- after every real change the whole
 * registry is encoded and handed over; the writer puts it on the flash in
 * the background, so this loop never waits for a slow fsync.
 */
pub async fn run(
    mut rx: mpsc::Receiver<Msg>,
    changed_tx: watch::Sender<()>,
    mut devices: HashMap<DeviceId, DeviceState>,
    save_tx: watch::Sender<Vec<u8>>,
) {
    /* THE FILING CABINET is `devices` above. It's the one and only copy of
     * hub state that exists anywhere -- owned by this function, so no
     * other task can ever reach in and touch it directly; the only way
     * anyone else affects it is by sending a Msg through the channel above,
     * which this same function reads and acts on, below. It starts out as
     * whatever the registry file held (empty on the very first start). */

    /* THE MAIN LOOP. `rx.recv().await` pulls the next message out of the
     * mailbox -- if none is waiting yet, this suspends right here
     * (cooperatively, same as tick.tick().await elsewhere in this project)
     * until one arrives, without blocking the OS thread.
     * `while let Some(msg) = ...` means: keep looping for as long as
     * `.recv()` keeps handing back an actual message (`Some(msg)`). It only
     * ever returns `None` -- ending the loop -- once every single Sender
     * for this channel has been dropped, i.e. it's now impossible for
     * anyone to ever send another message. That's this actor's natural
     * shutdown condition. */
    while let Some(msg) = rx.recv().await {
        /* `match` looks at which of the four Msg variants this particular
         * message actually is, and runs the matching arm below. Rust
         * forces every variant to be handled -- there's no "default" case
         * silently swallowing one you forgot. */
        match msg {
            Msg::UpdateDevice { id, properties } => {
                /* .entry(id) looks up this device in the filing cabinet.
                 * .or_default() means: if it's not there yet, insert a
                 * fresh empty property bag for it first -- so an update to
                 * an unknown device id silently creates it, no separate
                 * "register a new device" step needed. .extend(properties)
                 * then merges the new key/value pairs in: new keys get
                 * added, existing keys get overwritten with the new value,
                 * anything not mentioned is left alone. */
                let is_new = !devices.contains_key(&id);
                let entry = devices.entry(id).or_default();
                /* Remember the old properties, so we only write to the
                 * flash when something really changed -- a command that
                 * sets a lamp that's already on to "on" costs no write. */
                let before = entry.clone();
                entry.extend(properties);
                if is_new || *entry != before {
                    save_tx.send_replace(encode_registry(&devices));
                }
                /* send_replace, not send: send fails when nobody is
                 * listening (e.g. cloud sync disabled), send_replace just
                 * stores the value regardless -- nothing to handle. */
                changed_tx.send_replace(());
            }
            Msg::GetDevice { id, reply } => {
                /* devices.get(&id) looks the device up and returns an
                 * Option: Some(data) if it exists, None if it doesn't --
                 * exactly matching the "asking about an unknown device
                 * gives back nothing, not an error" behavior. .cloned()
                 * makes an independent copy of the data: we can't hand out
                 * a direct reference into our own private `devices` map,
                 * since the answer has to travel out of this function,
                 * across the channel, to a completely different task.
                 * reply.send(...) writes that copy onto the return
                 * envelope. `let _ =` in front discards the result of
                 * send(): it can fail if whoever asked already gave up and
                 * dropped their end of the oneshot channel, and we're
                 * explicitly saying "that's fine, nothing to do about it
                 * here" instead of letting the compiler warn about an
                 * unused Result. */
                let _ = reply.send(devices.get(&id).cloned());
            }
            Msg::GetAllDevices { reply } => {
                /* Same idea as GetDevice, but .clone() copies the entire
                 * filing cabinet (every device) instead of looking up just
                 * one. */
                let _ = reply.send(devices.clone());
            }
            Msg::RemoveDevice { id } => {
                /* Deletes this device's entry from the filing cabinet
                 * entirely, if it exists (does nothing if it didn't). */
                if devices.remove(&id).is_some() {
                    save_tx.send_replace(encode_registry(&devices));
                    changed_tx.send_replace(());
                }
            }
        }
    }
}

/* THE REGISTRY FILE (issue #33) -- the devices, as saved by store.rs.
 *
 * The layout of the payload, schema 1: one JSON object, device id ->
 * that device's properties, e.g. {"lamp-1": {"on": true}}. When the layout
 * changes (the capability model), REGISTRY_SCHEMA goes up and
 * decode_registry gets a branch that converts schema-1 files. */
pub const REGISTRY_SCHEMA: u32 = 1;

/* Registry -> payload bytes. Written through a BTreeMap (a map that keeps
 * its keys sorted) so the file lists devices alphabetically every time --
 * easier to read on the board, and saving the same devices twice gives
 * byte-identical files. Pretty-printed: it's small, and humans read it. */
pub fn encode_registry(devices: &HashMap<DeviceId, DeviceState>) -> Vec<u8> {
    let sorted: BTreeMap<&DeviceId, BTreeMap<&String, &serde_json::Value>> = devices
        .iter()
        .map(|(id, properties)| (id, properties.iter().collect()))
        .collect();
    /* Serializing maps of strings and JSON values can't fail. */
    serde_json::to_vec_pretty(&sorted).expect("device map serializes")
}

/* Payload bytes -> registry; handed to store.rs's load (see there for
 * what happens on Err). */
pub fn decode_registry(schema: u32, payload: &[u8]) -> Result<HashMap<DeviceId, DeviceState>, String> {
    match schema {
        1 => serde_json::from_slice(payload).map_err(|e| format!("invalid registry: {e}")),
        other => Err(format!("registry schema {other} is newer than this daemon understands ({REGISTRY_SCHEMA})")),
    }
}

/* Everything below only exists when running `cargo test` -- #[cfg(test)]
 * tells the compiler to leave this whole module out of a normal build
 * entirely (it's not part of the real backend-daemon binary at all). */
#[cfg(test)]
mod tests {
    /* Brings in everything from the parent module (state.rs itself) --
     * Msg, DeviceId, DeviceState, run, etc -- so the tests below can use
     * them without repeating `super::` in front of each one.
     * `serde_json::json!` is a convenience macro for writing JSON values
     * inline, e.g. json!(true) or json!(80), instead of building
     * serde_json::Value by hand. */
    use super::*;
    use serde_json::json;

    /* Small test helper: creates a brand-new mailbox (mpsc::channel(8) --
     * the 8 is how many messages can be queued up before a sender has to
     * wait its turn), and -- THIS is the one real place `run` gets spawned
     * anywhere in this crate right now -- hands the receiving half to a
     * freshly spawned task actually running the actor loop above. Returns
     * the sending half so each test can send it messages. */
    fn spawn_actor() -> mpsc::Sender<Msg> {
        let (tx, rx) = mpsc::channel(8);
        /* The tests don't care about change notifications; the receiving
         * half is simply dropped. */
        let (changed_tx, _) = watch::channel(());
        let (save_tx, _) = watch::channel(Vec::new());
        tokio::spawn(run(rx, changed_tx, HashMap::new(), save_tx));
        tx
    }

    /* Sends one message, then a GetAllDevices round trip: state.rs handles
     * messages strictly in order, so once that is answered, the message
     * before it has been fully handled (including any save). */
    async fn send_and_settle(tx: &mpsc::Sender<Msg>, msg: Msg) {
        tx.send(msg).await.unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Msg::GetAllDevices { reply: reply_tx }).await.unwrap();
        reply_rx.await.unwrap();
    }

    fn update(id: &str, key: &str, value: serde_json::Value) -> Msg {
        Msg::UpdateDevice {
            id: id.into(),
            properties: HashMap::from([(key.into(), value)]),
        }
    }

    #[test]
    fn registry_round_trip_and_sorted_output() {
        let devices = HashMap::from([
            ("lamp-2".to_string(), HashMap::from([("on".to_string(), json!(false))])),
            (
                "lamp-1".to_string(),
                HashMap::from([("on".to_string(), json!(true)), ("brightness".to_string(), json!(80))]),
            ),
        ]);
        let bytes = encode_registry(&devices);
        assert_eq!(decode_registry(REGISTRY_SCHEMA, &bytes), Ok(devices.clone()));
        /* Same devices -> same bytes, and lamp-1 comes first. */
        assert_eq!(encode_registry(&devices), bytes);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.find("lamp-1").unwrap() < text.find("lamp-2").unwrap());
    }

    #[test]
    fn registry_rejects_unknown_schema_and_bad_json() {
        assert!(decode_registry(2, b"{}").unwrap_err().contains("newer"));
        assert!(decode_registry(1, b"[1, 2]").is_err());
    }

    /* Devices loaded at start are there, and every real change hands the
     * writer a fresh copy of the registry -- but a no-op update doesn't. */
    #[tokio::test]
    async fn changes_are_saved_and_loaded_devices_kept() {
        let (tx, rx) = mpsc::channel(8);
        let (changed_tx, _) = watch::channel(());
        let (save_tx, mut save_rx) = watch::channel(Vec::new());
        let loaded = HashMap::from([("lamp-1".to_string(), HashMap::from([("on".to_string(), json!(true))]))]);
        tokio::spawn(run(rx, changed_tx, loaded, save_tx));

        /* Setting "on" to the value it already has: nothing to save. */
        send_and_settle(&tx, update("lamp-1", "on", json!(true))).await;
        assert!(!save_rx.has_changed().unwrap());

        send_and_settle(&tx, update("lamp-2", "on", json!(false))).await;
        assert!(save_rx.has_changed().unwrap());
        let saved = decode_registry(REGISTRY_SCHEMA, &save_rx.borrow_and_update()).unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved["lamp-1"]["on"], json!(true));

        send_and_settle(&tx, Msg::RemoveDevice { id: "lamp-1".into() }).await;
        let saved = decode_registry(REGISTRY_SCHEMA, &save_rx.borrow_and_update()).unwrap();
        assert_eq!(saved.keys().collect::<Vec<_>>(), vec!["lamp-2"]);
    }

    /* #[tokio::test] is like #[tokio::main] but for a single test function
     * -- it spins up a Tokio runtime just for this test so `.await` is
     * usable inside it. */
    #[tokio::test]
    async fn update_then_get_returns_merged_properties() {
        let tx = spawn_actor();

        /* Send two separate UpdateDevice messages for the same device id,
         * each with a different property -- proving properties merge
         * together rather than each update wiping out the last one.
         * .await here waits for the send itself to complete (the mailbox
         * having room); .unwrap() panics the test if sending somehow
         * failed (e.g. the actor task had already died). */
        tx.send(Msg::UpdateDevice {
            id: "lamp-1".into(),
            properties: HashMap::from([("on".into(), json!(true))]),
        })
        .await
        .unwrap();
        tx.send(Msg::UpdateDevice {
            id: "lamp-1".into(),
            properties: HashMap::from([("brightness".into(), json!(80))]),
        })
        .await
        .unwrap();

        /* To ask a question, first create a oneshot channel by hand here in
         * the test (this is the "return envelope with your name on it"),
         * keep reply_rx for ourselves, and hand reply_tx to the actor
         * inside the message. */
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Msg::GetDevice {
            id: "lamp-1".into(),
            reply: reply_tx,
        })
        .await
        .unwrap();

        /* reply_rx.await suspends until the actor actually writes an
         * answer into our envelope. .unwrap() unwraps the oneshot channel
         * itself succeeding; .expect(...) then unwraps the Option inside
         * it, panicking with that message if the device somehow wasn't
         * found. assert_eq! checks both properties from the two separate
         * updates above both ended up present together. */
        let state = reply_rx.await.unwrap().expect("device should exist");
        assert_eq!(state.get("on"), Some(&json!(true)));
        assert_eq!(state.get("brightness"), Some(&json!(80)));
    }

    /* Same request/reply pattern as above, but asking about a device id
     * that was never created -- checking we get back None (not a crash,
     * not an error) for an unknown device. */
    #[tokio::test]
    async fn get_unknown_device_returns_none() {
        let tx = spawn_actor();

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Msg::GetDevice {
            id: "does-not-exist".into(),
            reply: reply_tx,
        })
        .await
        .unwrap();

        assert_eq!(reply_rx.await.unwrap(), None);
    }

    /* Creates a device, removes it, then asks for it again -- checking
     * RemoveDevice actually makes it disappear rather than just marking it
     * somehow. */
    #[tokio::test]
    async fn remove_device_clears_it() {
        let tx = spawn_actor();

        tx.send(Msg::UpdateDevice {
            id: "lamp-1".into(),
            properties: HashMap::from([("on".into(), json!(true))]),
        })
        .await
        .unwrap();
        tx.send(Msg::RemoveDevice {
            id: "lamp-1".into(),
        })
        .await
        .unwrap();

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Msg::GetDevice {
            id: "lamp-1".into(),
            reply: reply_tx,
        })
        .await
        .unwrap();

        assert_eq!(reply_rx.await.unwrap(), None);
    }

    /* Registers two different devices, then uses GetAllDevices (instead of
     * GetDevice) to check both come back together in one reply. */
    #[tokio::test]
    async fn get_all_devices_returns_every_registered_device() {
        let tx = spawn_actor();

        for id in ["lamp-1", "lamp-2"] {
            tx.send(Msg::UpdateDevice {
                id: id.into(),
                properties: HashMap::from([("on".into(), json!(true))]),
            })
            .await
            .unwrap();
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Msg::GetAllDevices { reply: reply_tx }).await.unwrap();

        let all = reply_rx.await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("lamp-1"));
        assert!(all.contains_key("lamp-2"));
    }
}
