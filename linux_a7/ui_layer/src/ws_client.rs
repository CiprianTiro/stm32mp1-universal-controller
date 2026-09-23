// ws_client.rs -- talks to backend_daemon over the same local WebSocket
// path any other client (a phone, a browser) would use, per
// ARCHITECTURE.md's "Local IPC" design: ui_layer is just an ordinary
// ws://127.0.0.1:8080/ws client, not a special-cased one with its own
// protocol.
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

/// What this UI ever asks backend_daemon for. Mirrors ws.rs's
/// `ClientRequest` enum on the backend_daemon side -- MUST stay in sync
/// with it by hand, since these are two separate Cargo crates/processes
/// with no shared type definition between them (see ARCHITECTURE.md: they
/// talk over the network, not a shared library). Only the two LED-related
/// variants are defined here since this UI only ever needs those, even
/// though ws.rs's real enum has more (GetDevice, UpdateDevice, ...).
#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    SetLed { on: bool },
    GetLedState,
}

/// What backend_daemon can send back, restricted to the shapes this UI
/// actually understands (again mirroring ws.rs's real `ServerResponse`,
/// which has more variants this UI never asks for and so never needs to
/// parse). `#[serde(other)]` on `Unknown` means "any response shape this
/// enum doesn't otherwise recognize gets parsed as `Unknown` instead of
/// failing to parse at all" -- since backend_daemon's protocol has response
/// types this UI intentionally ignores, and a parse *error* would be a
/// worse way to represent "not interesting to us" than a variant meaning
/// exactly that.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    LedState { on: bool },
    Error { message: String },
    #[serde(other)]
    Unknown,
}

/// What this module reports back to the GUI thread via the `mpsc::Receiver`
/// that `start()` returns. main.rs's event loop drains these each frame and
/// updates the `AppWindow`'s `led-on`/`connected` properties accordingly --
/// this is the *only* way the GUI's displayed state ever changes, matching
/// ui_layer's "just displays whatever the daemon says" design.
pub enum Update {
    Connected,
    Disconnected,
    LedState(bool),
}

const BACKEND_URL: &str = "ws://127.0.0.1:8080/ws";
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How often to re-read the LED's state, to pick up changes made from
/// elsewhere (the cloud). Each read is one tiny local WebSocket message
/// plus one RPMsg round trip to the M4 -- negligible at once per second.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// The outcome of one `request_reply()` call. Two different failures are
/// kept apart because they need different reactions: a broken connection
/// means reconnect, while an `Error` reply (e.g. the M4 firmware isn't
/// loaded) means the connection is fine and we should simply try again later.
enum Reply {
    Led(bool),
    BackendError,
    ConnectionLost,
}

/// Starts the background connection thread and returns the two channel ends
/// main.rs needs: `request_tx` to ask for a LED change (send into it from
/// the GUI thread whenever the button is tapped), and `update_rx` to
/// receive `Update`s back (poll it, non-blockingly, once per GUI frame).
pub fn start() -> (mpsc::Sender<bool>, mpsc::Receiver<Update>) {
    // `request_tx`/`request_rx`: the GUI thread sends `true`/`false` (the
    // desired LED state) into `request_tx` when the button is tapped; the
    // background thread's loop below reads them from `request_rx`.
    let (request_tx, request_rx) = mpsc::channel::<bool>();
    // `update_tx`/`update_rx`: the reverse direction, for connection status
    // and LED state changes flowing back to the GUI thread.
    let (update_tx, update_rx) = mpsc::channel::<Update>();

    std::thread::spawn(move || connection_loop(request_rx, update_tx));

    (request_tx, update_rx)
}

