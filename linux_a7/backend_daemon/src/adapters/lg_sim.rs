/*
 * lg_sim.rs -- a simulated LG webOS TV, for the adapter's tests (issue
 * #40). Test builds only.
 *
 * Like a real one: TLS with a SELF-SIGNED certificate (made fresh by
 * rcgen for each sim, so pinning is tested for real), SSAP over a
 * WebSocket, the pairing PROMPT (answered at once -- or declined, like a
 * person pressing "no"), subscriptions pushed on every change, and
 * turnOff closing the connection. What it understands is only what the
 * adapter sends.
 *
 * Issue #44: the "pointer input socket" (buttons arrive as
 * "type:button\nname:UP" on a second WebSocket -- any path on the same
 * port here), the on-screen keyboard (ime), apps and channels. Pressed
 * buttons and typed text are recorded for the tests.
 *
 * The test drives it: set_volume_from_remote() plays the TV's own remote;
 * after turnOff it refuses connections until wake_in() (a real TV needs
 * Wake-on-LAN for that; the sim can't receive the magic packet).
 */
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_tungstenite::tungstenite::Message;

pub struct TvSim {
    addr: SocketAddr,
    fingerprint: String,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

struct Tv {
    off: bool,
    volume: u8,
    muted: bool,
    app: String,
    prompts: u32,
    decline: bool,
    /* Just woken: reports standby for a moment, like the real TV. */
    booting: bool,
    /* WebSocket connections accepted so far, and ended so far. */
    connections: u32,
    closed: u32,
    /* Issue #44: buttons received (webOS names), the keyboard's text,
     * Enter presses, the channel on (while Live TV is). */
    presses: Vec<String>,
    typed: String,
    enters: u32,
    channel: usize,
    /* The port, for the pointer socket's address. */
    port: u16,
}

struct Shared {
    tv: Mutex<Tv>,
    /* Which subscription uris changed: every connection re-sends those. */
    changed: broadcast::Sender<&'static str>,
}

const VOLUME: &str = "ssap://audio/getVolume";
const AUDIO: &str = "ssap://audio/getStatus";
const APP: &str = "ssap://com.webos.applicationManager/getForegroundAppInfo";
const POWER: &str = "ssap://com.webos.service.tvpower/power/getPowerState";
const CHANNEL: &str = "ssap://tv/getCurrentChannel";

/* The sim's channels: (id, number, name). */
const CHANNELS: [(&str, &str, &str); 3] = [("ch-1", "1", "TVR 1"), ("ch-5", "5", "Pro TV"), ("ch-7", "7", "Antena 1")];

impl TvSim {
    /* The key the sim hands out when a pairing is accepted. */
    pub const KEY: &'static str = "sim-client-key";

    /* On, volume 12, showing HDMI_1. */
    pub async fn start() -> TvSim {
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["lgwebostv".into()])
            .unwrap()
            .self_signed(&key_pair)
            .unwrap();
        let cert_der: CertificateDer<'static> = cert.der().clone();
        let fingerprint = ring::digest::digest(&ring::digest::SHA256, cert_der.as_ref())
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

        let shared = Arc::new(Shared {
            tv: Mutex::new(Tv {
                off: false,
                volume: 12,
                muted: false,
                app: "com.webos.app.hdmi1".into(),
                prompts: 0,
                decline: false,
                booting: false,
                connections: 0,
                closed: 0,
                presses: Vec::new(),
                typed: String::new(),
                enters: 0,
                channel: 0,
                port: 0,
            }),
            changed: broadcast::channel(16).0,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        shared.tv.lock().unwrap().port = addr.port();
        let for_server = shared.clone();
        let server = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let shared = for_server.clone();
                tokio::spawn(async move {
                    /* Off: the network is asleep -- nobody answers. */
                    if shared.tv.lock().unwrap().off {
                        return;
                    }
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let Ok(ws) = tokio_tungstenite::accept_async(tls).await else { return };
                    shared.tv.lock().unwrap().connections += 1;
                    serve(ws, shared.clone()).await;
                    shared.tv.lock().unwrap().closed += 1;
                });
            }
        });
        TvSim {
            addr,
            fingerprint,
            shared,
            server,
        }
    }

    /* "127.0.0.1:<port>": the adapter uses the given port for TLS. */
    pub fn host(&self) -> String {
        self.addr.to_string()
    }

    pub fn fingerprint(&self) -> String {
        self.fingerprint.clone()
    }

    /* (accepted, ended) WebSocket connections. */
    pub fn presses(&self) -> Vec<String> {
        self.shared.tv.lock().unwrap().presses.clone()
    }

    /* (the keyboard's text, how often Enter was sent) */
    pub fn typed(&self) -> (String, u32) {
        let tv = self.shared.tv.lock().unwrap();
        (tv.typed.clone(), tv.enters)
    }

    pub fn connections(&self) -> (u32, u32) {
        let tv = self.shared.tv.lock().unwrap();
        (tv.connections, tv.closed)
    }

    pub fn prompts(&self) -> u32 {
        self.shared.tv.lock().unwrap().prompts
    }

    pub fn decline_prompts(&self) {
        self.shared.tv.lock().unwrap().decline = true;
    }

    pub fn volume(&self) -> u8 {
        self.shared.tv.lock().unwrap().volume
    }

    /* The app on screen ("com.webos.app.hdmi1", "com.webos.app.livetv"). */
    pub fn app(&self) -> String {
        self.shared.tv.lock().unwrap().app.clone()
    }

    pub fn is_off(&self) -> bool {
        self.shared.tv.lock().unwrap().off
    }

    pub fn set_volume_from_remote(&self, volume: u8) {
        self.shared.tv.lock().unwrap().volume = volume;
        let _ = self.shared.changed.send(VOLUME);
        let _ = self.shared.changed.send(AUDIO);
    }

    /* Comes back on after `delay` (as if woken) -- first in standby for
     * a moment, like the real TV, then "Active" (pushed). */
    pub fn wake_in(&self, delay: Duration) {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            {
                let mut tv = shared.tv.lock().unwrap();
                tv.off = false;
                tv.booting = true;
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
            shared.tv.lock().unwrap().booting = false;
            let _ = shared.changed.send(POWER);
        });
    }
}

