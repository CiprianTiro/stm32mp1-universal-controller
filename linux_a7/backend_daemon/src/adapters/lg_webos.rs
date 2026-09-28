/*
 * lg_webos.rs -- the adapter for LG smart TVs with webOS (issue #40;
 * template templates/lg-webos-tv.json).
 *
 * THE PROTOCOL ("SSAP", LG's second-screen API; the same one LG's phone
 * app and Home Assistant use -- the message formats here follow Home
 * Assistant's aiowebostv): JSON messages over ONE WebSocket,
 *   wss://<tv>:3001  (TLS, a self-signed certificate: webOS since ~2018)
 *   ws://<tv>:3000   (older TVs, no TLS)
 * Every message has a "type" and an "id" the answer repeats:
 *   {"type":"register", "payload": {manifest, "client-key"?}}
 *        -> "registered" {"client-key"}: we're in
 *        -> first a "response" {"pairingType":"PROMPT"}: the TV asks the
 *           person ("Allow Universal Controller?") -- pairing
 *        -> "error" "403 ...": they said no
 *   {"type":"request",   "uri":"ssap://audio/setVolume", "payload":{...}}
 *        -> "response" {"returnValue": true, ...} or "error"
 *   {"type":"subscribe", "uri":"ssap://audio/getVolume"}
 *        -> "response" now, and again on every change (same id)
 *
 * SETUP (the wizard):
 *   action "pair"  first contact: any certificate is accepted, and its
 *                  fingerprint kept (trust on first use, net.rs); the TV
 *                  shows its prompt, the person accepts -> the CLIENT KEY
 *                  (kept as a secret) and the TV's MAC (for switching it on).
 *   probe          connects as a paired client (pinned certificate + key):
 *                  "LG OLED55C1..." on the test step.
 *
 * AT RUN TIME the device's task connects, registers with its key and
 * SUBSCRIBES to power, volume, mute, inputs and the app on screen; every
 * push becomes a report (switch, media). Commands are requests.
 *
 * POWER. A TV that's off (standby) has its network asleep: there's nobody
 * to talk to. So:
 *   - off = not connected. The task keeps trying, slowly; the device is
 *     shown "off" -- and online, as long as the hub can wake it.
 *   - switch on = Wake-on-LAN to its MAC (net.rs), then connecting every
 *     second until it answers (WAKE_LIMIT). Needs "Turn on via Wi-Fi" /
 *     "Mobile TV On" enabled on the TV.
 *   - switch off = "ssap://system/turnOff"; the connection then drops.
 *
 * THE REMOTE (issue #44), as `device_action`s (device::check_action):
 *   press    a button, sent over the TV's second WebSocket, the "pointer
 *            input socket" (its address comes from
 *            networkinput/getPointerInputSocket): "type:button\nname:UP"
 *   type / delete / submit   the TV's on-screen keyboard (ime/...)
 *   apps / launch            the TV's apps (listLaunchPoints, launcher)
 *   channels / tune          the channel list, a channel (tv/...)
 * The app on screen and, while watching TV, the channel are reported in
 * `media` (app, channel).
 *
 * REVOKED ACCESS. If the TV no longer accepts our key (someone removed the
 * hub from its list, a factory reset) or shows a different certificate,
 * the device becomes "unauthorized": the task stops connecting -- each
 * attempt would pop up a prompt on the TV -- until it's paired again
 * (the wizard's reauth), which restarts it with the new key.
 */
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use super::channels;
use super::net::{self, AnyWebSocket, NetError, Transport};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Channel, Device, Health, Media, MediaInput, Remote, Switch, REMOTE_BUTTONS};
use crate::secrets::Secret;
use crate::templates::ErrorKind;

const PORT_TLS: u16 = 3001;
const PORT_PLAIN: u16 = 3000;

/* One request's answer. */
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/* Keep-alive, as in wled.rs. (Much shorter in tests, so a test can
 * watch several rounds.) */
#[cfg(not(test))]
const PING_EVERY: Duration = Duration::from_secs(20);
#[cfg(not(test))]
const SILENT_LIMIT: Duration = Duration::from_secs(50);
#[cfg(test)]
const PING_EVERY: Duration = Duration::from_millis(100);
#[cfg(test)]
const SILENT_LIMIT: Duration = Duration::from_millis(250);
/* Reconnecting to a TV that's off: it's off most of the day, so after a
 * few quick tries, once every RETRY_MAX. */
const RETRY_MIN: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(30);
/* After switching it off: the TV takes a few seconds to go, still
 * answering meanwhile -- don't reconnect to a TV on its way out. */
const AFTER_OFF: Duration = Duration::from_secs(10);
/* After Wake-on-LAN: how long a TV may take to boot its network. */
const WAKE_LIMIT: Duration = Duration::from_secs(30);

/* Watching TV channels isn't one of the TV's inputs (its input list only
 * has the HDMI/AV ports) but an app of its own, "Live TV": offered as the
 * first input, with this id; choosing it launches the app. (Seen on the
 * real TV: channels showed no input, and there was no way back to them.) */
const LIVE_TV: &str = "TV";
const LIVE_TV_LABEL: &str = "Live TV";
const LIVE_TV_APP: &str = "com.webos.app.livetv";

/* The button socket is opened when first needed, and again after it's
 * been idle this long: the TV may drop an idle one without telling, and a
 * press into a dead socket would be lost silently. */
const POINTER_IDLE: Duration = Duration::from_secs(30);

/* The hub's button names (device::REMOTE_BUTTONS) -> webOS's. */
fn webos_button(button: &str) -> &str {
    match button {
        "OK" => "ENTER",
        "VOLUME_UP" => "VOLUMEUP",
        "VOLUME_DOWN" => "VOLUMEDOWN",
        "CHANNEL_UP" => "CHANNELUP",
        "CHANNEL_DOWN" => "CHANNELDOWN",
        "FAST_FORWARD" => "FASTFORWARD",
        other => other,
    }
}

/* Secret and config names (the template's inputs + what pairing adds). */
const KEY: &str = "client_key";
const CERT: &str = "cert_sha256";

/* The permissions the hub asks for when pairing. The TV approves exactly
 * this list at the prompt -- and refuses anything outside it with "401
 * insufficient permissions" (seen on the real TV: #40's trimmed list
 * lacked the buttons, the keyboard and the channel list). A TV paired
 * with a shorter list must be paired again to get more.
 *
 * So: Home Assistant's list (aiowebostv's handshake.py), tested on many
 * TVs, rather than a hand-picked one that fails a feature at a time. The
 * pairing key it earns never leaves the hub (secrets.rs). Unsigned
 * manifests are accepted by current webOS. */
const PERMISSIONS: &[&str] = &[
    "APP_TO_APP",
    "CLOSE",
    "CONTROL_AUDIO",
    "CONTROL_DISPLAY",
    "CONTROL_INPUT_JOYSTICK",
    "CONTROL_INPUT_MEDIA_PLAYBACK",
    "CONTROL_INPUT_MEDIA_RECORDING",
    "CONTROL_INPUT_TEXT",
    "CONTROL_INPUT_TV",
    "CONTROL_MOUSE_AND_KEYBOARD",
    "CONTROL_POWER",
    "CONTROL_TV_SCREEN",
    "LAUNCH",
    "LAUNCH_WEBAPP",
    "READ_APP_STATUS",
    "READ_COUNTRY_INFO",
    "READ_CURRENT_CHANNEL",
    "READ_INPUT_DEVICE_LIST",
    "READ_INSTALLED_APPS",
    "READ_LGE_SDX",
    "READ_LGE_TV_INPUT_EVENTS",
    "READ_NETWORK_STATE",
    "READ_NOTIFICATIONS",
    "READ_POWER_STATE",
    "READ_RUNNING_APPS",
    "READ_SETTINGS",
    "READ_TV_CHANNEL_LIST",
    "READ_TV_CURRENT_TIME",
    "READ_UPDATE_INFO",
    "SEARCH",
    "TEST_OPEN",
    "TEST_PROTECTED",
    "TEST_SECURE",
    "UPDATE_FROM_REMOTE_APP",
    "WRITE_NOTIFICATION_ALERT",
    "WRITE_NOTIFICATION_TOAST",
    "WRITE_SETTINGS",
];

