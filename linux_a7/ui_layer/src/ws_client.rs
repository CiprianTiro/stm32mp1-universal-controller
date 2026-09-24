// ws_client.rs -- talks to backend_daemon over the same local WebSocket
// path any other client (a phone, a browser) would use, per
// ARCHITECTURE.md's "Local IPC" design: ui_layer is just an ordinary
// ws://127.0.0.1:8080/ws client, not a special-cased one with its own
// protocol. (Being on 127.0.0.1 does matter for one thing: backend_daemon
// only accepts WiFi changes from the hub itself, see its ws.rs.)
//
// This crate has no async runtime (see Cargo.toml's comment on why
// `tungstenite`, not `tokio-tungstenite`) -- the GUI's own event loop in
// main.rs is a plain blocking loop that, once per iteration, checks the
// framebuffer, checks touch input, and checks for WebSocket updates, none
// of which are allowed to block each other for long. So this module runs
// the actual blocking WebSocket connection on its own background
// `std::thread`, and hands data back and forth across two plain
// `std::sync::mpsc` channels instead -- the same "actor talks only through
// a channel" idea used throughout backend_daemon (see its state.rs), just
// with `std::thread`/`std::sync::mpsc` here instead of `tokio::spawn`/
// `tokio::sync::mpsc`, since there's no Tokio runtime in this process to
// give it an async task to run on.

use serde::{Deserialize, Serialize};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tungstenite::Message;

/// What this UI ever asks backend_daemon for. Mirrors (a part of) ws.rs's
/// `ClientRequest` enum on the backend_daemon side -- MUST stay in sync
/// with it by hand, since these are two separate Cargo crates/processes
/// with no shared type definition between them (see ARCHITECTURE.md: they
/// talk over the network, not a shared library). main.rs sends these into
/// the channel `start()` returns.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    SetLed { on: bool },
    GetLedState,
    // Network (issue #61).
    GetNetworkStatus,
    WifiScan,
    WifiConnect { ssid: String, password: String },
    WifiForget,
    SetWifiCountry { country: String },
}

/// The network status as backend_daemon's network.rs reports it (its
/// `Status`). Only the fields this UI shows; serde ignores the others.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct NetworkStatus {
    /// "ethernet", "wifi", or absent (offline).
    pub uplink: Option<String>,
    pub ethernet: EthernetStatus,
    pub wifi: WifiStatus,
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

/// One network from a scan (network.rs's `Network`).
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub bars: u8,
    pub secure: bool,
    pub saved: bool,
}

/// What backend_daemon can send back, restricted to the shapes this UI
/// actually understands (again mirroring ws.rs's real `ServerResponse`).
/// `#[serde(other)]` on `Unknown` means "any response shape this enum
/// doesn't otherwise recognize gets parsed as `Unknown` instead of failing
/// to parse at all".
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    LedState { on: bool },
    NetworkStatus { status: NetworkStatus },
    WifiNetworks { networks: Vec<WifiNetwork> },
    Ack,
    Error { message: String },
    #[serde(other)]
    Unknown,
}

/// Which WiFi action an `ActionDone` update is about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Connect,
    Forget,
    Country,
}

/// What this module reports back to the GUI thread via the `mpsc::Receiver`
/// that `start()` returns. main.rs's event loop drains these each frame and
/// updates the `AppWindow`'s properties accordingly -- this is the *only*
/// way the GUI's displayed state ever changes, matching ui_layer's "just
/// displays whatever the daemon says" design.
pub enum Update {
    Connected,
    Disconnected,
    LedState(bool),
    Network(NetworkStatus),
    Networks(Vec<WifiNetwork>),
    ScanFailed(String),
    ActionDone(Action, Result<(), String>),
}

const BACKEND_URL: &str = "ws://127.0.0.1:8080/ws";
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How often to re-read the LED's state, to pick up changes made from
/// elsewhere (the cloud). Each read is one tiny local WebSocket message
/// plus one RPMsg round trip to the M4 -- negligible at once per second.
const LED_REFRESH: Duration = Duration::from_secs(1);
/// How often to re-read the network status (cable in/out, WiFi signal).
/// Reading it is a few small file reads and a couple of wpa_supplicant
/// queries on the backend side.
const NETWORK_REFRESH: Duration = Duration::from_secs(2);

/// Starts the background connection thread and returns the two channel ends
/// main.rs needs: `request_tx` to send a `Request` (a LED tap, a WiFi scan,
/// ...), and `update_rx` to receive `Update`s back (poll it, non-blockingly,
/// once per GUI frame).
pub fn start() -> (mpsc::Sender<Request>, mpsc::Receiver<Update>) {
    let (request_tx, request_rx) = mpsc::channel::<Request>();
    let (update_tx, update_rx) = mpsc::channel::<Update>();

    std::thread::spawn(move || connection_loop(request_rx, update_tx));

    (request_tx, update_rx)
}