impl Drop for TvSim {
    fn drop(&mut self) {
        self.server.abort();
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

/* One client connection. */
async fn serve(mut ws: Ws, shared: Arc<Shared>) {
    let mut changed = shared.changed.subscribe();
    /* This client's subscriptions: uri -> id. */
    let mut subscriptions: Vec<(String, String)> = Vec::new();
    loop {
        tokio::select! {
            incoming = ws.next() => {
                let text = match incoming {
                    /* A button, on the pointer socket (not JSON). */
                    Some(Ok(Message::Text(text))) if text.starts_with("type:button") => {
                        if let Some(name) = text.lines().find_map(|l| l.strip_prefix("name:")) {
                            shared.tv.lock().unwrap().presses.push(name.to_string());
                        }
                        continue;
                    }
                    Some(Ok(Message::Text(text))) => text,
                    /* Pings are answered by tungstenite itself. */
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => return,
                };
                let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                let id = message["id"].as_str().unwrap_or_default().to_string();
                let replies = match message["type"].as_str() {
                    Some("register") => register(&shared, &message["payload"], &id).await,
                    Some(kind @ ("request" | "subscribe")) => {
                        let uri = message["uri"].as_str().unwrap_or_default().to_string();
                        if kind == "subscribe" {
                            subscriptions.push((uri.clone(), id.clone()));
                        }
                        if uri == "ssap://system/turnOff" {
                            shared.tv.lock().unwrap().off = true;
                            return; /* the TV goes: connection closed */
                        }
                        let answer = request(&shared, &uri, &message["payload"], &id);
                        /* Like the real TV: the channel is only there while
                         * Live TV is on screen. */
                        if kind == "subscribe" && answer["type"] == "error" {
                            subscriptions.retain(|(_, i)| *i != id);
                        }
                        vec![answer]
                    }
                    _ => vec![],
                };
                for reply in replies {
                    if ws.send(Message::Text(reply.to_string())).await.is_err() {
                        return;
                    }
                }
            }
            Ok(uri) = changed.recv() => {
                for (subscribed, id) in &subscriptions {
                    if subscribed == uri {
                        let push = request(&shared, uri, &json!({}), id);
                        if ws.send(Message::Text(push.to_string())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

async fn register(shared: &Shared, payload: &Value, id: &str) -> Vec<Value> {
    if payload["client-key"] == TvSim::KEY {
        return vec![json!({"type": "registered", "id": id, "payload": {"client-key": TvSim::KEY}})];
    }
    /* Unknown or no key: the TV asks on screen. */
    let decline = {
        let mut tv = shared.tv.lock().unwrap();
        tv.prompts += 1;
        tv.decline
    };
    let mut replies = vec![json!({"type": "response", "id": id, "payload": {"pairingType": "PROMPT", "returnValue": true}})];
    if decline {
        replies.push(json!({"type": "error", "id": id, "error": "403 User denied access", "payload": {}}));
    } else if payload["client-key"].is_null() {
        /* The person accepts (a stale key would wait for them too; the
         * adapter never waits for that). */
        tokio::time::sleep(Duration::from_millis(50)).await;
        replies.push(json!({"type": "registered", "id": id, "payload": {"client-key": TvSim::KEY}}));
    }
    replies
}

/* A request (or a subscription's current value). */
fn request(shared: &Shared, uri: &str, payload: &Value, id: &str) -> Value {
    let mut tv = shared.tv.lock().unwrap();
    let answer = match uri.strip_prefix("ssap://").unwrap_or(uri) {
        "com.webos.service.tvpower/power/getPowerState" => {
            json!({"state": if tv.booting { "Active Standby" } else { "Active" }})
        }
        "audio/getVolume" => json!({"volumeStatus": {"volume": tv.volume, "muteStatus": tv.muted}}),
        "audio/getStatus" => json!({"mute": tv.muted, "volume": tv.volume}),
        "tv/getExternalInputList" => json!({"devices": [
            {"id": "HDMI_1", "label": "PlayStation", "appId": "com.webos.app.hdmi1"},
            {"id": "HDMI_2", "label": "HDMI 2", "appId": "com.webos.app.hdmi2"}
        ]}),
        "com.webos.applicationManager/getForegroundAppInfo" => json!({"appId": tv.app}),
        "system/getSystemInfo" => json!({"modelName": "OLED55SIM"}),
        "com.webos.service.connectionmanager/getinfo" => json!({
            "wiredInfo": {"state": "disconnected"},
            "wifiInfo": {"state": "connected", "macAddress": "AA:BB:CC:DD:EE:FF", "ipAddress": "127.0.0.1"}
        }),
        "audio/setVolume" => {
            tv.volume = payload["volume"].as_u64().unwrap_or(0).min(100) as u8;
            let _ = shared.changed.send(VOLUME);
            json!({})
        }
        "audio/setMute" => {
            tv.muted = payload["mute"].as_bool().unwrap_or(false);
            let _ = shared.changed.send(AUDIO);
            json!({})
        }
        "tv/switchInput" => {
            let input = payload["inputId"].as_str().unwrap_or_default();
            tv.app = format!("com.webos.app.{}", input.to_lowercase().replace('_', ""));
            let _ = shared.changed.send(APP);
            json!({})
        }
        "com.webos.service.tvpower/power/turnOnScreen" => json!({}),
        "com.webos.service.networkinput/getPointerInputSocket" => {
            json!({"socketPath": format!("wss://127.0.0.1:{}/resources/sim/netinput.pointer.sock", tv.port)})
        }
        "com.webos.service.ime/insertText" => {
            let text = payload["text"].as_str().unwrap_or_default().to_string();
            tv.typed.push_str(&text);
            json!({})
        }
        "com.webos.service.ime/deleteCharacters" => {
            for _ in 0..payload["count"].as_u64().unwrap_or(0) {
                tv.typed.pop();
            }
            json!({})
        }
        "com.webos.service.ime/sendEnterKey" => {
            tv.enters += 1;
            json!({})
        }
        "com.webos.applicationManager/listLaunchPoints" => json!({"launchPoints": [
            {"id": "netflix", "title": "Netflix"},
            {"id": "youtube.leanback.v4", "title": "YouTube"},
            {"id": "com.webos.app.livetv", "title": "Live TV"}
        ]}),
        /* Big, like a real one (365 KB on the real TV; each channel comes
         * with many more fields than these): the three real channels,
         * then 3000 more with a long made-up field -- over the 256 KB a
         * device message may normally have (net.rs). */
        "tv/getChannelList" => {
            let filler = "x".repeat(100);
            let mut list: Vec<Value> = CHANNELS
                .iter()
                .map(|(id, number, name)| json!({"channelId": id, "channelNumber": number, "channelName": name}))
                .collect();
            list.extend((100..3100).map(|n| {
                json!({"channelId": format!("ch-{n}"), "channelNumber": n.to_string(),
                       "channelName": format!("Channel {n}"), "programId": filler})
            }));
            json!({"channelList": list})
        }
        "tv/getCurrentChannel" => {
            /* Like the real TV: not while Live TV isn't on screen -- nor
             * while it's still waking up. */
            if tv.app != "com.webos.app.livetv" || tv.booting {
                return json!({"type": "error", "id": id, "error": "500 Application error", "payload": {}});
            }
            let (cid, number, name) = CHANNELS[tv.channel];
            json!({"channelId": cid, "channelNumber": number, "channelName": name})
        }
        "tv/openChannel" => {
            let wanted = payload["channelId"].as_str().unwrap_or_default();
            let Some(i) = CHANNELS.iter().position(|c| c.0 == wanted) else {
                return json!({"type": "error", "id": id, "error": "no such channel", "payload": {}});
            };
            tv.channel = i;
            let _ = shared.changed.send(CHANNEL);
            json!({})
        }
        "system.launcher/launch" => {
            tv.app = payload["id"].as_str().unwrap_or_default().to_string();
            let _ = shared.changed.send(APP);
            json!({})
        }
        _ => return json!({"type": "error", "id": id, "error": "404 no such service or method", "payload": {}}),
    };
    let mut answer = answer;
    answer["returnValue"] = json!(true);
    json!({"type": "response", "id": id, "payload": answer})
}
