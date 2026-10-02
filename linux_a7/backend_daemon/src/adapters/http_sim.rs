/*
 * http_sim.rs -- simulated HTTP devices for the generic HTTP adapter's
 * tests (issue #75). Test builds only.
 *
 * One HTTP server on 127.0.0.1 (a free port) playing two devices:
 *   - a Shelly Plug S Gen3: /shelly, /rpc/Switch.GetStatus,
 *     /rpc/Switch.Set (the parts templates/shelly-plug-gen3.json uses),
 *     answering like the real one;
 *   - a made-up DIY light with a POST API (/diy/state, /diy/set), for the
 *     template features the plug doesn't use (bodies, dimmer, colour).
 *
 * The test drives it: press_button() plays the plug's own button,
 * set_auth() a password set in the Shelly app, stop() "unplugged".
 */
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

pub struct Sim {
    addr: SocketAddr,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

struct Shared {
    shelly_on: AtomicBool,
    auth: AtomicBool,
    diy: Mutex<Value>,
}

impl Sim {
    /* The plug starts off; the light on, 50 %, white. */
    pub async fn start() -> Sim {
        let shared = Arc::new(Shared {
            shelly_on: AtomicBool::new(false),
            auth: AtomicBool::new(false),
            diy: Mutex::new(json!({"power": "ON", "light": {"bri": 50, "rgb": "ffffff"}})),
        });
        let router = Router::new()
            .route("/shelly", get(shelly_info))
            .route("/rpc/Switch.GetStatus", get(shelly_status))
            .route("/rpc/Switch.Set", get(shelly_set))
            .route("/diy/state", get(diy_state))
            .route("/diy/set", post(diy_set))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Sim { addr, shared, server }
    }

    pub fn host(&self) -> String {
        self.addr.to_string()
    }

    pub fn shelly_on(&self) -> bool {
        self.shared.shelly_on.load(Ordering::Relaxed)
    }

    pub fn press_button(&self) {
        self.shared.shelly_on.fetch_xor(true, Ordering::Relaxed);
    }

    pub fn set_auth(&self, on: bool) {
        self.shared.auth.store(on, Ordering::Relaxed);
    }

    pub fn diy(&self) -> Value {
        self.shared.diy.lock().unwrap().clone()
    }

    pub fn stop(&self) {
        self.server.abort();
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn shelly_info(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(json!({
        "name": null, "id": "shellyplugsg3-8cbfea9a1b2c", "mac": "8CBFEA9A1B2C", "slot": 0,
        "model": "S3PL-00112EU", "gen": 3, "fw_id": "20241011-114455/1.4.4-g6d2a586", "ver": "1.4.4",
        "app": "PlugSG3", "auth_en": shared.auth.load(Ordering::Relaxed), "auth_domain": null
    }))
}

/* As the real plug answers; draws 40 W while on. */
async fn shelly_status(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let on = shared.shelly_on.load(Ordering::Relaxed);
    let power = if on { 40.0 } else { 0.0 };
    Json(json!({
        "id": 0, "source": "HTTP_in", "output": on, "apower": power, "voltage": 231.4, "freq": 50.0,
        "current": power / 231.4, "aenergy": {"total": 1234.567, "by_minute": [0.0, 0.0, 0.0], "minute_ts": 1759400000},
        "temperature": {"tC": 31.2, "tF": 88.2}
    }))
}

/* Shelly answers a Switch.Set with the state before it. */
async fn shelly_set(State(shared): State<Arc<Shared>>, Query(query): Query<HashMap<String, String>>) -> Json<Value> {
    let on = query.get("on").map(|v| v == "true").unwrap_or(false);
    let was = shared.shelly_on.swap(on, Ordering::Relaxed);
    Json(json!({ "was_on": was }))
}

async fn diy_state(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(shared.diy.lock().unwrap().clone())
}

/* Merges the posted fields into the state; answers plain text, as small
 * firmwares do (the adapter mustn't need JSON there). */
async fn diy_set(State(shared): State<Arc<Shared>>, Json(patch): Json<Value>) -> &'static str {
    let mut state = shared.diy.lock().unwrap();
    if let Some(power) = patch.get("power") {
        state["power"] = power.clone();
    }
    if let Some(light) = patch.get("light").and_then(Value::as_object) {
        for (k, v) in light {
            state["light"][k] = v.clone();
        }
    }
    "OK"
}
