/*
 * m4_led.rs -- the adapter for the board's LED LD7, switched by the
 * Cortex-M4 over RPMsg (issues #11/#12, moved onto the adapter interface
 * in #40; its template is templates/m4-led.json).
 *
 * Built in: the "ld7" device is created at start if it doesn't exist (a
 * new board, or a registry from before #34), and can't be removed.
 *
 * Its task keeps the device in step with the real LED:
 *   - `led_rx` is rpmsg.rs's watch channel with the LED's state as the M4
 *     last reported it (None = not heard from the M4 yet). EVERY M4 answer
 *     lands there, whoever asked -- so whatever switches the LED, the task
 *     records it.
 *   - the state saved in the registry may be stale (the LED is off after a
 *     reboot), so until the M4 has answered, it asks every RETRY.
 *   - a command switches the LED and reports what the M4 CONFIRMED
 *     (normally, but not by assumption, what was asked).
 */
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

use super::{Adapter, DeviceCmd, DeviceHandle, Hub};
use crate::device::{Capabilities, Device, Health, Source, Switch};
use crate::rpmsg;

/* Its id is also its AWS shadow's name, unchanged since #26, so the
 * existing shadow simply carries on. */
pub const LED_DEVICE_ID: &str = "ld7";

/* How long to wait for the M4. It normally answers in well under a
 * millisecond; this only matters if it hangs, so nothing waits forever. */
const M4_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/* While the M4 hasn't told us the LED's state yet (it isn't running, or
 * its firmware isn't loaded), ask again this often. */
const RETRY: Duration = Duration::from_secs(2);

pub struct M4Led {
    rpmsg_tx: mpsc::Sender<rpmsg::Cmd>,
    led_rx: watch::Receiver<Option<bool>>,
}

impl M4Led {
    pub fn new(rpmsg_tx: mpsc::Sender<rpmsg::Cmd>, led_rx: watch::Receiver<Option<bool>>) -> Self {
        M4Led { rpmsg_tx, led_rx }
    }
}

impl Adapter for M4Led {
    fn id(&self) -> &'static str {
        "m4-led"
    }

    /* As created the first time (afterwards it can be renamed or moved to
     * another room like any device). Name and room are the template's
     * defaults (a test checks they match). */
    fn builtin_devices(&self) -> Vec<Device> {
        vec![Device {
            id: LED_DEVICE_ID.into(),
            name: "Board LED (LD7)".into(),
            room: "Hub".into(),
            template: "m4-led".into(),
            source: Source::new("m4-led"),
            config: Default::default(),
            identity: String::new(),
            online: None,
            capabilities: Capabilities {
                switch: Some(Switch { on: false }),
                ..Default::default()
            },
        }]
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        tokio::spawn(run(
            device.id.clone(),
            hub,
            self.rpmsg_tx.clone(),
            self.led_rx.clone(),
            commands_rx,
        ));
        DeviceHandle { commands }
    }
}

async fn run(
    id: String,
    hub: Hub,
    rpmsg_tx: mpsc::Sender<rpmsg::Cmd>,
    mut led_rx: watch::Receiver<Option<bool>>,
    mut commands: mpsc::Receiver<DeviceCmd>,
) {
    loop {
        /* borrow_and_update: read AND mark as seen, so changed() below only
         * wakes for a newer value. Copied out at once (the borrow holds a
         * lock on the channel). */
        let led = *led_rx.borrow_and_update();
        if let Some(on) = led {
            if let Err(e) = hub.report(&id, "switch", serde_json::json!({ "on": on })).await {
                println!("m4-led: could not record {id}'s state: {e}");
            }
            /* The M4 answers: the LED is reachable (after its state, like
             * every adapter). Never marked offline: an M4 that stops
             * answering is a firmware bug, not a device someone
             * unplugged -- rpmsg.rs logs that. */
            hub.set_online(&id, Health::Online).await;
        } else {
            /* Not heard from the M4 yet: ask. The answer (if any) arrives
             * through led_rx. */
            let _ = m4(&rpmsg_tx, None).await;
        }
        tokio::select! {
            changed = led_rx.changed() => if changed.is_err() { return },
            cmd = commands.recv() => match cmd {
                Some(cmd) => handle(cmd, &id, &hub, &rpmsg_tx).await,
                None => return, /* the registry is gone: shutting down */
            },
            /* Only while the M4 hasn't answered: ask again. */
            _ = tokio::time::sleep(RETRY), if led.is_none() => {}
        }
    }
}

async fn handle(cmd: DeviceCmd, id: &str, hub: &Hub, rpmsg_tx: &mpsc::Sender<rpmsg::Cmd>) {
    let DeviceCmd::Command { capability, value, reply } = cmd else {
        /* control.rs only sends actions a capability has; the LED's
         * switch has none. */
        return cmd.refuse(format!("{id} has no actions"));
    };
    let result = async {
        /* Its only capability is switch (control.rs checked the command
         * against it), so the value is {"on": true|false}. */
        if capability != "switch" {
            return Err(format!("{id} has no capability {capability:?}"));
        }
        let on = value["on"].as_bool().ok_or("switch needs {\"on\": true|false}")?;
        let confirmed = m4(rpmsg_tx, Some(on)).await?;
        hub.report(id, "switch", serde_json::json!({ "on": confirmed })).await.map(|_| ())
    }
    .await;
    let _ = reply.send(result);
}

/* Switches the LED (Some) or only asks for its state (None); returns the
 * state the M4 reports. */
async fn m4(rpmsg_tx: &mpsc::Sender<rpmsg::Cmd>, on: Option<bool>) -> Result<bool, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = match on {
        Some(on) => rpmsg::Cmd::SetLed { on, reply: reply_tx },
        None => rpmsg::Cmd::GetLedState { reply: reply_tx },
    };
    rpmsg_tx.send(cmd).await.map_err(|_| "M4 link unavailable".to_string())?;
    match tokio::time::timeout(M4_REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("M4 link dropped the reply".into()),
        Err(_) => Err("no answer from the M4".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* The built-in device matches its template (templates/m4-led.json). */
    #[test]
    fn builtin_device_matches_its_template() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/m4-led.json");
        let template: crate::templates::Template =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let (rpmsg_tx, _) = mpsc::channel(1);
        let (_, led_rx) = watch::channel(None);
        let led = &M4Led::new(rpmsg_tx, led_rx).builtin_devices()[0];
        assert!(template.builtin);
        assert_eq!(template.adapter, led.source.as_str());
        assert_eq!(template.id, led.template);
        assert_eq!(template.defaults.name, led.name);
        assert_eq!(template.defaults.room, led.room);
        assert!(crate::device::BUILTIN_SOURCES.contains(&template.adapter.as_str()));
    }
}
