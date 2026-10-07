/*
 * adapters -- the code that speaks each device family's protocol (issue
 * #40). A TEMPLATE (templates.rs) says what a device type is; its ADAPTER
 * makes it work. One adapter per protocol family: "m4-led" (the board's
 * LED, through the Cortex-M4), "wled", "lg-webos", "wiz", ... -- and
 * "http" (http_generic.rs, issue #75), which runs whatever a template's
 * "http" block describes: simple devices need no adapter of their own.
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
pub mod camera;
pub mod channels;
pub mod cloud;
pub mod esp_prov;
pub mod ezviz;
pub mod http_generic;
pub mod ipcam;
pub mod ir_blaster;
pub mod lg_webos;
pub mod m4_led;
pub mod miio;
pub mod roborock;
pub mod mqtt_generic;
pub mod roborock_cloud;
pub mod roborock_map;
pub mod roborock_proto;
pub mod tapo;
pub mod net;
pub mod onvif;
pub mod wiz;
pub mod wled;
#[cfg(test)]
pub mod http_sim;
#[cfg(test)]
pub mod lg_sim;
#[cfg(test)]
pub mod test_hub;
#[cfg(test)]
pub mod wiz_sim;
#[cfg(test)]
pub mod wled_sim;

use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Notify};

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

/* Issue #74: a command's or an action's reply, once the task knows which
 * one it got (a switch command and an option action handled alike). */
pub type Reply = Box<dyn FnOnce(Result<(), String>) + Send>;

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
    /* Issue #72: "read your state now" (a webhook said something changed).
     * Only tasks that poll have one; for the others it would mean nothing. */
    pub refresh: Option<Arc<Notify>>,
}

impl DeviceHandle {
    pub fn new(commands: mpsc::Sender<DeviceCmd>) -> Self {
        DeviceHandle { commands, refresh: None }
    }

    pub fn with_refresh(mut self, refresh: Arc<Notify>) -> Self {
        self.refresh = Some(refresh);
        self
    }
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

    /* The device as the hub knows it now (issue #72: a task that hasn't
     * heard from its device yet falls back to the saved state). */
    pub async fn device(&self, id: &str) -> Option<Device> {
        self.control.get(id).await.ok().flatten()
    }

    /* Issue #85: capabilities that come and go with a device's setting
     * (an IR device used as a light): added with their neutral value /
     * removed; only changes are announced. */
    pub async fn add_capabilities(&self, id: &str, names: &[&str]) -> Result<Vec<String>, String> {
        self.control.add_missing_capabilities(id, names.iter().map(|n| n.to_string()).collect()).await
    }

    pub async fn remove_capabilities(&self, id: &str, names: &[&str]) -> Result<Vec<String>, String> {
        self.control.remove_capabilities(id, names.iter().map(|n| n.to_string()).collect()).await
    }

    /* Issue #72: a battery device reported (Device::last_seen). */
    pub async fn seen(&self, id: &str) {
        self.control.seen(id).await;
    }

    pub fn secrets(&self, id: &str) -> DeviceSecrets {
        self.control.secrets().get(id)
    }

    /* An IR blaster's learned codes (issue #42, ir_codes.rs). */
    pub fn ir_codes(&self) -> &crate::ir_codes::IrCodes {
        self.control.ir_codes()
    }

    /* Issue #74: a vendor account's session (config "account"), and
     * saving a renewed one for all of its devices (accounts.rs). */
    pub fn account_session(&self, account: &str) -> Option<String> {
        crate::accounts::get(self.control.secrets(), account).map(|a| a.session)
    }

    /* Issue #74: the cloud session a device uses. Its account's; only a
     * device set up before accounts were saved (no "account" in its
     * config) uses its own copy. One whose account was signed out gets
     * none -- signing out must sign it out -- and its old copy is
     * deleted. */
    pub fn cloud_session(&self, device: &Device) -> Option<String> {
        match device.config.get("account") {
            Some(account) => {
                let session = self.account_session(account);
                if session.is_none() && self.control.secrets().remove_one(&device.id, "cloud_session") {
                    println!("adapters: {}: account {account} is signed out: its old session copy deleted", device.id);
                }
                session
            }
            None => self.secrets(&device.id).get("cloud_session").map(|s| s.expose().to_string()),
        }
    }

    pub fn store_account_session(&self, account: &str, session: String) {
        crate::accounts::update_session(self.control.secrets(), account, session);
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
    /* The template being set up (the generic HTTP adapter's probe reads
     * its "http" block). */
    pub template: String,
    /* Issue #74: a vendor_login step's `list_action` answers with the
     * account's devices; the person picks one (wizard.rs). */
    pub cloud_devices: Vec<CloudDevice>,
}

/* One device of a vendor account (issue #74). */
#[derive(Default, Clone, Debug)]
pub struct CloudDevice {
    /* The vendor's id for it (Roborock's "duid"). */
    pub id: String,
    pub name: String,
    /* One line under the name: "Roborock S7 - online". */
    pub detail: String,
    /* false: shown, but can't be picked -- `detail` says why ("this model
     * isn't supported yet"). */
    pub available: bool,
    /* What the device needs, merged into setup's values when picked. */
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
    /* Their "read your state now" signals (issue #72), where they have one. */
    refreshers: Mutex<HashMap<DeviceId, Arc<Notify>>>,
}

impl Registry {
    pub fn new(adapters: Vec<Box<dyn Adapter>>) -> Self {
        Registry {
            adapters: adapters.into_iter().map(|a| (a.id(), a)).collect(),
            running: Mutex::new(HashMap::new()),
            refreshers: Mutex::new(HashMap::new()),
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
        match handle.refresh {
            Some(refresh) => self.refreshers.lock().unwrap().insert(device.id.clone(), refresh),
            None => self.refreshers.lock().unwrap().remove(&device.id),
        };
    }

    /* Stops a device's task (the device was removed): dropping the only
     * sender ends the task's command loop. */
    pub fn stop(&self, id: &str) {
        self.running.lock().unwrap().remove(id);
        self.refreshers.lock().unwrap().remove(id);
    }

    /* "Read your state now" (a webhook, issue #72). false: this device's
     * task has nothing to refresh (it's told by the device anyway). */
    pub fn refresh(&self, id: &str) -> bool {
        match self.refreshers.lock().unwrap().get(id) {
            Some(refresh) => {
                refresh.notify_one();
                true
            }
            None => false,
        }
    }

    /* Issue #99: can this device be asked to "read now"? (Nothing asked.) */
    pub fn can_refresh(&self, id: &str) -> bool {
        self.refreshers.lock().unwrap().contains_key(id)
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
