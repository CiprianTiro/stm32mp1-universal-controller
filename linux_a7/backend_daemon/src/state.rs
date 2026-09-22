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
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

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
 * IMPORTANT, since this was the actual question: calling `run(...)` here in
 * this file does NOT start it running. Like any async fn, calling it just
 * builds an inert Future -- nothing executes until something spawns it onto
 * the Tokio runtime. Search this whole crate right now and `run` is only
 * ever actually spawned in ONE place: `spawn_actor()` in the test module
 * below (`tokio::spawn(run(rx))`), which only runs when you execute
 * `cargo test` -- never when you run the real compiled daemon. `main.rs`
 * currently only has `mod state;`, which just tells the compiler "compile
 * this file as part of the crate" -- it does not call or spawn anything in
 * it. That wiring (main.rs actually spawning this actor for real) is
 * future work, for when ws.rs exists and needs something to talk to.
 *
 * `mut rx: mpsc::Receiver<Msg>` -- this function takes ownership of the
 * RECEIVING half of a mailbox (created elsewhere, handed in as a
 * parameter). `mut` because pulling messages out of it below changes its
 * internal read position each time.
 */
pub async fn run(mut rx: mpsc::Receiver<Msg>) {
    /* THE FILING CABINET. This is the one and only copy of hub state that
     * exists anywhere -- a local variable, private to this function. No
     * other task can ever reach in and touch this directly; the only way
     * anyone else affects it is by sending a Msg through the channel above,
     * which this same function reads and acts on, below. Starts empty. */
    let mut devices: HashMap<DeviceId, DeviceState> = HashMap::new();

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
                devices.entry(id).or_default().extend(properties);
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
                devices.remove(&id);
            }
        }
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
        tokio::spawn(run(rx));
        tx
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
