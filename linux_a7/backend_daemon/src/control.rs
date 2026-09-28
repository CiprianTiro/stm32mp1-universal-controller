/*
 * control.rs -- the one door every device command goes through (issue #34),
 * whoever sends it: the touchscreen or the app (ws.rs), or the cloud
 * (mqtt.rs).
 *
 * A command ("set lamp-1's dimmer to 30") is:
 *   1. checked against the device's capabilities (device.rs) -- for EVERY
 *      device, so hardware never even receives an invalid command;
 *   2. carried out according to where the device's truth is (its
 *      source, issue #40):
 *        virtual  -> state.rs changes it directly;
 *        hardware -> the device's ADAPTER (adapters/) carries it out, and
 *                    state.rs then records what the device CONFIRMED
 *                    (which is normally, but not by assumption, what was
 *                    asked).
 * The answer is the device as it is afterwards, or the reason it failed.
 *
 * Before #34, the LED ("ld7") was a special case in both ws.rs and mqtt.rs;
 * until #40 it still had its own branch here. Now it's an ordinary device
 * run by the "m4-led" adapter, like every future hardware device.
 */
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::adapters::Registry;
use crate::device::{self, Device, Health, Origin};
use crate::secrets::Secrets;
use crate::state::Msg;

/* Cheap to clone (a channel handle and two Arcs): ws.rs gives one to
 * every client. */
#[derive(Clone)]
pub struct Control {
    state_tx: mpsc::Sender<Msg>,
    adapters: Arc<Registry>,
    /* Devices' secrets (issue #40): removed with their device. */
    secrets: Arc<Secrets>,
}

impl Control {
    pub fn new(state_tx: mpsc::Sender<Msg>, adapters: Arc<Registry>, secrets: Arc<Secrets>) -> Self {
        Control {
            state_tx,
            adapters,
            secrets,
        }
    }

    pub fn secrets(&self) -> &Secrets {
        &self.secrets
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

    /* Adding by hand is for virtual devices only: hardware appears
     * through the wizard (#40) or as a built-in device, never by a client
     * simply claiming it exists. */
    pub async fn add(&self, device: Device) -> Result<Device, String> {
        if !device.source.is_virtual() {
            return Err(format!("{} devices are added with the wizard, not by hand", device.source));
        }
        self.ask(|reply| Msg::AddDevice { device, reply }).await?
    }

    /* A device the wizard set up (wizard.rs): stored, its secrets kept
     * apart (secrets.rs), then its adapter started. */
    pub async fn add_from_setup(&self, device: Device, secrets: crate::secrets::DeviceSecrets) -> Result<Device, String> {
        let device = self.ask(|reply| Msg::AddDevice { device, reply }).await??;
        if !secrets.is_empty() {
            self.secrets.set(&device.id, secrets);
        }
        self.adapters.start(&device, self);
        Ok(device)
    }

    /* "Pair again" / "change settings" finished (wizard.rs): the new
     * secrets first, then the config -- which restarts the adapter, so it
     * starts with both. */
    pub async fn update_from_setup(
        &self,
        id: &str,
        config: std::collections::BTreeMap<String, String>,
        secrets: crate::secrets::DeviceSecrets,
    ) -> Result<Device, String> {
        if !secrets.is_empty() {
            self.secrets.set(id, secrets);
        }
        self.set_config(id, config).await
    }

    /* The adapters, for the wizard's probe and actions. */
    pub fn adapters(&self) -> &Registry {
        &self.adapters
    }

    /* A built-in device, added by its adapter at start (adapters/). */
    pub async fn add_builtin(&self, device: Device) -> Result<Device, String> {
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

    /* Removed means gone for good: its adapter task stops and its
     * secrets are deleted (issue #40). */
    pub async fn remove(&self, id: &str) -> Result<(), String> {
        self.ask(|reply| Msg::RemoveDevice { id: id.to_string(), reply }).await??;
        self.adapters.stop(id);
        self.secrets.remove(id);
        Ok(())
    }

    /* The command itself -- see the header comment. */
    pub async fn command(&self, id: &str, capability: &str, value: serde_json::Value) -> Result<Device, String> {
        let device = self.get(id).await?.ok_or_else(|| format!("unknown device {id:?}"))?;
        /* 1. The rules, for every device (the result is thrown away here:
         *    state.rs applies the change itself, below). */
        device::set_capability(&device, capability, value.clone(), Origin::Client)?;
        /* 2. Carry it out. */
        if device.source.is_virtual() {
            return self.set(id, capability, value, Origin::Client).await;
        }
        /* The adapter answers once the device confirmed and the new state
         * is recorded: read it back. */
        self.adapters.command(id, capability, value).await?;
        self.get(id).await?.ok_or_else(|| format!("{id} disappeared"))
    }

    /* A one-off action (issue #44): checked by device.rs's rules like a
     * command, then carried out by the device's adapter. Virtual devices
     * have nothing that could carry one out. Returns the action's result
     * (e.g. a TV's channel list; `{}` for a button press). */
    pub async fn action(&self, id: &str, capability: &str, name: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let device = self.get(id).await?.ok_or_else(|| format!("unknown device {id:?}"))?;
        device::check_action(&device, capability, name, &args)?;
        if device.source.is_virtual() {
            return Err(format!("{id} is a virtual device: it has nothing to carry out actions"));
        }
        self.adapters.action(id, capability, name, args).await
    }

    /* Adds the capabilities a device's template lists but the device
     * lacks (a template that gained one after the device was added, e.g.
     * #44's `remote` for TVs added in #40). Returns the ones added. */
    pub async fn add_missing_capabilities(&self, id: &str, names: Vec<String>) -> Result<Vec<String>, String> {
        self.ask(|reply| Msg::AddMissingCapabilities {
            id: id.to_string(),
            names,
            reply,
        })
        .await?
    }

    /* Changes some of a device's adapter settings (the given keys only),
     * and restarts its adapter task so it uses them (issue #40: a device
     * found at a new address). */
    pub async fn set_config(&self, id: &str, config: std::collections::BTreeMap<String, String>) -> Result<Device, String> {
        let device = self
            .ask(|reply| Msg::SetConfig {
                id: id.to_string(),
                config,
                reply,
            })
            .await??;
        if !device.source.is_virtual() {
            self.adapters.start(&device, self);
        }
        Ok(device)
    }

    /* Saves adapter settings the adapter itself learned (a TV's MAC), WITHOUT
     * restarting it (unlike set_config): it already uses them. */
    pub async fn store_config(&self, id: &str, config: std::collections::BTreeMap<String, String>) -> Result<Device, String> {
        self.ask(|reply| Msg::SetConfig {
            id: id.to_string(),
            config,
            reply,
        })
        .await?
    }

    /* A device's real state, as its adapter reports it (adapters::Hub). */
    pub async fn report(&self, id: &str, capability: &str, value: serde_json::Value) -> Result<Device, String> {
        self.set(id, capability, value, Origin::Device).await
    }

    /* A device's reachability, as its adapter sees it (adapters::Hub). */
    pub async fn set_online(&self, id: &str, online: Health) -> Result<Device, String> {
        self.ask(|reply| Msg::SetOnline {
            id: id.to_string(),
            online,
            reply,
        })
        .await?
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
}
