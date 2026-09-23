use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

use crate::rpmsg;
use crate::state::{DeviceId, DeviceState, Msg};

/* What a connected client is allowed to ask for. This deliberately mirrors
 * state.rs's own Msg enum -- ws.rs doesn't invent its own device logic, it
 * just translates network JSON into the exact same messages state.rs
 * already understands. `#[serde(tag = "action", rename_all = "snake_case")]`
 * means the JSON looks like {"action": "get_all_devices"} or
 * {"action": "get_device", "id": "lamp-1"} -- the "action" field picks
 * which variant this is.
 *
 * `SetLed`/`GetLedState` are new for issue #14 -- unlike the four variants
 * above, these don't go to state.rs at all (there's no "m4-led" entry in its
 * HashMap); they go straight to rpmsg.rs's actor instead, since this is a
 * real hardware command/reply round trip, not a stored property. See
 * rpmsg.rs's own doc comment for why that split exists. */
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ClientRequest {
    GetDevice { id: DeviceId },
    GetAllDevices,
    UpdateDevice {
        id: DeviceId,
        properties: DeviceState,
    },
    RemoveDevice { id: DeviceId },
    SetLed { on: bool },
    GetLedState,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerResponse {
    Device {
        id: DeviceId,
        state: Option<DeviceState>,
    },
    AllDevices {
        devices: HashMap<DeviceId, DeviceState>,
    },
    Ack,
    /// Sent in reply to both `SetLed` and `GetLedState` -- `on` is the LED's
    /// actual resulting state as reported by the M4, not just an echo of
    /// whatever the client asked for (see rpmsg.rs's `round_trip` for why
    /// that distinction matters: the M4 might refuse or be unreachable).
    LedState {
        on: bool,
    },
    Error {
        message: String,
    },
}

/* Axum's `State` extractor (used in `ws_handler` below) only accepts ONE
 * state type per router -- this struct just bundles the two mailbox
 * `Sender`s this module needs to hand to every connection into that one
 * type. `#[derive(Clone)]` matters here: Axum clones the state for every
 * incoming connection, and cloning a struct of two `Sender`s is exactly as
 * cheap as cloning either one alone (see main.rs's comment on what cloning a
 * `Sender` actually costs -- a permission slip, not the mailbox itself). */
#[derive(Clone)]
struct AppState {
    state_tx: mpsc::Sender<Msg>,
    rpmsg_tx: mpsc::Sender<rpmsg::Cmd>,
}

/* Entry point, spawned once from main(). Both `Sender`s are moved in once
 * here; every connected client's task gets its own clone of each, same
 * pattern as everywhere else in this project. */
pub async fn run(state_tx: mpsc::Sender<Msg>, rpmsg_tx: mpsc::Sender<rpmsg::Cmd>) {
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(AppState { state_tx, rpmsg_tx });

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind WebSocket listener");
    println!(
        "ws.rs listening on {}",
        listener.local_addr().expect("listener has no local address")
    );
    axum::serve(listener, app)
        .await
        .expect("WebSocket server crashed");
}

/* This runs once per incoming HTTP request to /ws. `WebSocketUpgrade` is
 * Axum's way of handling the special HTTP handshake a WebSocket connection
 * starts as; `.on_upgrade(...)` is what actually completes that handshake
 * and hands us a real, bidirectional `WebSocket` to talk over -- from that
 * point on it behaves like the socket types discussed earlier in this
 * project, not like a normal one-shot HTTP request/response. */
async fn ws_handler(ws: WebSocketUpgrade, State(app_state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, app_state))
}

/* One of these runs per connected client, for as long as that client stays
 * connected -- this is the per-device-task pattern from earlier, just
 * per-connection instead of per-device. */
