// ws_client.rs -- talks to backend_daemon over the same local WebSocket
// path any other client (a phone, a browser) would use, per
// ARCHITECTURE.md's "Local IPC" design: ui_layer is just an ordinary
// ws://127.0.0.1:8080/ws client, not a special-cased one with its own
// protocol. (Being on 127.0.0.1 does matter for one thing: backend_daemon
// only accepts WiFi changes from the hub itself, see its ws.rs.)
//
// Protocol v2 (issue #34, see backend_daemon's ws.rs): after connecting,
// this client subscribes to device events and lists the devices once;
// from then on the backend PUSHES every change. Nothing is polled except
// the network status (cable in/out isn't a device event).
//
// This crate has no async runtime (see Cargo.toml's comment on why
// `tungstenite`, not `tokio-tungstenite`) -- the GUI's own event loop in
// main.rs is a plain blocking loop. So this module runs the WebSocket
// connection on its own background `std::thread`, and hands data back and
// forth across two plain `std::sync::mpsc` channels -- the same "actor
// talks only through a channel" idea used throughout backend_daemon.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tungstenite::Message;

/// What this UI asks backend_daemon for. Mirrors (a part of) ws.rs's
/// `ClientRequest` -- MUST stay in sync with it by hand: two separate
/// crates/processes, no shared type definition (they talk over the
/// network). main.rs sends these into the channel `start()` returns.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    Subscribe,
    ListDevices,
    /// Change one capability of one device, e.g. capability "switch",
    /// value {"on": true}.
    Command {
        id: String,
        capability: String,
        value: serde_json::Value,
    },
    // Network (issue #61).
    GetNetworkStatus,
    WifiScan,
    WifiConnect { ssid: String, password: String },
    WifiForget,
    SetWifiCountry { country: String },
    // Pairing (issue #35; only accepted from the hub itself).
    StartPairing,
    PairingStatus,
    CancelPairing,
    ListClients,
    RevokeClient { id: String },
    // The setup hotspot (issue #36; only accepted from the hub itself).
    StartHotspot,
    StopHotspot,
    // The hub's settings (issue #39): design preset + time zone. Only the
    // fields given are changed.
    GetSettings,
    SetSettings {
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        accent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        density: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        time_zone: Option<String>,
    },
}

/// The hub's settings (issue #39), as backend_daemon's settings.rs sends
/// them. `default`: a field an older backend doesn't send yet takes its
/// default instead of breaking the message.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct HubSettings {
    pub mode: String,
    pub accent: String,
    pub density: String,
    pub time_zone: String,
}

impl Default for HubSettings {
    fn default() -> Self {
        HubSettings {
            mode: "dark".into(),
            accent: "sky".into(),
            density: "comfortable".into(),
            time_zone: "UTC".into(),
        }
    }
}

/// A device as backend_daemon describes it (its device.rs). Only what this
/// UI shows; serde ignores the rest. Capabilities this UI doesn't know yet
/// (added to the backend later) are ignored too, so an older UI never
/// breaks on a newer backend.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub room: String,
    pub capabilities: Capabilities,
}

