/*
 * test_hub.rs -- the hub around ONE device, for adapter tests (test
 * builds only): the state actor, a registry with the adapter under test,
 * and control.rs -- what a device's task talks to. (wled.rs and
 * lg_webos.rs have their own copies, written before this one.)
 */
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};

use super::{Adapter, Registry};
use crate::control::Control;
use crate::device::Device;
use crate::secrets::Secrets;
use crate::state::{self, Event, Outputs};

pub struct TestHub {
    pub control: Control,
    pub registry: Arc<Registry>,
    events: broadcast::Receiver<Event>,
    events_tx: broadcast::Sender<Event>,
    id: String,
}

impl TestHub {
    /* The hub with `device` in its registry, its task started. */
    pub async fn start(device: Device, adapter: Box<dyn Adapter>) -> TestHub {
        let id = device.id.clone();
        let (state_tx, state_rx) = mpsc::channel(8);
        let (events_tx, events) = broadcast::channel(64);
        let outputs = Outputs {
            changed_tx: watch::channel(()).0,
            events_tx: events_tx.clone(),
            save_tx: watch::channel(Vec::new()).0,
        };
        tokio::spawn(state::run(state_rx, [(id.clone(), device)].into(), outputs));
        let registry = Arc::new(Registry::new(vec![adapter]));
        let secrets = Arc::new(Secrets::new(Default::default(), watch::channel(Vec::new()).0));
        let control = Control::new(state_tx, registry.clone(), secrets);
        registry.start_all(&control).await;
        TestHub {
            control,
            registry,
            events,
            events_tx,
            id,
        }
    }

    /* Every device change from now on (automations.rs's tests). */
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    /* Waits (up to 10 s) for the device to look like `wanted`. */
    pub async fn until(&mut self, wanted: impl Fn(&Device) -> bool) -> Device {
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);
        if let Some(d) = self.control.get(&self.id).await.unwrap() {
            if wanted(&d) {
                return d;
            }
        }
        loop {
            tokio::select! {
                _ = &mut deadline => panic!("timed out; device is {:?}", self.control.get(&self.id).await),
                event = self.events.recv() => if let Ok(Event::Changed(d)) = event {
                    if d.id == self.id && wanted(&d) { return d }
                },
            }
        }
    }
}