async fn handle_socket(mut socket: WebSocket, app_state: AppState) {
    while let Some(Ok(msg)) = socket.recv().await {
        let Message::Text(text) = msg else {
            continue;
        };

        let response = match serde_json::from_str::<ClientRequest>(&text) {
            Ok(req) => handle_request(req, &app_state).await,
            Err(e) => ServerResponse::Error {
                message: e.to_string(),
            },
        };

        let payload = serde_json::to_string(&response).expect("ServerResponse is always valid JSON");
        if socket.send(Message::Text(payload)).await.is_err() {
            // Client disconnected mid-send -- not an error worth logging,
            // just stop serving this connection. The loop condition above
            // handles the more common "client closed cleanly" case too;
            // either way this task ends here and is dropped, no explicit
            // cleanup needed.
            break;
        }
    }
}

/* The actual translation from "what the client asked for" to "a message
 * state.rs's (or, for the two LED variants, rpmsg.rs's) actor understands",
 * using the exact request/reply pattern covered when state.rs was built. */
async fn handle_request(req: ClientRequest, app_state: &AppState) -> ServerResponse {
    let state_tx = &app_state.state_tx;
    let rpmsg_tx = &app_state.rpmsg_tx;

    match req {
        ClientRequest::GetDevice { id } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if state_tx
                .send(Msg::GetDevice {
                    id: id.clone(),
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                return ServerResponse::Error {
                    message: "state actor unavailable".into(),
                };
            }
            let state = reply_rx.await.unwrap_or(None);
            ServerResponse::Device { id, state }
        }
        ClientRequest::GetAllDevices => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if state_tx
                .send(Msg::GetAllDevices { reply: reply_tx })
                .await
                .is_err()
            {
                return ServerResponse::Error {
                    message: "state actor unavailable".into(),
                };
            }
            let devices = reply_rx.await.unwrap_or_default();
            ServerResponse::AllDevices { devices }
        }
        ClientRequest::UpdateDevice { id, properties } => {
            if state_tx
                .send(Msg::UpdateDevice { id, properties })
                .await
                .is_err()
            {
                return ServerResponse::Error {
                    message: "state actor unavailable".into(),
                };
            }
            ServerResponse::Ack
        }
        ClientRequest::RemoveDevice { id } => {
            if state_tx.send(Msg::RemoveDevice { id }).await.is_err() {
                return ServerResponse::Error {
                    message: "state actor unavailable".into(),
                };
            }
            ServerResponse::Ack
        }
        /* Both LED variants go through this one helper -- `LedRequest` is
         * just "which of the two rpmsg::Cmd variants to build," since the
         * real oneshot reply channel has to be created fresh inside
         * `send_led_cmd` right before sending (a `oneshot` channel is
         * single-use, so there's no reusable one to pass in from here). */
        ClientRequest::SetLed { on } => send_led_cmd(rpmsg_tx, LedRequest::Set(on)).await,
        ClientRequest::GetLedState => send_led_cmd(rpmsg_tx, LedRequest::Get).await,
    }
}

/// Which `rpmsg::Cmd` to build inside `send_led_cmd` -- exists only because
/// `rpmsg::Cmd`'s real variants each carry a `oneshot::Sender`, which can't
/// be constructed until `send_led_cmd` is already holding the receiving
/// half, so this can't just be `rpmsg::Cmd` itself with the reply field left
/// out.
enum LedRequest {
    Set(bool),
    Get,
}

/* Sends the requested LED command and turns its `Result<bool, String>`
 * reply into a `ServerResponse` -- factored out since `SetLed` and
 * `GetLedState` both need this exact same "create a reply channel, send,
 * await, translate" sequence, just for a different `rpmsg::Cmd` variant. */
async fn send_led_cmd(rpmsg_tx: &mpsc::Sender<rpmsg::Cmd>, req: LedRequest) -> ServerResponse {
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = match req {
        LedRequest::Set(on) => rpmsg::Cmd::SetLed { on, reply: reply_tx },
        LedRequest::Get => rpmsg::Cmd::GetLedState { reply: reply_tx },
    };

    if rpmsg_tx.send(cmd).await.is_err() {
        return ServerResponse::Error {
            message: "rpmsg actor unavailable".into(),
        };
    }

    match reply_rx.await {
        Ok(Ok(on)) => ServerResponse::LedState { on },
        Ok(Err(message)) => ServerResponse::Error { message },
        Err(_) => ServerResponse::Error {
            message: "rpmsg actor dropped the reply channel".into(),
        },
    }
}
