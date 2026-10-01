/*
 * adapters -- the code that speaks each device family's protocol (issue
 * #40). A TEMPLATE (templates.rs) says what a device type is; its ADAPTER
 * makes it work. One adapter per protocol family: "m4-led" (the board's
 * LED, through the Cortex-M4), later "wled", "lg-webos", ...
 *
 * AT RUN TIME every hardware device has its own task, started by its
 * adapter (Adapter::start). The task keeps whatever connection the device
 * needs, REPORTS every state change into state.rs (through `Hub`), and
 * carries out commands arriving on its channel (DeviceCmd). One device's
 * task stuck or crashed never blocks another's, and never the daemon: a
 * Tokio task that panics only ends itself.
 *
 * control.rs routes every command for a non-virtual device here
 * (Registry::command), after checking it against the device's
 * capabilities -- so an adapter only ever receives valid commands.
 *
 * AT SETUP TIME (the wizard, wizard.rs) an adapter answers two things:
 *   probe   the wizard's "test" step: is the device there, is it what the
 *           template says, and what does it tell about itself (its MAC,
 *           its own name)?
 *   action  a named step a template asks for ("pair" on the LG TV):
 *           may take long (waiting for the user to confirm on the device)
 *           and returns new values, plain or secret.
 * Both get the values setup collected so far and fail with an error KIND
 * (templates::ErrorKind), which the wizard turns into a sentence a person
 * understands.
 */
pub mod channels;
pub mod esp_prov;
pub mod ir_blaster;
pub mod lg_webos;
pub mod m4_led;
pub mod net;
pub mod wled;
#[cfg(test)]
pub mod lg_sim;
#[cfg(test)]
pub mod wled_sim;

use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::control::Control;
use crate::device::{Device, Health};
use crate::secrets::DeviceSecrets;
use crate::state::DeviceId;
use crate::templates::ErrorKind;

/* What a device's task is asked to do. */
pub enum DeviceCmd {
    /* Set a capability's state. The reply comes AFTER the device
     * confirmed and the task reported the new state (Hub::report), so the
     * caller can read the device back and see the result. */
    Command {
        capability: String,
        value: Value,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /* A one-off action (issue #44, device::check_action): press a button,
     * list the channels. Its reply carries the action's result (the list),
     * `{}` if it has none. */
    Action {
        capability: String,
        name: String,
        args: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

impl DeviceCmd {
    /* Answers with an error, whatever the request was (a device that
     * can't do it now, or a task that has no actions at all). */
    pub fn refuse(self, why: impl Into<String>) {
        let why = why.into();
        match self {
            DeviceCmd::Command { reply, .. } => {
                let _ = reply.send(Err(why));
            }
            DeviceCmd::Action { reply, .. } => {
                let _ = reply.send(Err(why));
            }
        }
    }
}

/* How to reach a running device task. */
pub struct DeviceHandle {
    pub commands: mpsc::Sender<DeviceCmd>,
}

/* What a device task may do to the hub: report state, and read or store
 * ITS OWN device's secrets (a TV's pairing key). Cheap to clone. */
#[derive(Clone)]
pub struct Hub {
    control: Control,
}

impl Hub {
    /* The device's real state changed (or was confirmed). Recorded with
     * Origin::Device: the hardware's word counts, read-only capabilities
     * (sensors) included. */
    pub async fn report(&self, id: &str, capability: &str, value: Value) -> Result<Device, String> {
        self.control.report(id, capability, value).await
    }

    /* The device became reachable or not (Device::online). Only a
     * change is passed on, so a task may call this on every attempt. */
    pub async fn set_online(&self, id: &str, online: Health) {
        if let Err(e) = self.control.set_online(id, online).await {
            println!("adapters: could not record whether {id} is online: {e}");
        }
    }

    pub fn secrets(&self, id: &str) -> DeviceSecrets {
        self.control.secrets().get(id)
    }

    /* An IR blaster's learned codes (issue #42, ir_codes.rs). */
    pub fn ir_codes(&self) -> &crate::ir_codes::IrCodes {
        self.control.ir_codes()
    }

    /* E.g. a TV that issued a new pairing key. */
    pub fn store_secrets(&self, id: &str, values: DeviceSecrets) {
        self.control.secrets().set(id, values);
    }

    /* A plain setting the task learned by itself (a TV's MAC): saved in the
     * device's config, the task keeps running. */
    pub async fn store_config(&self, id: &str, key: &str, value: &str) {
        let config = [(key.to_string(), value.to_string())].into();
        if let Err(e) = self.control.store_config(id, config).await {
            println!("adapters: could not save {id}'s {key}: {e}");
        }
    }
}

/* What setup has collected (wizard.rs): plain values ("host", "mac") and
 * secret ones (a password, a pairing key). */
#[derive(Default, Clone, Debug)]
pub struct SetupValues {
    pub plain: BTreeMap<String, String>,
    pub secret: DeviceSecrets,
}

/* What a probe learned about the device. */
#[derive(Default, Debug, PartialEq)]
pub struct Probe {
    /* New plain values, e.g. {"mac": "b8d61a6b33ac"} for the template's
     * identity. Never replace what the user typed or discovery found. */
    pub values: BTreeMap<String, String>,
    /* The name the device gives itself, if any (offered as the default
     * name). */
    pub name: Option<String>,
    /* One line for the test step's "OK" screen: "WLED 16.0.1". */
    pub summary: String,
}

/* Why a setup step failed: the kind picks the sentence the person sees
 * (overridable per template), the detail is for the log and the "more"
 * line. */
#[derive(Debug, PartialEq)]
pub struct SetupError {
    pub kind: ErrorKind,
    pub detail: String,
}

impl SetupError {
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        SetupError {
            kind,
            detail: detail.into(),
        }
    }
}

/* An async answer from a trait object. (An `async fn` in a trait can't be
 * called through `dyn Adapter`; a boxed future can. The Box costs one
 * allocation per call -- nothing, at setup time.) */
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/* One protocol family. `Send + Sync`: the registry is shared by every
 * connection's task. */
pub trait Adapter: Send + Sync {
    /* The id templates and devices use ("m4-led"). */
    fn id(&self) -> &'static str;

    /* Devices that are part of the hub itself (the LED): created at start
     * if they don't exist yet. Most adapters have none. */
    fn builtin_devices(&self) -> Vec<Device> {
        Vec::new()
    }

    /* Starts the task that runs one device (see the header). */
    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle;

    /* Setup's "test" step (see the header). Adapters of built-in devices
     * have no setup, hence the default. */
    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        let _ = values;
        Box::pin(async { Err(SetupError::new(ErrorKind::Unsupported, "this adapter has no setup")) })
    }

    /* A named setup action (see the header): the new values it produced. */
    fn action<'a>(&'a self, name: &'a str, values: &'a SetupValues) -> BoxFuture<'a, Result<SetupValues, SetupError>> {
        let _ = values;
        Box::pin(async move { Err(SetupError::new(ErrorKind::Unsupported, format!("this adapter has no action {name:?}"))) })
    }
}

