/*
 * ws.rs -- the local WebSocket API, ws://<hub>:8080/ws: what the touchscreen
 * UI, the phone app and tools/hub_ws.py talk to.
 *
 * PROTOCOL v2 (issue #34). JSON text messages. The client sends requests,
 * {"action": "...", ...}; the server answers each one, in order, with
 * {"type": "...", ...}. After "subscribe", the server additionally PUSHES
 * events whenever a device changes, so a client never has to poll. Replies
 * and events are told apart by their "type".
 *
 *   hello                                  -> hello {protocol, capabilities}
 *   list_devices                           -> devices {devices: [...]}
 *   get_device {id}                        -> device {device} (null: unknown)
 *   add_device {device}                    -> device {device}
 *   update_device_info {id, name?, room?}  -> device {device}
 *   remove_device {id}                     -> ack
 *   command {id, capability, value}        -> device {device}
 *        e.g. {"action":"command","id":"lamp-1","capability":"dimmer",
 *              "value":{"level":30}}
 *   subscribe                              -> ack, then events:
 *        device_changed {device} / device_removed {id} /
 *        events_lost (too slow to keep up: list_devices again)
 *   get_network_status, wifi_scan, wifi_connect, wifi_forget,
 *   set_wifi_country                       -> see network.rs (#61)
 *   anything refused                       -> error {message}
 *
 * A device is device.rs's JSON: {"id", "name", "room", "template", "source",
 * "capabilities": {"switch": {"on": true}, ...}}. Every command goes
 * through control.rs, the same door the cloud uses.
 *
 * The full reference with examples is on the wiki (Device-Model page) --
 * it's the contract the Flutter app (#48) is built against.
 */
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::control::Control;
use crate::device::{self, Device};
use crate::network;
use crate::state::{DeviceId, Event};

/* The protocol version "hello" reports. v1 (before #34) had property bags
 * and LED-specific actions; clients check this to know what they talk to. */
const PROTOCOL_VERSION: u32 = 2;

/* What a client may ask. `#[serde(tag = "action", rename_all =
 * "snake_case")]`: the JSON's "action" field picks the variant, e.g.
 * {"action": "get_device", "id": "lamp-1"}. An unknown action or a missing
 * field is refused with serde's own (quite clear) message. */
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ClientRequest {
    Hello,
    ListDevices,
    GetDevice {
        id: DeviceId,
    },
    AddDevice {
        device: Device,
    },
    UpdateDeviceInfo {
        id: DeviceId,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        room: Option<String>,
    },
    RemoveDevice {
        id: DeviceId,
    },
    Command {
        id: DeviceId,
        capability: String,
        value: serde_json::Value,
    },
    Subscribe,
    /* Network (issue #61), answered by network.rs. Reading the status is
     * allowed for every client; everything else only for clients on the
     * hub itself -- see local_only below. */
    GetNetworkStatus,
    WifiScan,
    WifiConnect {
        ssid: String,
        /* Omitted or empty for an open network. */
        #[serde(default)]
        password: Option<String>,
    },
    WifiForget,
    SetWifiCountry {
        country: String,
    },
}

impl ClientRequest {
    /* Requests that change the hub's network (or list the neighbours'
     * networks) are only accepted from the hub itself, i.e. the
     * touchscreen UI, which connects over 127.0.0.1. Until LAN clients
     * authenticate (#35), anyone on the LAN can reach this port; without
     * this check, any device on the network could move the hub to another
     * WiFi. */
    fn local_only(&self) -> bool {
        matches!(
            self,
            ClientRequest::WifiScan
                | ClientRequest::WifiConnect { .. }
                | ClientRequest::WifiForget
                | ClientRequest::SetWifiCountry { .. }
        )
    }
}

/* What the server sends: replies and (after subscribe) events. */
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Hello {
        protocol: u32,
        /* The capability names this hub knows (device.rs). */
        capabilities: Vec<&'static str>,
    },
    Devices {
        devices: Vec<Device>,
    },
    Device {
        device: Option<Device>,
    },
    Ack,
    NetworkStatus {
        status: network::Status,
    },
    WifiNetworks {
        networks: Vec<network::Network>,
    },
    /* Events. */
    DeviceChanged {
        device: Device,
    },
    DeviceRemoved {
        id: DeviceId,
    },
    EventsLost,
    Error {
        message: String,
    },
}