pub struct LgWebos;

impl Adapter for LgWebos {
    fn id(&self) -> &'static str {
        "lg-webos"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let config = |key: &str| device.config.get(key).filter(|v| !v.is_empty()).cloned();
        let task = Task {
            id: device.id.clone(),
            has_remote: device.capabilities.remote.is_some(),
            host: config("host"),
            mac: config("mac"),
            cert: config(CERT),
            hub,
            tv: TvState::default(),
        };
        tokio::spawn(task.run(commands_rx));
        DeviceHandle { commands }
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(probe(values))
    }

    fn action<'a>(&'a self, name: &'a str, values: &'a SetupValues) -> BoxFuture<'a, Result<SetupValues, SetupError>> {
        Box::pin(async move {
            match name {
                "pair" => pair(values).await,
                other => Err(SetupError::new(ErrorKind::Unsupported, format!("no action {other:?}"))),
            }
        })
    }
}

/* ------------------------------------------------------------------ */
/* One connection                                                      */
/* ------------------------------------------------------------------ */

struct Conn {
    ws: AnyWebSocket,
    /* The button socket (see POINTER_IDLE), and when it was last used. */
    pointer: Option<(AnyWebSocket, Instant)>,
    next_id: u64,
    /* Messages that arrived while waiting for a request's answer
     * (subscription pushes): handled next. */
    backlog: VecDeque<Value>,
    /* When anything last came from the TV -- including the pongs to our
     * keep-alive pings, which recv_socket otherwise skips. (Only counting
     * JSON messages made a quiet TV look dead after SILENT_LIMIT: seen on
     * the real TV, a reconnect every 60 s.) */
    heard: Instant,
}

/* Why registering failed. */
enum RegisterError {
    /* Not reachable, or the connection broke. */
    Net(NetError),
    /* The TV wants to ask the person (our key isn't valid any more). */
    NeedsPrompt,
    /* The person (or the TV) said no. */
    Denied(String),
}

impl Conn {
    /* Connects: with a pinned certificate -> TLS only; without one (the
     * first contact) -> TLS if the TV has it, else the old plain port.
     * Also returns the certificate's fingerprint (None: plain). */
    async fn open(host: &str, cert: Option<&str>, first_contact: bool) -> Result<(Conn, Option<String>), NetError> {
        let (name, port) = split_host(host);
        /* The main connection carries the channel list: large messages. */
        let big = net::LARGE_WS_MESSAGE;
        let tls = net::ws_open_sized(name, port.unwrap_or(PORT_TLS), "/", Transport::Pinned(cert), big).await;
        let (ws, fingerprint) = match tls {
            Ok(opened) => opened,
            /* A TV paired over TLS stays on TLS: no quiet downgrade. (A
             * given port -- the tests' sim -- is TLS only too.) */
            Err(e) if cert.is_some() || !first_contact || port.is_some() => return Err(e),
            Err(_) => net::ws_open_sized(name, PORT_PLAIN, "/", Transport::Plain, big).await?,
        };
        let conn = Conn {
            ws,
            pointer: None,
            next_id: 0,
            backlog: VecDeque::new(),
            heard: Instant::now(),
        };
        Ok((conn, fingerprint))
    }

    /* The next message: from the backlog, else the socket. */
    async fn recv(&mut self) -> Result<Value, NetError> {
        if let Some(message) = self.backlog.pop_front() {
            return Ok(message);
        }
        self.recv_socket().await
    }

    async fn recv_socket(&mut self) -> Result<Value, NetError> {
        loop {
            let frame = self.ws.next().await;
            if matches!(frame, Some(Ok(_))) {
                self.heard = Instant::now();
            }
            match frame {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(message) = serde_json::from_str::<Value>(&text) {
                        return Ok(message);
                    }
                }
                Some(Ok(Message::Close(_))) | None => return Err(closed()),
                Some(Ok(_)) => {} /* ping/pong/binary */
                Some(Err(e)) => return Err(NetError::new(ErrorKind::Unreachable, format!("connection lost: {e}"))),
            }
        }
    }

    async fn send(&mut self, message: Value) -> Result<(), NetError> {
        self.ws
            .send(Message::Text(message.to_string()))
            .await
            .map_err(|e| NetError::new(ErrorKind::Unreachable, format!("connection lost: {e}")))
    }

    /* Registers; `key` None = pair (the TV prompts), then waits as long
     * as the caller lets it. Returns the (new or same) client key. */
    async fn register(&mut self, key: Option<&str>) -> Result<String, RegisterError> {
        let mut payload = json!({
            "forcePairing": false,
            "pairingType": "PROMPT",
            "manifest": { "manifestVersion": 1, "appVersion": "1.1", "permissions": PERMISSIONS },
        });
        if let Some(key) = key {
            payload["client-key"] = json!(key);
        }
        self.send(json!({ "type": "register", "id": "register", "payload": payload }))
            .await
            .map_err(RegisterError::Net)?;
        loop {
            let message = self.recv().await.map_err(RegisterError::Net)?;
            if message["id"] != "register" {
                continue;
            }
            match message["type"].as_str() {
                Some("registered") => {
                    return message["payload"]["client-key"]
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| RegisterError::Denied("the TV sent no key".into()))
                }
                Some("response") if message["payload"]["pairingType"] == "PROMPT" => {
                    if key.is_some() {
                        return Err(RegisterError::NeedsPrompt);
                    }
                    /* Pairing: the prompt is on screen now, wait. */
                }
                Some("error") => return Err(RegisterError::Denied(message["error"].as_str().unwrap_or("refused").to_string())),
                _ => {}
            }
        }
    }

    /* A request or subscription (`kind`); waits for its answer, keeping
     * other messages for later. `id`: fixed for subscriptions (their pushes
     * carry it), else a fresh one. */
    async fn call(&mut self, kind: &str, id: Option<&str>, uri: &str, payload: Value) -> Result<Value, NetError> {
        let id = match id {
            Some(id) => id.to_string(),
            None => {
                self.next_id += 1;
                format!("req-{}", self.next_id)
            }
        };
        self.send(json!({ "type": kind, "id": id, "uri": format!("ssap://{uri}"), "payload": payload }))
            .await?;
        let answer = tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                let message = self.recv_socket().await?;
                if message["id"] == id.as_str() {
                    return Ok(message);
                }
                self.backlog.push_back(message);
            }
        })
        .await
        .map_err(|_| NetError::new(ErrorKind::Timeout, format!("the TV didn't answer {uri}")))??;
        if answer["type"] == "error" || answer["payload"]["returnValue"] == false {
            let why = answer["error"]
                .as_str()
                .or(answer["payload"]["errorText"].as_str())
                .unwrap_or("refused");
            /* The TV didn't approve this at pairing (see PERMISSIONS):
             * only pairing again fixes it -- say so. */
            if why.starts_with("401") || why.contains("insufficient permissions") {
                return Err(NetError::new(
                    ErrorKind::Refused,
                    "The TV didn't allow this when it was paired. Pair it again (tap its name, then Pair again) and accept on the TV.".into(),
                ));
            }
            return Err(NetError::new(ErrorKind::Unsupported, format!("{uri}: {why}")));
        }
        Ok(answer)
    }

    async fn request(&mut self, uri: &str, payload: Value) -> Result<Value, NetError> {
        Ok(self.call("request", None, uri, payload).await?["payload"].take())
    }
}

/* "192.168.1.40" -> (it, None); "127.0.0.1:4000" (a port given: the
 * tests' simulated TV) -> ("127.0.0.1", Some(4000)). */