/* All adapters of this build, and the devices running on them. */
pub struct Registry {
    adapters: BTreeMap<&'static str, Box<dyn Adapter>>,
    /* std Mutex, not tokio's: only held for a quick map lookup, never
     * across an .await. */
    running: Mutex<HashMap<DeviceId, mpsc::Sender<DeviceCmd>>>,
}

impl Registry {
    pub fn new(adapters: Vec<Box<dyn Adapter>>) -> Self {
        Registry {
            adapters: adapters.into_iter().map(|a| (a.id(), a)).collect(),
            running: Mutex::new(HashMap::new()),
        }
    }

    /* For templates.rs: which adapters a template may name. */
    pub fn ids(&self) -> Vec<&'static str> {
        self.adapters.keys().copied().collect()
    }

    /* For the wizard: the adapter a template names. */
    pub fn get(&self, id: &str) -> Option<&dyn Adapter> {
        self.adapters.get(id).map(|a| a.as_ref())
    }

    /* At start: create the built-in devices that are missing, then start
     * a task for every device that has an adapter. A device whose adapter
     * isn't in this build (a newer registry) is left alone, logged. */
    pub async fn start_all(&self, control: &Control) {
        for adapter in self.adapters.values() {
            for device in adapter.builtin_devices() {
                match control.get(&device.id).await {
                    Ok(Some(_)) => {}
                    Ok(None) => match control.add_builtin(device.clone()).await {
                        Ok(_) => println!("adapters: {} added (built into the hub)", device.id),
                        Err(e) => println!("adapters: could not add {}: {e}", device.id),
                    },
                    Err(_) => return, /* shutting down */
                }
            }
        }
        let Ok(devices) = control.list().await else { return };
        for device in devices.iter().filter(|d| !d.source.is_virtual()) {
            self.start(device, control);
        }
    }

    /* Starts (or restarts) one device's task. */
    pub fn start(&self, device: &Device, control: &Control) {
        let Some(adapter) = self.adapters.get(device.source.as_str()) else {
            println!("adapters: {} needs adapter {:?}, which this hub doesn't have", device.id, device.source.as_str());
            return;
        };
        let handle = adapter.start(device, Hub { control: control.clone() });
        self.running.lock().unwrap().insert(device.id.clone(), handle.commands);
    }

    /* Stops a device's task (the device was removed): dropping the only
     * sender ends the task's command loop. */
    pub fn stop(&self, id: &str) {
        self.running.lock().unwrap().remove(id);
    }

    /* A command for a hardware device (already checked by control.rs). */
    pub async fn command(&self, id: &str, capability: &str, value: Value) -> Result<(), String> {
        let (reply, reply_rx) = oneshot::channel();
        self.send(
            id,
            DeviceCmd::Command {
                capability: capability.to_string(),
                value,
                reply,
            },
        )
        .await?;
        reply_rx.await.map_err(|_| format!("{id}'s adapter dropped the command"))?
    }

    /* An action for a hardware device (already checked by control.rs). */
    pub async fn action(&self, id: &str, capability: &str, name: &str, args: Value) -> Result<Value, String> {
        let (reply, reply_rx) = oneshot::channel();
        self.send(
            id,
            DeviceCmd::Action {
                capability: capability.to_string(),
                name: name.to_string(),
                args,
                reply,
            },
        )
        .await?;
        reply_rx.await.map_err(|_| format!("{id}'s adapter dropped the action"))?
    }

    async fn send(&self, id: &str, cmd: DeviceCmd) -> Result<(), String> {
        let commands = self
            .running
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("{id}'s adapter isn't running"))?;
        commands.send(cmd).await.map_err(|_| format!("{id}'s adapter has stopped"))
    }
}