/* Axum's `State` extractor accepts ONE state type per router -- this struct
 * bundles what every connection needs. Cloning is cheap (channel handles
 * and Arcs); Axum clones it for every connection. */
#[derive(Clone)]
struct AppState {
    control: Control,
    network_tx: mpsc::Sender<network::Cmd>,
    /* To subscribe a client to state.rs's device events. */
    events_tx: broadcast::Sender<Event>,
    /* How many clients are connected right now (issue #29) -- shared with
     * health.rs, which reports it to the cloud as "local_clients". */
    local_clients: Arc<AtomicUsize>,
}

/* Counts one connected client for as long as it exists: +1 when created,
 * -1 when dropped. Tying the -1 to `Drop` (Rust runs it automatically when
 * the value goes out of scope) means it happens however the connection
 * ends -- clean close, network error, or the task being cancelled -- with
 * no way to forget it on some early-return path. */
struct ClientCount(Arc<AtomicUsize>);

impl ClientCount {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        ClientCount(counter.clone())
    }
}

impl Drop for ClientCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/* Entry point, spawned once from main(). */
pub async fn run(
    control: Control,
    network_tx: mpsc::Sender<network::Cmd>,
    events_tx: broadcast::Sender<Event>,
    local_clients: Arc<AtomicUsize>,
) {
    let app = Router::new().route("/ws", get(ws_handler)).with_state(AppState {
        control,
        network_tx,
        events_tx,
        local_clients,
    });

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind WebSocket listener");
    println!(
        "ws.rs listening on {}",
        listener.local_addr().expect("listener has no local address")
    );
    /* `into_make_service_with_connect_info`: makes each connection's
     * address (who is connecting) available to ws_handler, for the
     * local-only check (issue #61). */
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("WebSocket server crashed");
}

/* Runs once per incoming HTTP request to /ws: completes the WebSocket
 * handshake and hands the connection to handle_socket. */
async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(app_state): State<AppState>,
) -> impl IntoResponse {
    /* A client on the hub itself: 127.0.0.1 or ::1 (to_canonical also
     * turns an IPv4 address written the IPv6 way back into IPv4). */
    let local = peer.ip().to_canonical().is_loopback();
    ws.on_upgrade(move |socket| handle_socket(socket, app_state, local))
}

/* One of these runs per connected client, for as long as it's connected.
 * It waits for whichever comes first: a request from the client, or (once
 * subscribed) a device event to pass on. */
async fn handle_socket(mut socket: WebSocket, app_state: AppState, local: bool) {
    /* `_count` only has to live until this function returns; its Drop
     * lowers the count again. (A plain `_` would drop it immediately.) */
    let _count = ClientCount::new(&app_state.local_clients);
    /* None until the client subscribes. */
    let mut events: Option<broadcast::Receiver<Event>> = None;

    loop {
        let outgoing = tokio::select! {
            incoming = socket.recv() => {
                /* None / Err: the client is gone. */
                let Some(Ok(msg)) = incoming else { break };
                let Message::Text(text) = msg else { continue };
                match serde_json::from_str::<ClientRequest>(&text) {
                    Ok(ClientRequest::Subscribe) => {
                        /* Events from now on; the client lists the devices
                         * once to know where to start. */
                        events = Some(app_state.events_tx.subscribe());
                        ServerMessage::Ack
                    }
                    Ok(req) if req.local_only() && !local => ServerMessage::Error {
                        message: "only allowed from the hub's own screen".into(),
                    },
                    Ok(req) => handle_request(req, &app_state).await,
                    Err(e) => ServerMessage::Error { message: e.to_string() },
                }
            }
            /* This branch only exists while subscribed (the `if`). */
            event = next_event(&mut events), if events.is_some() => event,
        };

        let payload = serde_json::to_string(&outgoing).expect("ServerMessage is always valid JSON");
        if socket.send(Message::Text(payload)).await.is_err() {
            /* The client disconnected mid-send: just stop serving it. */
            break;
        }
    }
}

/* The next device event for a subscribed client, as a message. The
 * broadcast channel keeps a limited backlog per subscriber (see main.rs);
 * a client too slow to keep up misses events ("Lagged") and is told so, so
 * it can list the devices again instead of showing stale ones. */
