use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::state::{DeviceId, DeviceState, Msg};

const STATE_TOPIC: &str = "hub/devices/state";
const COMMAND_TOPIC: &str = "hub/commands";

/* A command published to hub/commands sets a device's desired properties --
 * the classic "device twin" shape: the cloud (or anything publishing here)
 * declares what a device SHOULD be, and it gets applied the same way a
 * WebSocket UpdateDevice would. */
#[derive(serde::Deserialize)]
struct Command {
    id: DeviceId,
    properties: DeviceState,
}

/* Broker address is configurable via environment variables rather than
 * hardcoded, per the DoD -- defaults match a plain local test broker. */
fn broker_address() -> (String, u16) {
    let host = std::env::var("MQTT_BROKER_HOST").unwrap_or_else(|_| "localhost".into());
    let port = std::env::var("MQTT_BROKER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1883);
    (host, port)
}

pub async fn run(state_tx: mpsc::Sender<Msg>) {
    let (host, port) = broker_address();
    let mut mqttoptions = MqttOptions::new("backend-daemon", host, port);
    mqttoptions.set_keep_alive(Duration::from_secs(5));

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

    client
        .subscribe(COMMAND_TOPIC, QoS::AtLeastOnce)
        .await
        .expect("failed to subscribe to command topic");

    tokio::spawn(publish_state_periodically(client, state_tx.clone()));

    /* rumqttc's event loop handles reconnection itself: calling
     * eventloop.poll() again after an error is the documented way to
     * reconnect, no manual restart logic needed -- this loop just keeps
     * calling it forever. */
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::Publish(publish))) if publish.topic == COMMAND_TOPIC => {
                handle_command(&publish.payload, &state_tx).await;
            }
            Ok(_) => {}
            Err(e) => {
                println!("mqtt: connection error: {e}, retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_command(payload: &[u8], state_tx: &mpsc::Sender<Msg>) {
    match serde_json::from_slice::<Command>(payload) {
        Ok(cmd) => {
            let _ = state_tx
                .send(Msg::UpdateDevice {
                    id: cmd.id,
                    properties: cmd.properties,
                })
                .await;
        }
        Err(e) => println!("mqtt: bad command payload: {e}"),
    }
}

/* Periodically asks state.rs for the full device-twin snapshot and
 * publishes it -- the "state update published from the board is observable
 * on the broker" half of the DoD. */
async fn publish_state_periodically(client: AsyncClient, state_tx: mpsc::Sender<Msg>) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;

        let (reply_tx, reply_rx) = oneshot::channel();
        if state_tx
            .send(Msg::GetAllDevices { reply: reply_tx })
            .await
            .is_err()
        {
            break;
        }
        let devices: std::collections::HashMap<DeviceId, DeviceState> =
            reply_rx.await.unwrap_or_default();

        let Ok(payload) = serde_json::to_vec(&devices) else {
            continue;
        };
        let _ = client
            .publish(STATE_TOPIC, QoS::AtLeastOnce, false, payload)
            .await;
    }
}