fn split_host(host: &str) -> (&str, Option<u16>) {
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => match port.parse() {
            Ok(port) => (name, Some(port)),
            Err(_) => (host, None),
        },
        _ => (host, None),
    }
}

/* A channel from webOS ({"channelId", "channelNumber", "channelName"}). */
fn channel_of(value: &Value) -> Option<Channel> {
    let text = |key: &str| value[key].as_str().unwrap_or_default().chars().take(64).collect::<String>();
    let id = text("channelId");
    if id.is_empty() {
        return None;
    }
    Some(Channel {
        id,
        number: text("channelNumber"),
        name: text("channelName"),
    })
}

fn closed() -> NetError {
    NetError::new(ErrorKind::Unreachable, "the TV closed the connection".into())
}

/* ------------------------------------------------------------------ */
/* Setup                                                               */
/* ------------------------------------------------------------------ */

fn host_of(values: &SetupValues) -> Result<&str, SetupError> {
    values
        .plain
        .get("host")
        .map(String::as_str)
        .ok_or_else(|| SetupError::new(ErrorKind::Unsupported, "no address given"))
}

/* The "pair" action (see the header). Waits for the person; the wizard
 * decides how long (the step's timeout_s). */
async fn pair(values: &SetupValues) -> Result<SetupValues, SetupError> {
    let host = host_of(values)?;
    let (mut conn, fingerprint) = Conn::open(host, None, true).await?;
    let key = match conn.register(None).await {
        Ok(key) => key,
        Err(RegisterError::Net(e)) => return Err(e.into()),
        Err(RegisterError::NeedsPrompt) => unreachable!("only asked with a key"),
        Err(RegisterError::Denied(why)) => return Err(SetupError::new(ErrorKind::NotConfirmed, why)),
    };
    let mut new = SetupValues::default();
    new.secret.insert(KEY.into(), Secret::new(key));
    /* The certificate to trust from now on ("" = the TV has no TLS). */
    new.plain.insert(CERT.into(), fingerprint.unwrap_or_default());
    /* Its MAC, for switching it on: from the TV itself (the network
     * interface it answered on) -- or, if it doesn't say, from the board's
     * own ARP table: we just talked to it, so the kernel knows. */
    let told = match conn.request("com.webos.service.connectionmanager/getinfo", json!({})).await {
        Ok(info) => {
            let mac = mac_of(&info, host);
            if mac.is_none() {
                let keys: Vec<&String> = info.as_object().map(|o| o.keys().collect()).unwrap_or_default();
                println!("lg-webos: pairing {host}: no MAC in connectionmanager/getinfo (keys: {keys:?})");
            }
            mac
        }
        Err(e) => {
            println!("lg-webos: pairing {host}: connectionmanager/getinfo failed: {e}");
            None
        }
    };
    if let Some(mac) = told.or_else(|| net::mac_from_arp(split_host(host).0)) {
        new.plain.insert("mac".into(), mac);
    }
    let _ = conn.ws.close(None).await;
    Ok(new)
}

/* From connectionmanager/getinfo: the MAC of the interface with `host`'s
 * address, else of whichever is connected. */
fn mac_of(info: &Value, host: &str) -> Option<String> {
    let interfaces = [&info["wiredInfo"], &info["wifiInfo"]];
    let mac = |i: &Value| i["macAddress"].as_str().filter(|m| m.len() == 17).map(str::to_lowercase);
    interfaces
        .iter()
        .find(|i| i["ipAddress"] == host)
        .and_then(|i| mac(i))
        .or_else(|| interfaces.iter().filter(|i| i["state"] == "connected").find_map(|i| mac(i)))
}

/* The test step: connect as the paired client, read the model. */
async fn probe(values: &SetupValues) -> Result<Probe, SetupError> {
    let host = host_of(values)?;
    let key = values
        .secret
        .get(KEY)
        .ok_or_else(|| SetupError::new(ErrorKind::NotConfirmed, "not paired yet"))?;
    let cert = values.plain.get(CERT).filter(|c| !c.is_empty());
    let (mut conn, _) = Conn::open(host, cert.map(String::as_str), cert.is_none()).await?;
    match conn.register(Some(key.expose())).await {
        Ok(_) => {}
        Err(RegisterError::Net(e)) => return Err(e.into()),
        Err(RegisterError::NeedsPrompt | RegisterError::Denied(_)) => {
            return Err(SetupError::new(ErrorKind::Refused, "the TV didn't accept the pairing key"))
        }
    }
    let info = conn.request("system/getSystemInfo", json!({})).await.unwrap_or_default();
    let _ = conn.ws.close(None).await;
    let model = info["modelName"].as_str().unwrap_or("webOS TV");
    Ok(Probe {
        summary: format!("LG {model}"),
        ..Default::default()
    })
}

/* ------------------------------------------------------------------ */
/* Run time                                                            */
/* ------------------------------------------------------------------ */

/* What the TV told us, kept to build the reports. */
#[derive(Default)]
struct TvState {
    on: bool,
    media: Media,
    /* Input app ("com.webos.app.hdmi1") -> input id ("HDMI_1"). */
    input_apps: HashMap<String, String>,
    /* The app on screen. */
    app: String,
    /* App id -> its name, from the TV's list of apps (for media.app). */
    app_labels: HashMap<String, String>,
    /* Following the channel on screen (see follow_channel). */
    channel_follow: ChannelFollow,
    /* The TV's whole channel list, for searching and paging it
     * (adapters/channels.rs); fetched when the list is opened. */
    channel_list: Option<Vec<Channel>>,
}

/* The channel subscription only works while Live TV is on screen -- and
 * not even then right after the TV woke up: its Live TV app isn't ready
 * yet and refuses it (seen on the real TV: the channel never showed after
 * "switch on"). So it's asked for again, every CHANNEL_RETRY, until a
 * channel comes (at most CHANNEL_ATTEMPTS times); and afresh on every new
 * connection and every time Live TV comes back. */
#[derive(Default)]
struct ChannelFollow {
    subscribed: bool,
    attempts: u32,
    last: Option<Instant>,
}
#[cfg(not(test))]
const CHANNEL_RETRY: Duration = Duration::from_secs(5);
#[cfg(test)]
const CHANNEL_RETRY: Duration = Duration::from_millis(200);
const CHANNEL_ATTEMPTS: u32 = 12;

/* The subscriptions (their fixed ids). */
const CHANNEL_SUBSCRIPTION: (&str, &str) = ("channel", "tv/getCurrentChannel");
const SUBSCRIPTIONS: [(&str, &str); 5] = [
    ("power", "com.webos.service.tvpower/power/getPowerState"),
    ("volume", "audio/getVolume"),
    ("audio", "audio/getStatus"),
    ("inputs", "tv/getExternalInputList"),
    ("app", "com.webos.applicationManager/getForegroundAppInfo"),
];

struct Task {
    id: String,
    /* The device has the `remote` capability (TVs added before #44 get
     * it at start, see main.rs). */
    has_remote: bool,
    host: Option<String>,
    mac: Option<String>,
    cert: Option<String>,
    hub: Hub,
    tv: TvState,
}

enum End {
    Lost,
    /* We switched it off (see session). */
    SwitchedOff,
    Stopped,
    Unauthorized(String),
}

/* A "switch on" waiting for the TV to come up. */
struct Waking {
    reply: oneshot::Sender<Result<(), String>>,
    until: Instant,
}