async fn next_event(events: &mut Option<broadcast::Receiver<Event>>) -> ServerMessage {
    let Some(rx) = events.as_mut() else {
        /* Not reachable: select! only polls this while subscribed. */
        return std::future::pending().await;
    };
    match rx.recv().await {
        Ok(Event::Changed(device)) => ServerMessage::DeviceChanged { device },
        Ok(Event::Removed(id)) => ServerMessage::DeviceRemoved { id },
        Err(broadcast::error::RecvError::Lagged(_)) => ServerMessage::EventsLost,
        Err(broadcast::error::RecvError::Closed) => {
            /* state.rs is gone (shutting down): no more events. */
            *events = None;
            ServerMessage::EventsLost
        }
    }
}

/* Carries out one request. Device requests go through control.rs;
 * network ones to network.rs. */
async fn handle_request(req: ClientRequest, app_state: &AppState) -> ServerMessage {
    let control = &app_state.control;
    let device = |result: Result<Device, String>| match result {
        Ok(device) => ServerMessage::Device { device: Some(device) },
        Err(message) => ServerMessage::Error { message },
    };
    match req {
        ClientRequest::Hello => ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: device::CAPABILITY_NAMES.to_vec(),
        },
        ClientRequest::ListDevices => match control.list().await {
            Ok(devices) => ServerMessage::Devices { devices },
            Err(message) => ServerMessage::Error { message },
        },
        ClientRequest::GetDevice { id } => match control.get(&id).await {
            Ok(device) => ServerMessage::Device { device },
            Err(message) => ServerMessage::Error { message },
        },
        ClientRequest::AddDevice { device: new } => device(control.add(new).await),
        ClientRequest::UpdateDeviceInfo { id, name, room } => device(control.update_info(&id, name, room).await),
        ClientRequest::RemoveDevice { id } => ack_or_error(control.remove(&id).await),
        ClientRequest::Command { id, capability, value } => device(control.command(&id, &capability, value).await),
        /* Handled in handle_socket (it changes the connection itself). */
        ClientRequest::Subscribe => ServerMessage::Ack,
        ClientRequest::GetNetworkStatus => {
            match ask_network(&app_state.network_tx, |reply| network::Cmd::GetStatus { reply }).await {
                Ok(status) => ServerMessage::NetworkStatus { status },
                Err(message) => ServerMessage::Error { message },
            }
        }
        ClientRequest::WifiScan => {
            match ask_network(&app_state.network_tx, |reply| network::Cmd::Scan { reply }).await {
                Ok(Ok(networks)) => ServerMessage::WifiNetworks { networks },
                Ok(Err(message)) | Err(message) => ServerMessage::Error { message },
            }
        }
        ClientRequest::WifiConnect { ssid, password } => {
            let answer = ask_network(&app_state.network_tx, |reply| network::Cmd::Connect {
                ssid,
                password,
                reply,
            })
            .await;
            ack_or_error(answer.and_then(|r| r))
        }
        ClientRequest::WifiForget => {
            let answer = ask_network(&app_state.network_tx, |reply| network::Cmd::Forget { reply }).await;
            ack_or_error(answer.and_then(|r| r))
        }
        ClientRequest::SetWifiCountry { country } => {
            let answer = ask_network(&app_state.network_tx, |reply| network::Cmd::SetCountry { country, reply }).await;
            ack_or_error(answer.and_then(|r| r))
        }
    }
}

/* One request/reply round trip with network.rs's actor: `make` builds the
 * command around the reply envelope created here. Err only if the actor
 * is gone (the daemon shutting down). */
async fn ask_network<T>(
    network_tx: &mpsc::Sender<network::Cmd>,
    make: impl FnOnce(oneshot::Sender<T>) -> network::Cmd,
) -> Result<T, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    network_tx
        .send(make(reply_tx))
        .await
        .map_err(|_| "network actor unavailable".to_string())?;
    reply_rx.await.map_err(|_| "network actor dropped the reply".to_string())
}

/* For requests that only succeed or fail. */
fn ack_or_error(answer: Result<(), String>) -> ServerMessage {
    match answer {
        Ok(()) => ServerMessage::Ack,
        Err(message) => ServerMessage::Error { message },
    }
}
