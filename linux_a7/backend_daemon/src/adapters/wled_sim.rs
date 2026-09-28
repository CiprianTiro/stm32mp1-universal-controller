/*
 * wled_sim.rs -- a simulated WLED, for the adapter's tests (issue #40:
 * "tests against simulated devices", the way Home Assistant tests its
 * integrations). Test builds only.
 *
 * A tiny HTTP server on 127.0.0.1 (a free port) with the parts of WLED's
 * API the adapter uses -- GET/POST /json/state, GET /json/info and the /ws
 * push channel -- so the whole adapter runs for real (HTTP, WebSocket,
 * reconnects) with no ESP on the desk. What it understands of a state
 * change is only what the adapter sends: on, bri, seg.col.
 *
 * The test drives it: change_from_outside() plays "someone used the WLED
 * app", stop() plays "unplugged".
 */
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

pub struct Sim {
    addr: SocketAddr,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

struct Shared {
    state: Mutex<Value>,
    /* Every state change, as WLED pushes it, to every WebSocket client. */
    pushes: broadcast::Sender<String>,
    /* true = unplugged: WebSocket clients are dropped. */
    stopped: watch::Sender<bool>,
    /* start_not_wled: /json/info names another brand. */
    not_wled: AtomicBool,
}

impl Sim {
    /* A running sim: on, 50 % (bri 128), orange. `with_ws` false: no /ws
     * (a WLED built without WebSockets -- the adapter must poll). */
    pub async fn start(with_ws: bool) -> Sim {
        let shared = Arc::new(Shared {
            state: Mutex::new(json!({
                "on": true, "bri": 128, "transition": 7, "mainseg": 0,
                "seg": [{"id": 0, "start": 0, "stop": 30, "fx": 0,
                         "col": [[255, 136, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]]}]
            })),
            pushes: broadcast::channel(16).0,
            stopped: watch::channel(false).0,
            not_wled: AtomicBool::new(false),
        });
        let mut router = Router::new()
            .route("/json/state", get(get_state).post(post_state))
            .route("/json/info", get(get_info));
        if with_ws {
            router = router.route("/ws", get(ws));
        }
        let router = router.with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Sim { addr, shared, server }
    }

    /* Something else answering HTTP: a JSON API, but not WLED's. */
    pub async fn start_not_wled() -> Sim {
        let sim = Sim::start(false).await;
        sim.shared.not_wled.store(true, Ordering::Relaxed);
        sim
    }

    /* What a device's config "host" would hold. */
    pub fn host(&self) -> String {
        self.addr.to_string()
    }

    pub fn state(&self) -> Value {
        self.shared.state.lock().unwrap().clone()
    }

    /* Someone changed the light with the WLED app or a button. */
    pub fn change_from_outside(&self, patch: Value) {
        self.shared.apply(&patch);
    }

    /* Unplugged: stops accepting connections, drops the WebSockets. */
    pub fn stop(&self) {
        self.server.abort();
        self.shared.stopped.send_replace(true);
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Shared {
    /* Applies a change the way WLED does (the parts the adapter uses),
     * then pushes the new state. */
    fn apply(&self, patch: &Value) {
        let full = {
            let mut state = self.state.lock().unwrap();
            if let Some(on) = patch["on"].as_bool() {
                state["on"] = json!(on);
            }
            match patch["bri"].as_u64() {
                /* WLED: brightness 0 = off, the last brightness is kept. */
                Some(0) => state["on"] = json!(false),
                Some(bri) => state["bri"] = json!(bri),
                None => {}
            }
            if let Some(rgb) = patch["seg"]["col"][0].as_array() {
                let mut rgbw = rgb.clone();
                rgbw.resize(4, json!(0));
                state["seg"][0]["col"][0] = Value::Array(rgbw);
            }
            json!({ "state": *state, "info": info() }).to_string()
        };
        let _ = self.pushes.send(full);
    }
}

fn info() -> Value {
    json!({"ver": "0.0.0-sim", "name": "WLED Sim", "brand": "WLED", "product": "FOSS",
           "mac": "aabbccddeeff", "leds": {"count": 30}})
}

async fn get_state(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(shared.state.lock().unwrap().clone())
}

async fn post_state(State(shared): State<Arc<Shared>>, Json(patch): Json<Value>) -> Json<Value> {
    shared.apply(&patch);
    if patch["v"] == true {
        Json(shared.state.lock().unwrap().clone())
    } else {
        Json(json!({"success": true}))
    }
}

async fn get_info(State(shared): State<Arc<Shared>>) -> Json<Value> {
    if shared.not_wled.load(Ordering::Relaxed) {
        return Json(json!({"brand": "Other", "name": "a printer"}));
    }
    Json(info())
}

async fn ws(State(shared): State<Arc<Shared>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| push_to(socket, shared))
}

/* Like WLED: the full state right after connecting, then every change. */
async fn push_to(mut socket: WebSocket, shared: Arc<Shared>) {
    let mut pushes = shared.pushes.subscribe();
    let mut stopped = shared.stopped.subscribe();
    let hello = json!({ "state": *shared.state.lock().unwrap(), "info": info() }).to_string();
    /* Late, like the real ESP's: a greeting read after a command must
     * not undo that command (wled.rs, Task::connect). */
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    if socket.send(Message::Text(hello)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            push = pushes.recv() => match push {
                Ok(text) => if socket.send(Message::Text(text)).await.is_err() { return },
                Err(_) => return,
            },
            /* axum answers pings itself; anything else from the client
             * is ignored, a close ends it. */
            incoming = socket.recv() => if !matches!(incoming, Some(Ok(_))) { return },
            /* The async block drops wait_for's guard (a lock, not Send)
             * before select! sees the result. */
            _ = async { let _ = stopped.wait_for(|stopped| *stopped).await; } => return,
        }
    }
}
