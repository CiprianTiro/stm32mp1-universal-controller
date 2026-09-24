/*
 * control.rs -- the one door every device command goes through (issue #34),
 * whoever sends it: the touchscreen or the app (ws.rs), or the cloud
 * (mqtt.rs).
 *
 * A command ("set lamp-1's dimmer to 30") is:
 *   1. checked against the device's capabilities (device.rs) -- for EVERY
 *      device, so hardware never even receives an invalid command;
 *   2. carried out according to where the device's truth is (its Source):
 *        virtual -> state.rs changes it directly;
 *        m4_led  -> the M4 switches the real LED first, and state.rs then
 *                   records what the M4 CONFIRMED (which is normally, but
 *                   not by assumption, what was asked).
 * The answer is the device as it is afterwards, or the reason it failed.
 * New hardware sources (Zigbee, the ESP32 IR blaster, ...) add a branch in
 * `command` and a driver task like `run_led_driver`.
 *
 * Before #34, the LED ("ld7") was a special case in both ws.rs and mqtt.rs.
 * Now it's an ordinary device in state.rs, with source m4_led; the only
 * LED-specific code left is in this file.
 */
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

use crate::device::{self, Capabilities, Device, Origin, Source, Switch};
use crate::rpmsg;
use crate::state::Msg;

/* The board's LED LD7 as a device: its id is also its AWS shadow's name,
 * unchanged from #26, so the existing shadow simply carries on. */
pub const LED_DEVICE_ID: &str = "ld7";

/* How long to wait for the M4. It normally answers in well under a
 * millisecond; this only matters if it hangs, so nothing waits forever. */
const M4_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/* While the M4 hasn't told us the LED's state yet (it isn't running, or
 * its firmware isn't loaded), ask again this often. */
const LED_RETRY: Duration = Duration::from_secs(2);

/* Cheap to clone (two channel handles): ws.rs gives one to every client. */
#[derive(Clone)]
pub struct Control {
    state_tx: mpsc::Sender<Msg>,
    rpmsg_tx: mpsc::Sender<rpmsg::Cmd>,
}

impl Control {
    pub fn new(state_tx: mpsc::Sender<Msg>, rpmsg_tx: mpsc::Sender<rpmsg::Cmd>) -> Self {
        Control { state_tx, rpmsg_tx }
    }

    /* One request/reply round trip with state.rs. Err only while the
     * daemon is shutting down (the actor is gone). */
    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Msg) -> Result<T, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.state_tx
            .send(make(reply_tx))
            .await
            .map_err(|_| "state actor unavailable".to_string())?;
        reply_rx.await.map_err(|_| "state actor dropped the reply".to_string())
    }

    /* All devices, sorted by id (a stable order for lists on screens). */
    pub async fn list(&self) -> Result<Vec<Device>, String> {
        let all = self.ask(|reply| Msg::GetAllDevices { reply }).await?;
        let mut list: Vec<Device> = all.into_values().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(list)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Device>, String> {
        self.ask(|reply| Msg::GetDevice { id: id.to_string(), reply }).await
    }

    /* Adding is for virtual devices only: hardware appears through its
     * driver (the LED below; Zigbee pairing later), never by a client
     * simply claiming it exists. */
    pub async fn add(&self, device: Device) -> Result<Device, String> {
        if device.source != Source::Virtual {
            return Err(format!("{:?} devices are added by their driver, not by hand", device.source));
        }
        self.ask(|reply| Msg::AddDevice { device, reply }).await?
    }

    pub async fn update_info(&self, id: &str, name: Option<String>, room: Option<String>) -> Result<Device, String> {
        self.ask(|reply| Msg::UpdateInfo {
            id: id.to_string(),
            name,
            room,
            reply,
        })
        .await?
    }

    pub async fn remove(&self, id: &str) -> Result<(), String> {
        self.ask(|reply| Msg::RemoveDevice { id: id.to_string(), reply }).await?
    }

    /* The command itself -- see the header comment. */
    pub async fn command(&self, id: &str, capability: &str, value: serde_json::Value) -> Result<Device, String> {
        let device = self.get(id).await?.ok_or_else(|| format!("unknown device {id:?}"))?;
        /* 1. The rules, for every device (the result is thrown away here:
         *    state.rs applies the change itself, below). */
        device::set_capability(&device, capability, value.clone(), Origin::Client)?;
        /* 2. Carry it out. */
        match device.source {
            Source::Virtual => self.set(id, capability, value, Origin::Client).await,
            Source::M4Led => {
                /* Its only capability is switch (checked in step 1), so the
                 * value is {"on": true|false}. */
                let on = value["on"].as_bool().ok_or("switch needs {\"on\": true|false}")?;
                let confirmed = self.m4_led(Some(on)).await?;
                self.set(id, "switch", serde_json::json!({ "on": confirmed }), Origin::Device)
                    .await
            }
        }
    }

    async fn set(&self, id: &str, capability: &str, value: serde_json::Value, origin: Origin) -> Result<Device, String> {
        self.ask(|reply| Msg::SetCapability {
            id: id.to_string(),
            capability: capability.to_string(),
            value,
            origin,
            reply,
        })
        .await?
    }

    /* Switches the LED (Some) or only asks for its state (None); returns
     * the state the M4 reports. */
    async fn m4_led(&self, on: Option<bool>) -> Result<bool, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = match on {
            Some(on) => rpmsg::Cmd::SetLed { on, reply: reply_tx },
            None => rpmsg::Cmd::GetLedState { reply: reply_tx },
        };
        self.rpmsg_tx.send(cmd).await.map_err(|_| "M4 link unavailable".to_string())?;
        match tokio::time::timeout(M4_REPLY_TIMEOUT, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("M4 link dropped the reply".into()),
            Err(_) => Err("no answer from the M4".into()),
        }
    }
}

