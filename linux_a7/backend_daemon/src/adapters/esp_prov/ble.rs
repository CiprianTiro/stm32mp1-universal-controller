/*
 * ble.rs -- the Bluetooth side of esp_prov (issue #42): finding devices
 * that wait for WiFi setup, and talking to their provisioning service.
 *
 * Here the hub is the Bluetooth CLIENT (it connects to the device), the
 * other way round from the hub's own setup in ../../ble.rs, where the
 * phone connects to the hub. Both go through BlueZ with bluer.
 *
 * THE DEVICE'S SERVICE. Espressif's provisioning offers one GATT service
 * (our IR blasters advertise it with their own UUID, so the hub finds
 * only them) whose characteristics are the endpoints. Each is named by its
 * "Characteristic User Description" descriptor (0x2901): "proto-ver",
 * "prov-session", "prov-config", ... A request is WRITTEN to the endpoint's
 * characteristic (with response, so long messages -- SRP's 384-byte values
 * -- use BlueZ's long writes), and the answer READ back from it.
 *
 * BLUEZ REMEMBERS SERVICES. After an interrupted session (seen on the
 * bench), BlueZ kept a stale copy of the device's services and the next
 * connection found none. So if the service or its endpoints are missing,
 * the device is removed from BlueZ's cache, found again, and connected
 * once more.
 *
 * THE RADIO. The hub keeps its Bluetooth off when unused (../../ble.rs). It
 * is switched on here for the setup, and back off afterwards if it was off
 * before.
 */
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::gatt::WriteOp;
use bluer::{Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport, Uuid};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::{ProvError, Transport};
use crate::adapters::BoxFuture;

/* How long to look for devices. The blaster advertises every 160 ms, so
 * a few seconds find it; more for devices a room away. */
const SCAN_TIME: Duration = Duration::from_secs(8);
/* Connecting, and each request/answer. */
const LINK_TIMEOUT: Duration = Duration::from_secs(15);

/* The 0x2901 descriptor: a characteristic's name. */
const USER_DESCRIPTION: Uuid = Uuid::from_u128(0x0000_2901_0000_1000_8000_0080_5f9b_34fb);

/* A device found waiting for setup. */
#[derive(Debug, Clone)]
pub struct Waiting {
    pub address: Address,
    /* "PROV_IRB_91E4" */
    pub name: String,
}

/* The hub's Bluetooth adapter, switched on. Switched back off when
 * dropped, if it was off before. */
pub struct Radio {
    adapter: Adapter,
    was_on: bool,
}

impl Radio {
    pub async fn on() -> Result<Radio, ProvError> {
        let err = |e: bluer::Error| ProvError::Link(format!("the hub's Bluetooth: {e}"));
        let session = bluer::Session::new().await.map_err(err)?;
        let adapter = session.default_adapter().await.map_err(err)?;
        let was_on = adapter.is_powered().await.map_err(err)?;
        if !was_on {
            adapter.set_powered(true).await.map_err(err)?;
        }
        Ok(Radio { adapter, was_on })
    }