#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Capabilities {
    pub switch: Option<Switch>,
    pub dimmer: Option<Dimmer>,
    pub color: Option<Color>,
    pub sensor: Option<Sensor>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Switch {
    pub on: bool,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Dimmer {
    pub level: u8,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Color {
    pub hex: Option<String>,
    pub kelvin: Option<u16>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Sensor {
    pub readings: BTreeMap<String, Reading>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Reading {
    pub value: f64,
    #[serde(default)]
    pub unit: String,
}

/// The network status as backend_daemon's network.rs reports it (its
/// `Status`). Only the fields this UI shows; serde ignores the others.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct NetworkStatus {
    /// "ethernet", "wifi", or absent (offline).
    pub uplink: Option<String>,
    pub ethernet: EthernetStatus,
    pub wifi: WifiStatus,
    /// Not part of network.rs's status: ws.rs sends it next to it, in the
    /// same message (see ServerMessage::NetworkStatus); filled in there.
    #[serde(skip)]
    pub hotspot: Hotspot,
}

/// The setup hotspot (backend_daemon's hotspot.rs, issue #36).
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Hotspot {
    pub active: bool,
    pub ssid: String,
    /// Only sent to the hub's own screen.
    pub password: Option<String>,
    pub url: String,
    pub last_error: Option<String>,
    pub connecting: bool,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct EthernetStatus {
    pub connected: bool,
    pub ip: Option<String>,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct WifiStatus {
    pub available: bool,
    pub connected: bool,
    pub ssid: Option<String>,
    pub bars: u8,
    pub ip: Option<String>,
    pub saved_ssid: Option<String>,
    pub country: Option<String>,
}

/// The pairing screen's data (backend_daemon's auth.rs + ws.rs).
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Pairing {
    /// "none", "waiting", "paired", "locked" or "expired".
    pub state: String,
    pub code: Option<String>,
    pub seconds_left: u64,
    pub client_name: Option<String>,
    pub addresses: Vec<String>,
    pub port: u16,
    pub fingerprint: String,
    pub fingerprint_short: String,
}

/// A paired client (auth.rs's `ClientInfo`).
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Client {
    pub id: String,
    pub name: String,
    pub created: u64,
    pub last_seen: u64,
}

/// One network from a scan (network.rs's `Network`).
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub bars: u8,
    pub secure: bool,
    pub saved: bool,
}

/// Everything backend_daemon can send that this UI understands: replies
/// and events. `#[serde(other)]` on `Unknown`: any other shape parses as
/// `Unknown` instead of failing.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Devices { devices: Vec<Device> },
    Device {},
    NetworkStatus {
        status: NetworkStatus,
        #[serde(default)]
        hotspot: Hotspot,
    },
    /// The reply to StartHotspot/StopHotspot. Its content isn't needed: the
    /// network status asked for right afterwards carries the same.
    Hotspot {},
    WifiNetworks { networks: Vec<WifiNetwork> },
    Pairing(Pairing),
    Clients { clients: Vec<Client> },
    Settings(HubSettings),
    Ack,
    Error { message: String },
    // Events (pushed after Subscribe).
    DeviceChanged { device: Device },
    DeviceRemoved { id: String },
    EventsLost,
    SettingsChanged(HubSettings),
    #[serde(other)]
    Unknown,
}

/// Which WiFi action an `ActionDone` update is about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Connect,
    Forget,
    Country,
    Revoke,
    Hotspot,
}

/// What this module reports back to the GUI thread. main.rs drains these
/// each frame -- the *only* way the GUI's displayed state ever changes,
/// matching ui_layer's "just displays whatever the daemon says" design.
pub enum Update {
    Connected,
    Disconnected,
    /// The complete device list (after connecting, or after missed events).
    Devices(Vec<Device>),
    DeviceChanged(Device),
    DeviceRemoved(String),
    /// A command the user gave was refused (e.g. the M4 isn't answering).
    CommandFailed(String),
    Network(NetworkStatus),
    Networks(Vec<WifiNetwork>),
    ScanFailed(String),
    ActionDone(Action, Result<(), String>),
    Pairing(Pairing),
    Clients(Vec<Client>),
    /// The hub's settings: after connecting, and whenever they change --
    /// from this screen or from a phone (issue #39).
    Settings(HubSettings),
}

const BACKEND_URL: &str = "ws://127.0.0.1:8080/ws";
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How often to re-read the network status (cable in/out, WiFi signal).
const NETWORK_REFRESH: Duration = Duration::from_secs(2);
/// How long one read waits for a message before the loop checks for
/// requests from the GUI again. Short enough that a tap goes out at once.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// Starts the background connection thread and returns the two channel ends
/// main.rs needs: `request_tx` to send a `Request`, and `update_rx` to
/// receive `Update`s back (poll it, non-blockingly, once per GUI frame).
pub fn start() -> (mpsc::Sender<Request>, mpsc::Receiver<Update>) {
    let (request_tx, request_rx) = mpsc::channel::<Request>();
    let (update_tx, update_rx) = mpsc::channel::<Update>();

    std::thread::spawn(move || connection_loop(request_rx, update_tx));

    (request_tx, update_rx)
}

/// Runs forever on the background thread: connect (retrying every
/// RETRY_DELAY), subscribe and list the devices, then serve until the
/// connection breaks -- announce `Disconnected` and start over. Keeps all
/// reconnection logic in one place.
fn connection_loop(request_rx: mpsc::Receiver<Request>, update_tx: mpsc::Sender<Update>) {
    loop {
        let mut socket = match tungstenite::connect(BACKEND_URL) {
            Ok((socket, _http_response)) => socket,
            Err(e) => {
                println!("ui_layer: failed to connect to backend_daemon: {e}");
                std::thread::sleep(RETRY_DELAY);
                continue;
            }
        };
        // Reads give up after READ_TIMEOUT (see serve). `tungstenite`'s
        // stream type can wrap TLS too; ours is always the plain TCP one.
        if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = socket.get_mut() {
            let _ = tcp.set_read_timeout(Some(READ_TIMEOUT));
        }

        // `.send() == Err`: the GUI thread has exited; so should we.
        if update_tx.send(Update::Connected).is_err() {
            return;
        }
        if !serve(&mut socket, &request_rx, &update_tx) {
            return;
        }
        let _ = update_tx.send(Update::Disconnected);
        std::thread::sleep(RETRY_DELAY);
    }
}

/// One connection's life. Returns false when the GUI thread is gone (stop
/// for good), true when the connection broke (reconnect).
///
/// Requests go out as soon as they're there -- several can be on their
/// way at once -- and the backend answers them strictly in order, so
/// `in_flight` (oldest first) says which request each reply belongs to.
/// Events can arrive at any moment in between; they're told apart by type.
fn serve(socket: &mut Socket, request_rx: &mpsc::Receiver<Request>, update_tx: &mpsc::Sender<Update>) -> bool {
    let mut in_flight: VecDeque<Request> = VecDeque::new();
    // Subscribe first, THEN ask: nothing that changes in between is lost.
    // The settings first (issue #39): the chosen look should replace the
    // default one before the devices appear.
    let mut outbox: VecDeque<Request> =
        VecDeque::from([Request::Subscribe, Request::GetSettings, Request::ListDevices]);
    let mut next_network = Instant::now();

    loop {
        // 1. Everything the GUI asked for since the last pass.
        loop {
            match request_rx.try_recv() {
                Ok(request) => outbox.push_back(request),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
        if Instant::now() >= next_network {
            next_network = Instant::now() + NETWORK_REFRESH;
            outbox.push_back(Request::GetNetworkStatus);
        }

        // 2. Send it all.
        while let Some(request) = outbox.pop_front() {
            let payload = serde_json::to_string(&request).expect("Request is always valid JSON");
            if let Err(e) = socket.send(Message::Text(payload)) {
                println!("ui_layer: send failed: {e}");
                return true;
            }
            in_flight.push_back(request);
        }

        // 3. Whatever arrives within READ_TIMEOUT.
        let text = match socket.read() {
            Ok(Message::Text(text)) => text,
            // Ping/Pong frames keep the connection alive (tungstenite
            // answers them itself); nothing for us.
            Ok(_) => continue,
            // The read timeout: nothing arrived, which is normal.
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                continue
            }
            Err(e) => {
                println!("ui_layer: read failed: {e}");
                return true;
            }
        };
        let message = match serde_json::from_str::<ServerMessage>(&text) {
            Ok(message) => message,
            Err(e) => {
                println!("ui_layer: malformed message from backend_daemon: {e}");
                return true;
            }
        };
        let update = match message {
            // Events aren't replies: nothing in flight is answered by them.
            ServerMessage::DeviceChanged { device } => Some(Update::DeviceChanged(device)),
            ServerMessage::DeviceRemoved { id } => Some(Update::DeviceRemoved(id)),
            ServerMessage::SettingsChanged(settings) => Some(Update::Settings(settings)),
            ServerMessage::EventsLost => {
                // Missed some changes: start over from the full list.
                outbox.push_back(Request::ListDevices);
                None
            }
            reply => {
                let Some(request) = in_flight.pop_front() else {
                    println!("ui_layer: reply without a request, ignored");
                    continue;
                };
                to_update(&request, reply)
            }
        };
        if let Some(update) = update {
            // After a WiFi change, show its effect right away.
            if matches!(update, Update::ActionDone(_, Ok(()))) {
                next_network = Instant::now();
            }
            if update_tx.send(update).is_err() {
                return false;
            }
        }
    }
}

/// Turns the reply to `request` into what the GUI should hear about, if
/// anything. What a reply means depends on what was asked: an `Ack` or an
/// `Error` says nothing by itself.
fn to_update(request: &Request, reply: ServerMessage) -> Option<Update> {
    let action = match request {
        Request::WifiConnect { .. } => Some(Action::Connect),
        Request::WifiForget => Some(Action::Forget),
        Request::SetWifiCountry { .. } => Some(Action::Country),
        Request::RevokeClient { .. } => Some(Action::Revoke),
        Request::StartHotspot | Request::StopHotspot => Some(Action::Hotspot),
        _ => None,
    };
    match (request, reply) {
        (_, ServerMessage::Devices { devices }) => Some(Update::Devices(devices)),
        (_, ServerMessage::NetworkStatus { mut status, hotspot }) => {
            status.hotspot = hotspot;
            Some(Update::Network(status))
        }
        // Started/stopped: the next network status (asked for right away,
        // see ActionDone in serve) shows it.
        (_, ServerMessage::Hotspot {}) => action.map(|a| Update::ActionDone(a, Ok(()))),
        (_, ServerMessage::WifiNetworks { networks }) => Some(Update::Networks(networks)),
        (_, ServerMessage::Pairing(pairing)) => Some(Update::Pairing(pairing)),
        (_, ServerMessage::Clients { clients }) => Some(Update::Clients(clients)),
        (_, ServerMessage::Settings(settings)) => Some(Update::Settings(settings)),
        (Request::WifiScan, ServerMessage::Error { message }) => Some(Update::ScanFailed(message)),
        (Request::Command { id, .. }, ServerMessage::Error { message }) => {
            println!("ui_layer: command for {id} refused: {message}");
            Some(Update::CommandFailed(message))
        }
        // A command's result arrives as a DeviceChanged event (if anything
        // changed); the reply itself adds nothing.
        (Request::Command { .. }, _) => None,
        (_, ServerMessage::Ack) => action.map(|a| Update::ActionDone(a, Ok(()))),
        (_, ServerMessage::Error { message }) => match action {
            Some(a) => Some(Update::ActionDone(a, Err(message))),
            None => {
                println!("ui_layer: backend_daemon returned an error: {message}");
                None
            }
        },
        _ => None,
    }
}