/* The LED's device entry, as created the first time (afterwards it can be
 * renamed or moved to another room like any device). */
fn led_device() -> Device {
    Device {
        id: LED_DEVICE_ID.into(),
        name: "Board LED (LD7)".into(),
        room: "Hub".into(),
        template: "builtin-led".into(),
        source: Source::M4Led,
        capabilities: Capabilities {
            switch: Some(Switch { on: false }),
            ..Default::default()
        },
    }
}

/* The LED's driver: keeps the "ld7" device in step with the real LED.
 *
 * `led_rx` is rpmsg.rs's watch channel with the LED's state as the M4 last
 * reported it (None = not heard from the M4 yet). Every M4 answer lands
 * there, whoever asked -- so whatever switches the LED, this task records
 * it in state.rs, which tells the screens and the cloud.
 *
 * At start the device is created if it doesn't exist yet (a new board, or
 * a registry from before #34). The state saved in the registry may be
 * stale (the LED is off after a reboot), so until the M4 has answered, it
 * asks every LED_RETRY. */
pub async fn run_led_driver(control: Control, mut led_rx: watch::Receiver<Option<bool>>) {
    match control.get(LED_DEVICE_ID).await {
        Ok(Some(_)) => {}
        Ok(None) => match control.ask(|reply| Msg::AddDevice { device: led_device(), reply }).await {
            Ok(Ok(_)) => println!("control: {LED_DEVICE_ID} added as a device"),
            Ok(Err(e)) | Err(e) => println!("control: could not add {LED_DEVICE_ID}: {e}"),
        },
        Err(_) => return, /* shutting down */
    }

    loop {
        /* borrow_and_update: read AND mark as seen, so changed() below only
         * wakes for a newer value. Copied out at once (the borrow holds a
         * lock on the channel). */
        let led = *led_rx.borrow_and_update();
        match led {
            Some(on) => {
                if let Err(e) = control
                    .set(LED_DEVICE_ID, "switch", serde_json::json!({ "on": on }), Origin::Device)
                    .await
                {
                    println!("control: could not record {LED_DEVICE_ID}'s state: {e}");
                }
                if led_rx.changed().await.is_err() {
                    return;
                }
            }
            None => {
                /* The answer (if any) arrives through led_rx. */
                let _ = control.m4_led(None).await;
                tokio::select! {
                    changed = led_rx.changed() => if changed.is_err() { return },
                    _ = tokio::time::sleep(LED_RETRY) => {}
                }
            }
        }
    }
}