/// Runs forever on the background thread started by `start()`. Structure:
/// connect (retrying with a fixed delay on failure), announce `Connected`,
/// then serve requests from the GUI thread -- and on its own, re-read the
/// LED and network status every LED_REFRESH / NETWORK_REFRESH -- until the
/// connection breaks, at which point announce `Disconnected` and go back to
/// the top to reconnect. This keeps reconnection logic in one place instead
/// of scattered through every place a send/receive could fail.
///
/// Requests are answered strictly one after another (the connection is a
/// simple request/reply conversation). A WiFi connect can take up to ~25 s
/// on the backend; the refreshes simply wait until it's done -- the UI
/// shows "Connecting..." meanwhile anyway.
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

        // `.send() == Err` here just means "the GUI thread already exited"
        // (its `Receiver` was dropped) -- nothing left to update, so this
        // whole background thread should just end too.
        if update_tx.send(Update::Connected).is_err() {
            return;
        }

        // Both start at "now", so the very first passes ask right after
        // connecting (the UI doesn't have to guess a default).
        let mut next_led = Instant::now();
        let mut next_network = Instant::now();
        loop {
            // The next thing to ask: a due refresh first, otherwise wait
            // for a request from the GUI (at most 200 ms, so refreshes
            // never run late).
            let now = Instant::now();
            let request = if now >= next_led {
                next_led = now + LED_REFRESH;
                Request::GetLedState
            } else if now >= next_network {
                next_network = now + NETWORK_REFRESH;
                Request::GetNetworkStatus
            } else {
                match request_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(request) => request,
                    // Nothing within the timeout -- completely normal, just
                    // loop again (and maybe refresh).
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return, // GUI thread exited
                }
            };

            let Some(response) = request_reply(&mut socket, &request) else {
                break; // connection lost: fall through to reconnect
            };
            let Some(update) = to_update(&request, response) else {
                continue;
            };
            // After a WiFi change, show its effect right away instead of
            // at the next scheduled refresh.
            if matches!(update, Update::ActionDone(_, Ok(()))) {
                next_network = Instant::now();
            }
            if update_tx.send(update).is_err() {
                return;
            }
        }

        let _ = update_tx.send(Update::Disconnected);
        std::thread::sleep(RETRY_DELAY);
    }
}

/// Turns the reply to `request` into what the GUI should hear about, if
/// anything. What a reply means depends on what was asked: an `Ack` or an
/// `Error` says nothing by itself.
fn to_update(request: &Request, response: Response) -> Option<Update> {
    let action = match request {
        Request::WifiConnect { .. } => Some(Action::Connect),
        Request::WifiForget => Some(Action::Forget),
        Request::SetWifiCountry { .. } => Some(Action::Country),
        _ => None,
    };
    match (request, response) {
        (_, Response::LedState { on }) => Some(Update::LedState(on)),
        (_, Response::NetworkStatus { status }) => Some(Update::Network(status)),
        (_, Response::WifiNetworks { networks }) => Some(Update::Networks(networks)),
        (Request::WifiScan, Response::Error { message }) => Some(Update::ScanFailed(message)),
        (_, Response::Ack) => action.map(|a| Update::ActionDone(a, Ok(()))),
        (_, Response::Error { message }) => match action {
            Some(a) => Some(Update::ActionDone(a, Err(message))),
            None => {
                // Only log errors for things the user did. The once-a-
                // second LED refresh would otherwise log the same "M4 not
                // loaded" line 86,400 times a day whenever the M4 firmware
                // isn't running.
                if matches!(request, Request::SetLed { .. }) {
                    println!("ui_layer: backend_daemon returned an error: {message}");
                }
                None
            }
        },
        (_, Response::Unknown) => None,
    }
}

/// Sends one request and waits for its reply. `None` means the connection
/// broke (or the reply was garbled) -- the caller reconnects.
fn request_reply(
    // `tungstenite::connect()` always returns this type, even for a plain
    // ws:// (non-TLS) connection like ours -- `MaybeTlsStream` is an enum
    // that's *capable* of wrapping either a plain or a TLS-wrapped
    // `TcpStream`, chosen based on the URL scheme at connect time; there's
    // no separate "definitely-plain" socket type to ask for instead.
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    request: &Request,
) -> Option<Response> {
    let payload = serde_json::to_string(request).expect("Request is always valid JSON");
    if let Err(e) = socket.send(Message::Text(payload)) {
        println!("ui_layer: send failed: {e}");
        return None;
    }

    loop {
        let message = match socket.read() {
            Ok(m) => m,
            Err(e) => {
                println!("ui_layer: read failed: {e}");
                return None;
            }
        };

        // WebSocket connections carry more than just our own text messages
        // -- Ping/Pong frames are the protocol keeping the connection
        // alive, handled transparently by `tungstenite` itself, but they
        // still show up here as a `Message` variant. Skip anything that
        // isn't the `Text` reply we're actually waiting for instead of
        // treating it as an error.
        let Message::Text(text) = message else {
            continue;
        };

        return match serde_json::from_str::<Response>(&text) {
            Ok(response) => Some(response),
            Err(e) => {
                println!("ui_layer: malformed response: {e}");
                None
            }
        };
    }
}