/// Runs forever on the background thread started by `start()`. Structure:
/// connect (retrying with a fixed delay on failure), announce `Connected`,
/// ask for the LED's current state right away so the UI doesn't have to
/// guess a default (and again every REFRESH_INTERVAL), then serve requests
/// from the GUI thread until the connection breaks -- at which point
/// announce `Disconnected` and go back to the top to reconnect. This keeps reconnection logic in one place
/// instead of scattered through every place a send/receive could fail.
fn connection_loop(request_rx: mpsc::Receiver<bool>, update_tx: mpsc::Sender<Update>) {
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

        // Serve requests until something goes wrong with the socket --
        // `Reply::ConnectionLost` covers "the connection broke while
        // answering" and `recv_timeout`'s `Disconnected` covers "the GUI
        // thread exited."
        //
        // `next_refresh` (issue #26): the LED can also be switched from the
        // cloud (MQTT), not only by tapping this screen -- so besides
        // serving taps, re-ask for the LED's state every REFRESH_INTERVAL
        // and show whatever the M4 says. Starts at "now", so the very first
        // pass through the loop asks right after connecting.
        let mut next_refresh = Instant::now();
        loop {
            // Ask for the current state if it's time to; otherwise wait for
            // a tap (at most 200 ms, so the refresh never runs late).
            let request = if Instant::now() >= next_refresh {
                next_refresh = Instant::now() + REFRESH_INTERVAL;
                None
            } else {
                match request_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(on) => Some(on),
                    // No tap within the timeout -- completely normal, just
                    // loop again (and maybe refresh).
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return, // GUI thread exited
                }
            };

            match request_reply(&mut socket, request) {
                Reply::Led(state) => {
                    if update_tx.send(Update::LedState(state)).is_err() {
                        return;
                    }
                }
                // backend_daemon is fine, it just couldn't reach the M4 --
                // already logged, keep the connection and try again later.
                Reply::BackendError => {}
                Reply::ConnectionLost => break, // fall through to reconnect
            }
        }

        let _ = update_tx.send(Update::Disconnected);
        std::thread::sleep(RETRY_DELAY);
    }
}

/// Sends one request and waits for its reply, returning the LED's resulting
/// state on success, `BackendError` if backend_daemon answered with an
/// `Error`, or `ConnectionLost` if the socket broke or the reply was
/// malformed (see `Reply` for why those two are kept apart). `on = None` means
/// "just ask for the current state" (`GetLedState`); `on = Some(x)` means
/// "ask to set it to `x`" (`SetLed`) -- both requests get exactly the same
/// kind of reply (`LedState { on }`), so one function handles both.
fn request_reply(
    // `tungstenite::connect()` always returns this type, even for a plain
    // ws:// (non-TLS) connection like ours -- `MaybeTlsStream` is an enum
    // that's *capable* of wrapping either a plain or a TLS-wrapped
    // `TcpStream`, chosen based on the URL scheme at connect time; there's
    // no separate "definitely-plain" socket type to ask for instead.
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    on: Option<bool>,
) -> Reply {
    let request = match on {
        Some(on) => Request::SetLed { on },
        None => Request::GetLedState,
    };

    let payload = serde_json::to_string(&request).expect("Request is always valid JSON");
    if let Err(e) = socket.send(Message::Text(payload)) {
        println!("ui_layer: send failed: {e}");
        return Reply::ConnectionLost;
    }

    loop {
        let message = match socket.read() {
            Ok(m) => m,
            Err(e) => {
                println!("ui_layer: read failed: {e}");
                return Reply::ConnectionLost;
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
            Ok(Response::LedState { on }) => Reply::Led(on),
            Ok(Response::Error { message }) => {
                // Only log errors for taps. The once-a-second refresh would
                // otherwise log the same "M4 not loaded" line 86,400
                // times a day whenever the M4 firmware isn't running.
                if on.is_some() {
                    println!("ui_layer: backend_daemon returned an error: {message}");
                }
                Reply::BackendError
            }
            Ok(Response::Unknown) => continue, // not the reply we asked for, keep waiting
            Err(e) => {
                println!("ui_layer: malformed response: {e}");
                Reply::ConnectionLost
            }
        };
    }
}