impl Task {
    async fn run(mut self, mut commands: mpsc::Receiver<DeviceCmd>) {
        let key = self.hub.secrets(&self.id).get(KEY).map(|k| k.expose().to_string());
        let (Some(host), Some(mut key)) = (self.host.clone(), key) else {
            println!("lg-webos: {} isn't paired (no address or key): pair it again", self.id);
            self.park(Health::Unauthorized, &mut commands, "it needs to be paired again").await;
            return;
        };

        let mut retry = RETRY_MIN;
        let mut waking: Option<Waking> = None;
        let mut said_off = false;
        loop {
            /* Waking: the magic packet again before every attempt -- a TV
             * still shutting down, or one that missed a packet, ignores it
             * (seen on the real TV: "on" right after "off"). */
            if let (Some(_), Some(mac)) = (&waking, &self.mac) {
                let _ = net::wake_on_lan(mac, Some(split_host(&host).0)).await;
            }
            match self.connect(&host, &mut key).await {
                Ok(conn) => {
                    retry = RETRY_MIN;
                    said_off = false;
                    println!("lg-webos: {} connected at {host}", self.id);
                    /* A pending "on" is answered in the session, once the TV
                     * says it's on: right after waking it first reports
                     * standby (seen on the real TV). */
                    match self.session(conn, &mut commands, &mut waking).await {
                        End::Stopped => return,
                        End::Lost => println!("lg-webos: {} disconnected (switched off?)", self.id),
                        End::SwitchedOff => {
                            println!("lg-webos: {} switched off", self.id);
                            retry = AFTER_OFF;
                        }
                        End::Unauthorized(why) => {
                            println!("lg-webos: {}: {why}", self.id);
                            self.park(Health::Unauthorized, &mut commands, "it needs to be paired again").await;
                            return;
                        }
                    }
                }
                Err(End::Unauthorized(why)) => {
                    println!("lg-webos: {}: {why}", self.id);
                    if let Some(w) = waking.take() {
                        let _ = w.reply.send(Err("the TV needs to be paired again".into()));
                    }
                    self.park(Health::Unauthorized, &mut commands, "it needs to be paired again").await;
                    return;
                }
                Err(_) => {
                    if !said_off {
                        println!("lg-webos: {} not reachable at {host} (off?); will keep trying", self.id);
                        said_off = true;
                    }
                }
            }
            self.report_off().await;

            /* Off: wait, answering commands. While waking, try every
             * second; give up after WAKE_LIMIT. */
            let delay = if waking.is_some() { Duration::from_secs(1) } else { retry };
            let wait = tokio::time::sleep(delay);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    _ = &mut wait => break,
                    cmd = commands.recv() => match cmd {
                        None => return,
                        Some(cmd) => {
                            if let Some(w) = self.command_while_off(&host, cmd).await {
                                /* A newer "on" replaces an older one. */
                                if let Some(old) = waking.replace(w) {
                                    let _ = old.reply.send(Ok(()));
                                }
                                break;
                            }
                        }
                    },
                }
            }
            if let Some(w) = waking.take() {
                if Instant::now() < w.until {
                    waking = Some(w);
                } else {
                    let _ = w.reply.send(Err(
                        "The TV didn't wake up. On the TV, turn on \"Turn on via Wi-Fi\" (or \"Mobile TV On\") in its settings.".into(),
                    ));
                }
            }
            if waking.is_none() {
                retry = (retry * 2).min(RETRY_MAX).max(RETRY_MIN);
            }
        }
    }

    /* Connect, register with our key, subscribe; the TV's state is
     * reported. Err(Unauthorized) when the TV no longer accepts us. If the
     * TV answers with a NEW key (webOS may renew it), that one is kept --
     * in `key` and in the secrets file. */
    async fn connect(&mut self, host: &str, key: &mut String) -> Result<Conn, End> {
        let (mut conn, _) = Conn::open(host, self.cert.as_deref(), self.cert.is_none()).await.map_err(|e| {
            if e.kind == ErrorKind::Refused {
                End::Unauthorized(e.message)
            } else {
                End::Lost
            }
        })?;
        match conn.register(Some(key)).await {
            Ok(new) if new != *key => {
                println!("lg-webos: {}: the TV renewed its key, saved", self.id);
                self.hub.store_secrets(&self.id, [(KEY.to_string(), Secret::new(new.clone()))].into());
                *key = new;
            }
            Ok(_) => {}
            Err(RegisterError::Net(_)) => return Err(End::Lost),
            Err(RegisterError::NeedsPrompt) => return Err(End::Unauthorized("the TV no longer accepts our key".into())),
            Err(RegisterError::Denied(why)) => return Err(End::Unauthorized(format!("the TV refused us: {why}"))),
        }
        /* No MAC yet (the TV didn't tell at pairing): the board's ARP table
         * knows it now that we're connected. Saved, for switching it on. */
        if self.mac.is_none() {
            if let Some(mac) = net::mac_from_arp(split_host(host).0) {
                println!("lg-webos: {}: MAC {mac} (from the ARP table), saved", self.id);
                self.hub.store_config(&self.id, "mac", &mac).await;
                self.mac = Some(mac);
            }
        }
        /* Connected: it's on, unless the power subscription says it's
         * only in standby (older TVs don't have it). A new connection has
         * no subscriptions yet -- the channel's included (forgetting that
         * froze the channel after a reconnect, seen on the real TV). */
        self.tv.on = true;
        self.tv.channel_follow = ChannelFollow::default();
        for (id, uri) in SUBSCRIPTIONS {
            match conn.call("subscribe", Some(id), uri, json!({})).await {
                Ok(first) => self.apply(&first),
                /* Not every TV has every one ("404 no such service"). */
                Err(e) if e.kind == ErrorKind::Unsupported => {}
                Err(_) => return Err(End::Lost),
            }
        }
        /* The apps' names, for media.app ("Netflix", not its id). */
        if let Ok(apps) = self.list_apps(&mut conn).await {
            self.tv.app_labels = apps.into_iter().map(|a| (a.id, a.label)).collect();
            self.update_input();
        }
        self.follow_channel(&mut conn).await;
        if self.has_remote {
            let remote = Remote {
                buttons: REMOTE_BUTTONS.iter().map(|b| b.to_string()).collect(),
                keyboard: true,
            };
            if let Err(e) = self.hub.report(&self.id, "remote", json!(remote)).await {
                println!("lg-webos: {}: {e}", self.id);
            }
        }
        self.report().await;
        Ok(conn)
    }

    /* Whether the channel should be asked for (again) now or soon: Live
     * TV on screen, no channel known yet, attempts left. */
    fn channel_wanted(&self) -> bool {
        let follow = &self.tv.channel_follow;
        self.tv.app == LIVE_TV_APP
            && (!follow.subscribed || self.tv.media.channel.is_none())
            && follow.attempts < CHANNEL_ATTEMPTS
    }

    /* The channel subscription (see ChannelFollow), if wanted and its
     * time has come. True if a channel came in (the caller reports it). */
    async fn follow_channel(&mut self, conn: &mut Conn) -> bool {
        let due = self.tv.channel_follow.last.is_none_or(|t| t.elapsed() >= CHANNEL_RETRY);
        if !self.channel_wanted() || !due {
            return false;
        }
        let follow = &mut self.tv.channel_follow;
        follow.attempts += 1;
        follow.last = Some(Instant::now());
        let (id, uri) = CHANNEL_SUBSCRIPTION;
        match conn.call("subscribe", Some(id), uri, json!({})).await {
            Ok(first) => {
                self.tv.channel_follow.subscribed = true;
                self.apply(&first);
                self.tv.media.channel.is_some()
            }
            Err(_) => false,
        }
    }

    /* The TV's apps (launch points), as {id, label}. */
    async fn list_apps(&mut self, conn: &mut Conn) -> Result<Vec<MediaInput>, NetError> {
        let answer = conn.request("com.webos.applicationManager/listLaunchPoints", json!({})).await?;
        Ok(answer["launchPoints"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|a| {
                        let id = a["id"].as_str()?.chars().take(64).collect::<String>();
                        let label = a["title"].as_str().unwrap_or(&id).chars().take(64).collect();
                        Some(MediaInput { id, label })
                    })
                    .take(200)
                    .collect()
            })
            .unwrap_or_default())
    }

    /* A remote or media action (issue #44), while connected. The rules
     * (which actions, which arguments) were checked by control.rs. */
    async fn action(&mut self, conn: &mut Conn, capability: &str, name: &str, args: &Value) -> Result<Value, NetError> {
        let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
        match (capability, name) {
            ("remote", "press") => {
                self.press(conn, webos_button(&text("button"))).await?;
                Ok(json!({}))
            }
            ("remote", "type") => {
                conn.request("com.webos.service.ime/insertText", json!({ "text": text("text"), "replace": 0 }))
                    .await?;
                Ok(json!({}))
            }
            ("remote", "delete") => {
                let count = args["count"].as_u64().unwrap_or(1);
                conn.request("com.webos.service.ime/deleteCharacters", json!({ "count": count })).await?;
                Ok(json!({}))
            }
            ("remote", "submit") => {
                conn.request("com.webos.service.ime/sendEnterKey", json!({})).await?;
                Ok(json!({}))
            }
            ("media", "apps") => {
                let apps = self.list_apps(conn).await?;
                self.tv.app_labels = apps.iter().map(|a| (a.id.clone(), a.label.clone())).collect();
                Ok(json!({ "apps": apps }))
            }
            ("media", "launch") => {
                conn.request("system.launcher/launch", json!({ "id": text("app") })).await?;
                Ok(json!({}))
            }
            ("media", "channels") => {
                /* Fetched from the TV when the list is opened (it may have
                 * changed: a channel scan); searches and further pages
                 * use what we have. */
                let query = channels::Query::from_args(args);
                if query.is_fresh_look() || self.tv.channel_list.is_none() {
                    let answer = conn.request("tv/getChannelList", json!({})).await?;
                    let list = answer["channelList"]
                        .as_array()
                        .map(|list| list.iter().filter_map(channel_of).take(20_000).collect())
                        .unwrap_or_default();
                    self.tv.channel_list = Some(list);
                }
                Ok(channels::page(self.tv.channel_list.as_deref().unwrap_or_default(), &query))
            }
            ("media", "tune") => {
                conn.request("tv/openChannel", json!({ "channelId": text("channel") })).await?;
                Ok(json!({}))
            }
            _ => Err(NetError::new(ErrorKind::Unsupported, format!("a TV can't {capability} {name}"))),
        }
    }

    /* One button over the pointer socket, opened (again) when needed. */
    async fn press(&mut self, conn: &mut Conn, button: &str) -> Result<(), NetError> {
        let fresh = matches!(&conn.pointer, Some((_, used)) if used.elapsed() < POINTER_IDLE);
        if !fresh {
            conn.pointer = None;
            let answer = conn.request("com.webos.service.networkinput/getPointerInputSocket", json!({})).await?;
            let url = answer["socketPath"].as_str().unwrap_or_default();
            /* Refused, not Unreachable: the TV itself still answers (see
             * session's `lost`). */
            let socket = self
                .open_pointer(url)
                .await
                .map_err(|e| NetError::new(ErrorKind::Refused, format!("the button socket: {e}")))?;
            conn.pointer = Some((socket, Instant::now()));
        }
        let message = format!("type:button\nname:{button}\n\n");
        let (socket, used) = conn.pointer.as_mut().expect("opened above");
        if let Err(e) = socket.send(Message::Text(message)).await {
            conn.pointer = None;
            return Err(NetError::new(ErrorKind::Refused, format!("the button couldn't be sent, try again: {e}")));
        }
        *used = Instant::now();
        Ok(())
    }

    /* The pointer socket's URL ("wss://192.168.1.40:3001/resources/…/
     * netinput.pointer.sock") -> a connection, with the same certificate
     * pinning as the main one. The TV's own address is used, not the one
     * in the URL (a TV with two network interfaces may name the other). */
    async fn open_pointer(&self, url: &str) -> Result<AnyWebSocket, NetError> {
        let bad = || NetError::new(ErrorKind::Unsupported, format!("unexpected button socket address {url:?}"));
        let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
        let (authority, path) = rest.split_at(rest.find('/').ok_or_else(bad)?);
        let port: u16 = authority.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).ok_or_else(bad)?;
        let host = split_host(self.host.as_deref().unwrap_or_default()).0;
        let transport = if scheme == "wss" { Transport::Pinned(self.cert.as_deref()) } else { Transport::Plain };
        Ok(net::ws_open(host, port, path, transport).await?.0)
    }

    async fn session(&mut self, mut conn: Conn, commands: &mut mpsc::Receiver<DeviceCmd>, waking: &mut Option<Waking>) -> End {
        let mut tick = tokio::time::interval(PING_EVERY);
        tick.tick().await;
        loop {
            /* Pushes that arrived while a request waited. */
            while let Some(message) = conn.backlog.pop_front() {
                self.apply(&message);
                self.report().await;
            }
            /* Live TV came on screen: follow its channel (and show it). */
            if self.follow_channel(&mut conn).await {
                self.report().await;
            }
            /* A pending "on": answered once the TV says it's on. */
            if self.tv.on {
                if let Some(w) = waking.take() {
                    let _ = w.reply.send(Ok(()));
                }
            }
            tokio::select! {
                message = conn.recv_socket() => match message {
                    Ok(message) => {
                        self.apply(&message);
                        self.report().await;
                    }
                    Err(_) => return End::Lost,
                },
                cmd = commands.recv() => match cmd {
                    None => return End::Stopped,
                    /* "On" while connected in standby: wake it, and answer
                     * when it reports on (like an "on" while off). */
                    Some(DeviceCmd::Command { capability, value, reply })
                        if capability == "switch" && value["on"] == true && !self.tv.on =>
                    {
                        if let Some(mac) = &self.mac {
                            let _ = net::wake_on_lan(mac, self.host.as_deref().map(|h| split_host(h).0)).await;
                        }
                        let _ = conn.request("com.webos.service.tvpower/power/turnOnScreen", json!({})).await;
                        if let Some(old) = waking.replace(Waking { reply, until: Instant::now() + WAKE_LIMIT }) {
                            let _ = old.reply.send(Ok(()));
                        }
                    }
                    Some(DeviceCmd::Action { capability, name, args, reply }) => {
                        let result = self.action(&mut conn, &capability, &name, &args).await;
                        /* Only the MAIN connection failing means the TV is gone
                         * (a broken button socket is just opened again, see
                         * press). */
                        let lost = matches!(&result, Err(e) if e.kind == ErrorKind::Unreachable);
                        let _ = reply.send(result.map_err(|e| e.message));
                        if lost {
                            return End::Lost;
                        }
                    }
                    Some(DeviceCmd::Command { capability, value, reply }) => {
                        let switched_off = capability == "switch" && value["on"] == false && self.tv.on;
                        let result = self.command(&mut conn, &capability, &value).await;
                        let lost = matches!(&result, Err(e) if e.kind == ErrorKind::Unreachable);
                        let ok = result.is_ok();
                        let _ = reply.send(result.map_err(|e| e.message));
                        /* Switched off: we let go at once. The TV takes a few
                         * seconds to go, meanwhile still saying it's on --
                         * an "on" in that time must wake it properly, not be
                         * answered by a TV about to vanish. */
                        if lost {
                            return End::Lost;
                        }
                        if switched_off && ok {
                            return End::SwitchedOff;
                        }
                    }
                },
                /* Only while the channel is still wanted: wakes the loop so
                 * follow_channel (at its top) asks again. */
                _ = tokio::time::sleep(CHANNEL_RETRY), if self.channel_wanted() => {}
                _ = tick.tick() => {
                    if waking.as_ref().is_some_and(|w| Instant::now() > w.until) {
                        if let Some(w) = waking.take() {
                            let _ = w.reply.send(Err("The TV answers, but didn't switch on.".into()));
                        }
                    }
                    /* conn.heard: any frame, our pings' pongs included. */
                    if conn.heard.elapsed() > SILENT_LIMIT || conn.ws.send(Message::Ping(Vec::new())).await.is_err() {
                        return End::Lost;
                    }
                }
            }
        }
    }

    /* A command while connected. */
    async fn command(&mut self, conn: &mut Conn, capability: &str, value: &Value) -> Result<(), NetError> {
        match capability {
            "switch" => {
                let on = value["on"].as_bool().unwrap_or(false);
                if on == self.tv.on {
                    return Ok(());
                }
                if on {
                    /* Not reached: session handles "on" in standby itself
                     * (it answers later, when the TV is on). */
                    return Ok(());
                }
                /* The TV may go before it answers: don't wait for one. */
                conn.send(json!({ "type": "request", "id": "off", "uri": "ssap://system/turnOff" }))
                    .await?;
                self.tv.on = false;
                self.report().await;
                Ok(())
            }
            "media" => {
                let wanted: Media = serde_json::from_value(value.clone())
                    .map_err(|e| NetError::new(ErrorKind::Unsupported, format!("invalid media value: {e}")))?;
                if !self.tv.on {
                    return Err(NetError::new(ErrorKind::Unsupported, "The TV is off. Switch it on first.".into()));
                }
                if wanted.volume != self.tv.media.volume {
                    conn.request("audio/setVolume", json!({ "volume": wanted.volume })).await?;
                    self.tv.media.volume = wanted.volume;
                }
                if wanted.muted != self.tv.media.muted {
                    conn.request("audio/setMute", json!({ "mute": wanted.muted })).await?;
                    self.tv.media.muted = wanted.muted;
                }
                if !wanted.input.is_empty() && wanted.input != self.tv.media.input {
                    let known = self.tv.media.inputs.iter().any(|i| i.id == wanted.input);
                    if !known && !self.tv.media.inputs.is_empty() {
                        return Err(NetError::new(ErrorKind::Unsupported, format!("the TV has no input {:?}", wanted.input)));
                    }
                    if wanted.input == LIVE_TV {
                        conn.request("system.launcher/launch", json!({ "id": LIVE_TV_APP })).await?;
                    } else {
                        conn.request("tv/switchInput", json!({ "inputId": wanted.input })).await?;
                    }
                    self.tv.media.input = wanted.input;
                }
                self.report().await;
                Ok(())
            }
            other => Err(NetError::new(ErrorKind::Unsupported, format!("a TV has no capability {other:?}"))),
        }
    }

    /* A command while the TV is off (not connected). Some(Waking) = a
     * "switch on" was started: the caller retries quickly and answers it
     * when the TV is up. */
    async fn command_while_off(&mut self, host: &str, cmd: DeviceCmd) -> Option<Waking> {
        let DeviceCmd::Command { capability, value, reply } = cmd else {
            cmd.refuse("The TV is off. Switch it on first.");
            return None;
        };
        if capability != "switch" {
            let _ = reply.send(Err("The TV is off. Switch it on first.".into()));
            return None;
        }
        if value["on"] != true {
            let _ = reply.send(Ok(())); /* off already */
            return None;
        }
        let Some(mac) = &self.mac else {
            let _ = reply.send(Err(
                "The hub doesn't know the TV's MAC address, so it can't switch it on. Pair it again while it's on."
                    .into(),
            ));
            return None;
        };
        if let Err(e) = net::wake_on_lan(mac, Some(split_host(host).0)).await {
            let _ = reply.send(Err(e));
            return None;
        }
        println!("lg-webos: {}: wake-on-LAN sent", self.id);
        Some(Waking {
            reply,
            until: Instant::now() + WAKE_LIMIT,
        })
    }

    /* A message from the TV: a subscription's answer or push. */
    fn apply(&mut self, message: &Value) {
        let payload = &message["payload"];
        match message["id"].as_str() {
            Some("power") => {
                if let Some(state) = payload["state"].as_str() {
                    self.tv.on = !matches!(state, "Power Off" | "Suspend" | "Active Standby");
                }
            }
            /* Newer TVs: {"volumeStatus": {"volume", "muteStatus"}};
             * older: {"volume", "muted"}. */
            Some("volume") => {
                let status = if payload["volumeStatus"].is_object() { &payload["volumeStatus"] } else { payload };
                if let Some(volume) = status["volume"].as_u64() {
                    self.tv.media.volume = volume.min(100) as u8;
                }
                if let Some(muted) = status["muteStatus"].as_bool().or(status["muted"].as_bool()) {
                    self.tv.media.muted = muted;
                }
            }
            Some("audio") => {
                if let Some(muted) = payload["mute"].as_bool() {
                    self.tv.media.muted = muted;
                }
                if let Some(volume) = payload["volume"].as_u64() {
                    self.tv.media.volume = volume.min(100) as u8;
                }
            }
            Some("inputs") => {
                if let Some(devices) = payload["devices"].as_array() {
                    self.tv.input_apps.clear();
                    self.tv.input_apps.insert(LIVE_TV_APP.into(), LIVE_TV.into());
                    let live_tv = MediaInput {
                        id: LIVE_TV.into(),
                        label: LIVE_TV_LABEL.into(),
                    };
                    self.tv.media.inputs = std::iter::once(live_tv)
                        .chain(devices.iter().filter_map(|d| {
                            let id = d["id"].as_str()?.to_string();
                            let label = d["label"].as_str().unwrap_or(&id).chars().take(64).collect();
                            if let Some(app) = d["appId"].as_str() {
                                self.tv.input_apps.insert(app.to_string(), id.clone());
                            }
                            Some(MediaInput { id, label })
                        }))
                        .take(32)
                        .collect();
                    self.update_input();
                }
            }
            Some("app") => {
                if let Some(app) = payload["appId"].as_str() {
                    self.tv.app = app.to_string();
                    if app != LIVE_TV_APP {
                        /* Followed afresh when Live TV comes back. */
                        self.tv.channel_follow = ChannelFollow::default();
                    }
                    self.update_input();
                }
            }
            Some("channel") => {
                self.tv.media.channel = channel_of(payload);
            }
            _ => {}
        }
    }

    /* The input on screen: the input whose app is in the foreground. */
    fn update_input(&mut self) {
        self.tv.media.input = self.tv.input_apps.get(&self.tv.app).cloned().unwrap_or_default();
        /* The app on screen, by name: an input's label, the TV's own name
         * for the app, else its id. */
        self.tv.media.app = (!self.tv.app.is_empty()).then(|| {
            let from_input = self.tv.media.inputs.iter().find(|i| i.id == self.tv.media.input).map(|i| i.label.clone());
            let label = from_input
                .or_else(|| self.tv.app_labels.get(&self.tv.app).cloned())
                .unwrap_or_else(|| self.tv.app.clone());
            MediaInput {
                id: self.tv.app.chars().take(64).collect(),
                label: label.chars().take(64).collect(),
            }
        });
        /* Only while watching TV. */
        if self.tv.app != LIVE_TV_APP {
            self.tv.media.channel = None;
        }
    }

    /* Media before power before online: a screen that sees "on" never
     * shows it with the volume from before (as in wled.rs). */
    async fn report(&mut self) {
        if let Err(e) = self.hub.report(&self.id, "media", json!(self.tv.media)).await {
            println!("lg-webos: {}: {e}", self.id);
        }
        if let Err(e) = self.hub.report(&self.id, "switch", json!(Switch { on: self.tv.on })).await {
            println!("lg-webos: {}: {e}", self.id);
        }
        self.hub.set_online(&self.id, Health::Online).await;
    }

    /* Not connected: off. Still "online" if the hub can wake it. */
    async fn report_off(&mut self) {
        self.tv.on = false;
        if let Err(e) = self.hub.report(&self.id, "switch", json!(Switch { on: false })).await {
            println!("lg-webos: {}: {e}", self.id);
        }
        let health = if self.mac.is_some() { Health::Online } else { Health::Offline };
        self.hub.set_online(&self.id, health).await;
    }

    /* Nothing to do until the device is re-paired (its task restarted) or
     * removed: answer commands with why. */
    async fn park(&mut self, health: Health, commands: &mut mpsc::Receiver<DeviceCmd>, why: &str) {
        self.hub.set_online(&self.id, health).await;
        while let Some(cmd) = commands.recv().await {
            cmd.refuse(format!("The TV can't be controlled: {why}."));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::lg_sim::TvSim;
    use crate::adapters::Registry;
    use crate::control::Control;
    use crate::device::Capabilities;
    use crate::secrets::Secrets;
    use crate::state::{self, Event, Outputs};
    use std::sync::Arc;
    use tokio::sync::{broadcast, watch};

    #[test]
    fn mac_comes_from_the_right_interface() {
        let info = json!({
            "wiredInfo": {"state": "disconnected", "macAddress": "AA:AA:AA:AA:AA:AA", "ipAddress": ""},
            "wifiInfo": {"state": "connected", "macAddress": "BB:BB:BB:BB:BB:BB", "ipAddress": "192.168.1.40"}
        });
        assert_eq!(mac_of(&info, "192.168.1.40").as_deref(), Some("bb:bb:bb:bb:bb:bb"));
        assert_eq!(mac_of(&info, "10.0.0.1").as_deref(), Some("bb:bb:bb:bb:bb:bb"));
        assert_eq!(mac_of(&json!({}), "x"), None);
    }

    fn values(sim: &TvSim) -> SetupValues {
        SetupValues {
            plain: [("host".to_string(), sim.host())].into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn pairs_with_the_prompt_and_pins_the_certificate() {
        let sim = TvSim::start().await;
        let mut v = values(&sim);
        let paired = LgWebos.action("pair", &v).await.unwrap();
        assert_eq!(paired.secret[KEY].expose(), TvSim::KEY);
        assert_eq!(paired.plain[CERT], sim.fingerprint());
        assert_eq!(paired.plain["mac"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(sim.prompts(), 1);

        /* The test step, as the paired client: no prompt. */
        v.plain.extend(paired.plain);
        v.secret.extend(paired.secret);
        let probe = LgWebos.probe(&v).await.unwrap();
        assert_eq!(probe.summary, "LG OLED55SIM");
        assert_eq!(sim.prompts(), 1);

        /* Another certificate (another device at that address): refused. */
        v.plain.insert(CERT.into(), "00".repeat(32));
        assert_eq!(LgWebos.probe(&v).await.unwrap_err().kind, ErrorKind::Refused);
    }

    #[tokio::test]
    async fn a_declined_prompt_is_not_confirmed() {
        let sim = TvSim::start().await;
        sim.decline_prompts();
        let err = LgWebos.action("pair", &values(&sim)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotConfirmed);
    }

    /* ---- the device's task, against the simulated TV ---- */

    struct TestHub {
        control: Control,
        events: broadcast::Receiver<Event>,
    }

    async fn hub_with_tv(sim: &TvSim, key: &str) -> TestHub {
        let tv = Device {
            id: "tv".into(),
            name: "TV".into(),
            room: String::new(),
            template: "lg-webos-tv".into(),
            source: crate::device::Source::new("lg-webos"),
            config: [
                ("host".to_string(), sim.host()),
                ("mac".to_string(), "aa:bb:cc:dd:ee:ff".to_string()),
                (CERT.to_string(), sim.fingerprint()),
            ]
            .into(),
            identity: String::new(),
            online: None,
            capabilities: Capabilities::with_defaults(&["switch".into(), "media".into(), "remote".into()]).unwrap(),
        };
        let (state_tx, state_rx) = mpsc::channel(8);
        let (events_tx, events) = broadcast::channel(256);
        let outputs = Outputs {
            changed_tx: watch::channel(()).0,
            events_tx,
            save_tx: watch::channel(Vec::new()).0,
        };
        tokio::spawn(state::run(state_rx, [("tv".to_string(), tv)].into(), outputs));
        let registry = Arc::new(Registry::new(vec![Box::new(LgWebos)]));
        let secrets = Arc::new(Secrets::new(
            [("tv".to_string(), [(KEY.to_string(), Secret::new(key))].into())].into(),
            watch::channel(Vec::new()).0,
        ));
        let control = Control::new(state_tx, registry.clone(), secrets);
        registry.start_all(&control).await;
        TestHub { control, events }
    }

    async fn until(hub: &mut TestHub, wanted: impl Fn(&Device) -> bool) -> Device {
        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        if let Some(d) = hub.control.get("tv").await.unwrap() {
            if wanted(&d) {
                return d;
            }
        }
        loop {
            tokio::select! {
                _ = &mut deadline => panic!("timed out; tv is {:?}", hub.control.get("tv").await),
                event = hub.events.recv() => if let Ok(Event::Changed(d)) = event {
                    if wanted(&d) { return d }
                },
            }
        }
    }

    #[tokio::test]
    async fn follows_and_controls_the_tv() {
        let sim = TvSim::start().await;
        let mut hub = hub_with_tv(&sim, TvSim::KEY).await;

        let d = until(&mut hub, |d| {
            d.online == Some(Health::Online) && d.capabilities.switch.as_ref().is_some_and(|s| s.on)
        })
        .await;
        let media = d.capabilities.media.unwrap();
        assert_eq!(media.volume, 12);
        assert_eq!(media.input, "HDMI_1");
        /* Live TV first, then the TV's own inputs. */
        let ids: Vec<&str> = media.inputs.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["TV", "HDMI_1", "HDMI_2"]);

        /* Volume, mute, input: through the TV, confirmed. */
        let d = hub
            .control
            .command("tv", "media", json!({"volume": 30, "muted": true, "input": "HDMI_2"}))
            .await
            .unwrap();
        let media = d.capabilities.media.unwrap();
        assert_eq!((media.volume, media.muted, media.input.as_str()), (30, true, "HDMI_2"));
        assert_eq!(sim.volume(), 30);

        /* Back to TV channels: the Live TV app is launched. */
        let d = hub
            .control
            .command("tv", "media", json!({"volume": 30, "muted": true, "input": "TV"}))
            .await
            .unwrap();
        assert_eq!(d.capabilities.media.unwrap().input, "TV");
        assert_eq!(sim.app(), "com.webos.app.livetv");

        /* The remote changes the volume: pushed, reported. */
        sim.set_volume_from_remote(7);
        until(&mut hub, |d| d.capabilities.media.as_ref().is_some_and(|m| m.volume == 7)).await;

        /* An input it doesn't have: refused, nothing sent. */
        assert!(hub
            .control
            .command("tv", "media", json!({"volume": 7, "muted": true, "input": "HDMI_9"}))
            .await
            .is_err());

        /* Off: the TV goes away, the device shows off but stays online
         * (it can be woken). Media commands are refused while off. */
        hub.control.command("tv", "switch", json!({"on": false})).await.unwrap();
        until(&mut hub, |d| d.capabilities.switch.as_ref().is_some_and(|s| !s.on)).await;
        assert!(sim.is_off());
        let err = hub
            .control
            .command("tv", "media", json!({"volume": 5, "muted": false, "input": ""}))
            .await
            .unwrap_err();
        assert!(err.contains("off"), "{err}");
        assert_eq!(hub.control.get("tv").await.unwrap().unwrap().online, Some(Health::Online));

        /* On: the sim plays "woken" (a real TV needs the magic packet); the
         * command's answer waits until it's connected again. */
        sim.wake_in(Duration::from_millis(300));
        let d = hub.control.command("tv", "switch", json!({"on": true})).await.unwrap();
        assert!(d.capabilities.switch.unwrap().on);
    }

    /* Against a real TV, WITHOUT pairing (no prompt): the TLS connection
     * and the TV's "hello". Not run by default:
     *   LG_HOST=192.168.1.140 cargo test lg_webos::tests::live_hello -- --ignored --nocapture */
    #[tokio::test]
    #[ignore]
    async fn live_hello() {
        let host = std::env::var("LG_HOST").expect("set LG_HOST");
        let (mut conn, fingerprint) = Conn::open(&host, None, true).await.unwrap();
        println!("certificate: {fingerprint:?}");
        conn.send(json!({"type": "hello", "id": "hello", "payload": {}})).await.unwrap();
        println!("{}", conn.recv().await.unwrap());
    }

    /* Issue #44: the remote (buttons over the pointer socket), the
     * keyboard, apps and channels -- and the rules checked before any of
     * it reaches the TV. */
    #[tokio::test]
    async fn remote_keyboard_apps_and_channels() {
        let sim = TvSim::start().await;
        let mut hub = hub_with_tv(&sim, TvSim::KEY).await;
        let d = until(&mut hub, |d| {
            d.online == Some(Health::Online) && d.capabilities.remote.as_ref().is_some_and(|r| !r.buttons.is_empty())
        })
        .await;
        assert!(d.capabilities.remote.unwrap().keyboard);
        let control = hub.control.clone();
        let act = |capability: &'static str, name: &'static str, args: Value| {
            let control = control.clone();
            async move { control.action("tv", capability, name, args).await }
        };

        /* Buttons, in the hub's names, arrive in webOS's. */
        act("remote", "press", json!({"button": "UP"})).await.unwrap();
        act("remote", "press", json!({"button": "OK"})).await.unwrap();
        act("remote", "press", json!({"button": "CHANNEL_UP"})).await.unwrap();
        /* Sent is not yet received: the sim reads them on its own task. */
        for _ in 0..50 {
            if sim.presses().len() == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(sim.presses(), ["UP", "ENTER", "CHANNELUP"]);
        /* Refused before the TV sees it. */
        assert!(act("remote", "press", json!({"button": "SELF_DESTRUCT"})).await.is_err());
        assert!(act("remote", "fly", json!({})).await.is_err());

        /* The keyboard. */
        act("remote", "type", json!({"text": "netflixx"})).await.unwrap();
        act("remote", "delete", json!({"count": 1})).await.unwrap();
        act("remote", "submit", json!({})).await.unwrap();
        assert_eq!(sim.typed(), ("netflix".to_string(), 1));

        /* Apps: listed, launched, shown by name. */
        let apps = act("media", "apps", json!({})).await.unwrap();
        assert_eq!(apps["apps"][0], json!({"id": "netflix", "label": "Netflix"}));
        act("media", "launch", json!({"app": "netflix"})).await.unwrap();
        until(&mut hub, |d| d.capabilities.media.as_ref().and_then(|m| m.app.as_ref()).is_some_and(|a| a.label == "Netflix")).await;

        /* Channels: only while Live TV is on; listed, tuned, followed. */
        hub.control.command("tv", "media", json!({"volume": 12, "muted": false, "input": "TV"})).await.unwrap();
        let d = until(&mut hub, |d| d.capabilities.media.as_ref().is_some_and(|m| m.channel.is_some())).await;
        assert_eq!(d.capabilities.media.unwrap().channel.unwrap().name, "TVR 1");
        /* A list bigger than 256 KB (seen on the real TV): it arrives, and
         * the connection survives it. The screen gets one page of it, and
         * how many there are. */
        let channels = act("media", "channels", json!({})).await.unwrap();
        assert_eq!(channels["channels"][1], json!({"id": "ch-5", "number": "5", "name": "Pro TV"}));
        assert_eq!(channels["channels"].as_array().unwrap().len(), 100);
        assert_eq!(channels["total"], 3003);
        assert_eq!(sim.connections(), (2, 0), "main + button socket, none dropped");
        /* Search and further pages, from the list the hub keeps. */
        let found = act("media", "channels", json!({"query": "antena"})).await.unwrap();
        assert_eq!(found["channels"], json!([{"id": "ch-7", "number": "7", "name": "Antena 1"}]));
        let page = act("media", "channels", json!({"offset": 3000, "limit": 100})).await.unwrap();
        assert_eq!(page["channels"].as_array().unwrap().len(), 3);
        assert!(act("media", "channels", json!({"limit": 0})).await.is_err());
        act("media", "tune", json!({"channel": "ch-5"})).await.unwrap();
        until(&mut hub, |d| {
            d.capabilities.media.as_ref().and_then(|m| m.channel.as_ref()).is_some_and(|c| c.number == "5")
        })
        .await;
        /* Leaving Live TV: no channel any more. */
        hub.control.command("tv", "media", json!({"volume": 12, "muted": false, "input": "HDMI_2"})).await.unwrap();
        until(&mut hub, |d| d.capabilities.media.as_ref().is_some_and(|m| m.channel.is_none())).await;

        /* Off: actions are refused with the reason. */
        hub.control.command("tv", "switch", json!({"on": false})).await.unwrap();
        until(&mut hub, |d| d.capabilities.switch.as_ref().is_some_and(|s| !s.on)).await;
        let err = act("remote", "press", json!({"button": "UP"})).await.unwrap_err();
        assert!(err.contains("off"), "{err}");
    }

    /* After a reconnect (here: off, then on) the channel is still followed.
     * Regressions, both seen on the real TV: the "already subscribed" mark
     * outlived the connection (the channel froze); and right after waking
     * the TV refuses the channel, which was never asked for again. */
    #[tokio::test]
    async fn channel_is_followed_after_a_reconnect() {
        let sim = TvSim::start().await;
        let mut hub = hub_with_tv(&sim, TvSim::KEY).await;
        until(&mut hub, |d| d.online == Some(Health::Online)).await;
        hub.control.command("tv", "media", json!({"volume": 12, "muted": false, "input": "TV"})).await.unwrap();
        until(&mut hub, |d| d.capabilities.media.as_ref().is_some_and(|m| m.channel.is_some())).await;

        hub.control.command("tv", "switch", json!({"on": false})).await.unwrap();
        until(&mut hub, |d| d.capabilities.switch.as_ref().is_some_and(|s| !s.on)).await;
        sim.wake_in(Duration::from_millis(100));
        hub.control.command("tv", "switch", json!({"on": true})).await.unwrap();

        /* Still on Live TV after waking. The TV refused the channel while
         * waking up (the sim too): it comes once it's ready -- by itself. */
        until(&mut hub, |d| {
            d.capabilities.media.as_ref().and_then(|m| m.channel.as_ref()).is_some_and(|c| c.name == "TVR 1")
        })
        .await;
        /* And changing the channel shows. */
        hub.control.action("tv", "media", "tune", json!({"channel": "ch-7"})).await.unwrap();
        until(&mut hub, |d| {
            d.capabilities.media.as_ref().and_then(|m| m.channel.as_ref()).is_some_and(|c| c.name == "Antena 1")
        })
        .await;
    }

    /* A quiet TV (nothing changes, so it sends nothing) still answers the
     * keep-alive pings: the connection must stay up. Regression: pongs
     * weren't counted, and the real TV was dropped every 60 s. */
    #[tokio::test]
    async fn a_quiet_tv_stays_connected() {
        let sim = TvSim::start().await;
        let mut hub = hub_with_tv(&sim, TvSim::KEY).await;
        until(&mut hub, |d| d.online == Some(Health::Online)).await;
        /* Many times SILENT_LIMIT, in silence. */
        tokio::time::sleep(SILENT_LIMIT * 6).await;
        assert_eq!(sim.connections(), (1, 0), "dropped: the quiet TV was taken for dead");
    }

    #[tokio::test]
    async fn a_revoked_key_makes_it_unauthorized_without_prompting() {
        let sim = TvSim::start().await;
        let mut hub = hub_with_tv(&sim, "an-old-key").await;
        until(&mut hub, |d| d.online == Some(Health::Unauthorized)).await;
        /* No retry storm: one attempt, then parked. */
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(sim.prompts(), 1);
        let err = hub.control.command("tv", "switch", json!({"on": false})).await.unwrap_err();
        assert!(err.contains("paired again"), "{err}");
    }
}
