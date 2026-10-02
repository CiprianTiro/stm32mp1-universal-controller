/*
 * wiz_sim.rs -- a simulated WiZ light, for the adapter's tests (issue
 * #75). Test builds only.
 *
 * A UDP socket on 127.0.0.1 (a free port) answering getPilot, setPilot
 * and getSystemConfig the way a real bulb does -- what it understands of
 * setPilot is what the adapter sends: state, dimming, r/g/b, temp.
 *
 * The test drives it: change_from_outside() plays "someone used the WiZ
 * app", lose_every_other() a bad WiFi, stop() "switched off at the wall".
 */
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

pub struct Sim {
    addr: SocketAddr,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

struct Shared {
    pilot: Mutex<Value>,
    lossy: AtomicBool,
    received: AtomicU64,
}

impl Sim {
    /* A running bulb: on, 80 %, warm white (2700 K). */
    pub async fn start() -> Sim {
        let shared = Arc::new(Shared {
            pilot: Mutex::new(json!({
                "mac": "a8bb50aabbcc", "rssi": -55, "src": "", "state": true,
                "sceneId": 0, "temp": 2700, "dimming": 80
            })),
            lossy: AtomicBool::new(false),
            received: AtomicU64::new(0),
        });
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let server = tokio::spawn(serve(socket, shared.clone()));
        Sim { addr, shared, server }
    }

    /* What a device's config "host" would hold. */
    pub fn host(&self) -> String {
        self.addr.to_string()
    }

    pub fn pilot(&self) -> Value {
        self.shared.pilot.lock().unwrap().clone()
    }

    pub fn change_from_outside(&self, params: Value) {
        apply(&mut self.shared.pilot.lock().unwrap(), &params);
    }

    /* From now on, every other datagram received is dropped unanswered. */
    pub fn lose_every_other(&self) {
        self.shared.lossy.store(true, Ordering::Relaxed);
    }

    /* Switched off: no more answers. */
    pub fn stop(&self) {
        self.server.abort();
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn serve(socket: UdpSocket, shared: Arc<Shared>) {
    let mut buf = [0u8; 2048];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buf).await else { return };
        let n = shared.received.fetch_add(1, Ordering::Relaxed);
        if shared.lossy.load(Ordering::Relaxed) && n.is_multiple_of(2) {
            continue;
        }
        let Ok(request) = serde_json::from_slice::<Value>(&buf[..len]) else { continue };
        let method = request["method"].as_str().unwrap_or_default().to_string();
        let result = match method.as_str() {
            "getPilot" => shared.pilot.lock().unwrap().clone(),
            "setPilot" => {
                apply(&mut shared.pilot.lock().unwrap(), &request["params"]);
                json!({ "success": true })
            }
            "getSystemConfig" => json!({
                "mac": "a8bb50aabbcc", "homeId": 1234, "roomId": 1, "moduleName": "ESP01_SHRGB1C_31",
                "fwVersion": "1.26.0", "groupId": 0, "ping": 0
            }),
            _ => {
                let error = json!({"method": method, "env": "pro", "error": {"code": -32601, "message": "Method not found"}});
                let _ = socket.send_to(error.to_string().as_bytes(), from).await;
                continue;
            }
        };
        let answer = json!({ "method": method, "env": "pro", "result": result });
        let _ = socket.send_to(answer.to_string().as_bytes(), from).await;
    }
}

/* setPilot as a bulb does it: a colour replaces the white temperature and
 * the other way round; either one (or a dimming) also switches it on. */
fn apply(pilot: &mut Value, params: &Value) {
    let obj = pilot.as_object_mut().unwrap();
    if params.get("r").is_some() {
        obj.remove("temp");
        for c in ["r", "g", "b"] {
            obj.insert(c.into(), params[c].clone());
        }
        obj.insert("c".into(), json!(0));
        obj.insert("w".into(), json!(0));
        obj.insert("state".into(), json!(true));
    }
    if let Some(temp) = params.get("temp") {
        for c in ["r", "g", "b", "c", "w"] {
            obj.remove(c);
        }
        obj.insert("temp".into(), temp.clone());
        obj.insert("state".into(), json!(true));
    }
    if let Some(dimming) = params.get("dimming") {
        obj.insert("dimming".into(), dimming.clone());
        obj.insert("state".into(), json!(true));
    }
    if let Some(state) = params.get("state") {
        obj.insert("state".into(), state.clone());
    }
    obj.insert("sceneId".into(), json!(0));
}
