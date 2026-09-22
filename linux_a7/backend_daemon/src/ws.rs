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

use crate::state::{DeviceId, DeviceState, Msg};

/* What a connected client is allowed to ask for. This deliberately mirrors
 * state.rs's own Msg enum -- ws.rs doesn't invent its own device logic, it
 * just translates network JSON into the exact same messages state.rs
 * already understands. `#[serde(tag = "action", rename_all = "snake_case")]`
 * means the JSON looks like {"action": "get_all_devices"} or
 * {"action": "get_device", "id": "lamp-1"} -- the "action" field picks
 * which variant this is. */
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
    Error {
        message: String,
    },
}

/* Entry point, spawned once from main(). `state_tx` is a clone of the
 * Sender that talks to state.rs's actor -- every connected client's task
 * gets its own clone of this same handle, same pattern as everywhere else
 * in this project. */
pub async fn run(state_tx: mpsc::Sender<Msg>) {
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state_tx);

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
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state_tx): State<mpsc::Sender<Msg>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state_tx))
}

/* One of these runs per connected client, for as long as that client stays
 * connected -- this is the per-device-task pattern from earlier, just
 * per-connection instead of per-device. */
async fn handle_socket(mut socket: WebSocket, state_tx: mpsc::Sender<Msg>) {
    while let Some(Ok(msg)) = socket.recv().await {
        let Message::Text(text) = msg else {
            continue;
        };

        let response = match serde_json::from_str::<ClientRequest>(&text) {
            Ok(req) => handle_request(req, &state_tx).await,
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
 * state.rs's actor understands", using the exact request/reply pattern
 * covered when state.rs was built. */
async fn handle_request(req: ClientRequest, state_tx: &mpsc::Sender<Msg>) -> ServerResponse {
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
    }
}