    /* Devices advertising `service`, found within SCAN_TIME (sooner if one
     * turns up: 2 more seconds for others). */
    pub async fn find(&self, service: Uuid) -> Result<Vec<Waiting>, ProvError> {
        let err = |e: bluer::Error| ProvError::Link(format!("scanning: {e}"));
        let filter = DiscoveryFilter {
            uuids: HashSet::from([service]),
            transport: DiscoveryTransport::Le,
            ..Default::default()
        };
        self.adapter.set_discovery_filter(filter).await.map_err(err)?;
        let mut events = self.adapter.discover_devices().await.map_err(err)?;
        let mut found: Vec<Waiting> = Vec::new();
        let mut deadline = tokio::time::Instant::now() + SCAN_TIME;
        loop {
            match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(Some(AdapterEvent::DeviceAdded(address))) => {
                    let Ok(device) = self.adapter.device(address) else { continue };
                    /* The filter works on what BlueZ knows, which can include
                     * devices seen earlier: check it's advertising now. */
                    let offers = device.uuids().await.ok().flatten().is_some_and(|u| u.contains(&service));
                    if offers && !found.iter().any(|f| f.address == address) {
                        let name = device.name().await.ok().flatten().unwrap_or_else(|| address.to_string());
                        found.push(Waiting { address, name });
                        deadline = deadline.min(tokio::time::Instant::now() + Duration::from_secs(2));
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break, /* dropping `events` stops the scan */
            }
        }
        Ok(found)
    }

    /* Connects to a device's provisioning service. On a stale BlueZ cache
     * (no service, or no named endpoints), forgets the device, finds it
     * again, and tries once more. */
    pub async fn connect(&self, waiting: &Waiting, service: Uuid) -> Result<Link, ProvError> {
        match self.try_connect(waiting.address, service).await {
            Ok(link) => Ok(link),
            Err(first) => {
                println!("esp_prov: {}: {first}; forgetting it and trying again", waiting.name);
                let _ = self.adapter.remove_device(waiting.address).await;
                let again = self.find(service).await?;
                if !again.iter().any(|w| w.address == waiting.address) {
                    return Err(first);
                }
                self.try_connect(waiting.address, service).await
            }
        }
    }

    async fn try_connect(&self, address: Address, service: Uuid) -> Result<Link, ProvError> {
        let err = |e: bluer::Error| ProvError::Link(e.to_string());
        let device = self.adapter.device(address).map_err(err)?;
        let connect = async {
            if !device.is_connected().await.map_err(err)? {
                device.connect().await.map_err(err)?;
            }
            let mut endpoints = HashMap::new();
            for s in device.services().await.map_err(err)? {
                if s.uuid().await.map_err(err)? != service {
                    continue;
                }
                for c in s.characteristics().await.map_err(err)? {
                    for d in c.descriptors().await.map_err(err)? {
                        if d.uuid().await.map_err(err)? == USER_DESCRIPTION {
                            let name = String::from_utf8_lossy(&d.read().await.map_err(err)?).trim_end_matches('\0').to_string();
                            endpoints.insert(name, c.clone());
                        }
                    }
                }
            }
            for needed in ["proto-ver", "prov-session", "prov-config"] {
                if !endpoints.contains_key(needed) {
                    return Err(ProvError::Link(format!("the device's service has no {needed:?}")));
                }
            }
            Ok(endpoints)
        };
        let endpoints = match tokio::time::timeout(LINK_TIMEOUT, connect).await {
            Ok(result) => result,
            Err(_) => Err(ProvError::Link("connecting took too long".into())),
        };
        match endpoints {
            Ok(endpoints) => Ok(Link { device, endpoints }),
            Err(e) => {
                let _ = device.disconnect().await;
                Err(e)
            }
        }
    }
}

impl Drop for Radio {
    fn drop(&mut self) {
        if !self.was_on {
            /* Drop can't wait: switch off in the background. */
            let adapter = self.adapter.clone();
            tokio::spawn(async move {
                let _ = adapter.set_powered(false).await;
            });
        }
    }
}

/* A connection to one device's provisioning service. Disconnects when
 * dropped. */
pub struct Link {
    device: Device,
    endpoints: HashMap<String, Characteristic>,
}

impl Transport for Link {
    fn exchange<'a>(&'a mut self, endpoint: &'a str, data: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let c = self.endpoints.get(endpoint).ok_or_else(|| format!("no endpoint {endpoint}"))?;
            let request = CharacteristicWriteRequest {
                op_type: WriteOp::Request,
                ..Default::default()
            };
            let exchange = async {
                c.write_ext(data, &request).await.map_err(|e| e.to_string())?;
                c.read().await.map_err(|e| e.to_string())
            };
            match tokio::time::timeout(LINK_TIMEOUT, exchange).await {
                Ok(result) => result,
                Err(_) => Err(format!("{endpoint}: no answer in time")),
            }
        })
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let device = self.device.clone();
        tokio::spawn(async move {
            let _ = device.disconnect().await;
        });
    }
}

/* Finds the devices waiting for setup and sets up the one that accepts
 * `code` (a code belongs to exactly one device, so trying each finds the
 * right one without asking the person to pick). Returns its name and its
 * IP address on the WiFi. */
pub async fn provision_nearby(service: Uuid, code: &str, ssid: &str, password: &str) -> Result<(String, String), ProvError> {
    let radio = Radio::on().await?;
    let waiting = radio.find(service).await?;
    if waiting.is_empty() {
        return Err(ProvError::Link(
            "no device is waiting for setup nearby (is it plugged in, close to the hub, without WiFi yet?)".into(),
        ));
    }
    let mut last = ProvError::WrongCode;
    for device in &waiting {
        let link = match radio.connect(device, service).await {
            Ok(link) => link,
            Err(e) => {
                last = e;
                continue;
            }
        };
        match super::provision(link, code, ssid, password, super::TIMING).await {
            Ok(ip) => return Ok((device.name.clone(), ip)),
            /* Another blaster's code: try the next one. */
            Err(ProvError::WrongCode) => {
                println!("esp_prov: {} doesn't take this code", device.name);
                last = ProvError::WrongCode;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ---- against a real device waiting for setup (blaster: `wifi forget`) ----
     *
     *   IRB_CODE=XXXX-XXXX-XXXX-XXXX WIFI_SSID=... WIFI_PASS=... \
     *   cargo test esp_prov::ble::tests::live -- --ignored --nocapture
     *
     * The hub's own Bluetooth code: scan for our service, connect, SRP6a
     * with the code, send the WiFi, wait for "connected". */
    #[tokio::test]
    #[ignore]
    async fn live() {
        let code: String = std::env::var("IRB_CODE")
            .expect("set IRB_CODE")
            .chars()
            .filter(|c| *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let ssid = std::env::var("WIFI_SSID").expect("set WIFI_SSID");
        let password = std::env::var("WIFI_PASS").expect("set WIFI_PASS");
        let service = Uuid::parse_str("1f2b9c64-3a5e-4c1d-9f0a-5b6e7d8c9a01").unwrap();
        let started = std::time::Instant::now();
        let result = provision_nearby(service, &code, &ssid, &password).await;
        println!("result after {:.1} s: {result:?}", started.elapsed().as_secs_f32());
        result.unwrap();
    }
}
