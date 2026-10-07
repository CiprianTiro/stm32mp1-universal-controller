/*
 * roborock.rs -- Roborock robot vacuums (issue #74), capability `vacuum`.
 *
 * TWO PROTOCOLS, one device type -- which one a vacuum speaks depends on
 * the app it was set up with, and the template asks (a "choice"):
 *   "tcp"   the Roborock app: Roborock's own protocol, TCP 58867
 *           (roborock_proto.rs); its key, `local_key`, only comes from
 *           the Roborock account (roborock_cloud.rs) -- logged in ONCE in
 *           the wizard, then never again (pattern P4);
 *   "miio"  Xiaomi's Mi Home app: miIO, UDP 54321 (miio.rs), with a
 *           `token` typed in (fetching it from Xiaomi's cloud: follow-up).
 * Either way the hub then talks to the vacuum on the LAN only: it keeps
 * working without internet.
 *
 * THE TASK keeps a connection (tcp) and asks "get_status" every POLL_IDLE,
 * or POLL_BUSY while it cleans or drives home; a tcp vacuum also pushes
 * its state and battery the moment they change. Actions are the vacuum's
 * own commands: start = app_start, pause = app_pause, stop = app_stop,
 * dock = app_charge, locate = find_me.
 *
 * THE KEY CAN CHANGE: a vacuum reset and paired with the app again gets a
 * new local_key. It still lets us connect, but no longer answers -- the
 * task then reports "unauthorized", and "Pair again" logs in to the
 * account once more (the template's `reauth`).
 *
 * SETUP ACTIONS (the template's vendor_login step):
 *   send_code     email -> Roborock emails a code (login_* values)
 *   login         + code -> a session (secret login_session)
 *   list_devices  the account's vacuums, each with its duid and local_key
 * The wizard forgets the login_* values afterwards and saves the account
 * (accounts.rs: the session, for the next vacuum and the map); the vacuum
 * keeps its duid and "account" (plain) and local_key (secret). Its address comes from its
 * broadcasts (discovery.rs, "udp_listen").
 */
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::roborock_cloud::{self, Pending, Session};
use super::roborock_proto::{self as proto, Decoder, Message, Payload};
use super::{miio, Adapter, BoxFuture, CloudDevice, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Device, Health, VacuumState};
use crate::secrets::Secret;
use crate::templates::ErrorKind;

/* While it cleans or drives home: progress shows quickly. */
const POLL_BUSY: Duration = Duration::from_secs(5);
/* Docked or idle: nothing changes, pushes (tcp) report what does. */
const POLL_IDLE: Duration = Duration::from_secs(30);
/* A request's answer; the vacuum answers in well under a second. */
const ANSWER_WAIT: Duration = Duration::from_secs(5);
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(120);
/* Connected, but this many requests in a row unanswered: the key no
 * longer fits (see the header). */
const UNANSWERED_LIMIT: u32 = 2;

pub struct Roborock;

impl Adapter for Roborock {
    fn id(&self) -> &'static str {
        "roborock"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let secrets = hub.secrets(&device.id);
        /* Only while the person keeps it on ("Pair again" can turn it off;
         * a session from before is then ignored). */
        let map_on = device.config.get("map").is_some_and(|m| m == "true");
        /* The account's session (Hub::cloud_session). */
        let session = hub.cloud_session(device);
        let map = match (
            session.filter(|_| map_on).and_then(|s| serde_json::from_str::<Session>(&s).ok()),
            device.config.get("duid"),
            secrets.get("local_key"),
        ) {
            (Some(session), Some(duid), Some(key)) => Some(std::sync::Arc::new(MapSource {
                session,
                account: device.config.get("account").cloned(),
                hub: hub.clone(),
                duid: duid.clone(),
                local_key: key.expose().to_string(),
                state: tokio::sync::Mutex::new(MapState::default()),
            })),
            _ => None,
        };
        let task = Task {
            id: device.id.clone(),
            settings: Settings::from(&device.config, |name| secrets.get(name).map(|s| s.expose().to_string())),
            config: device.config.clone(),
            map,
            hub,
        };
        tokio::spawn(task.run(commands_rx));
        DeviceHandle::new(commands)
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(probe(values))
    }

    fn action<'a>(&'a self, name: &'a str, values: &'a SetupValues) -> BoxFuture<'a, Result<SetupValues, SetupError>> {
        Box::pin(action(name, values))
    }
}

/* How to reach one vacuum. */
#[derive(Clone, Debug, PartialEq)]
enum Settings {
    Tcp { host: String, local_key: String },
    Miio { host: String, token: [u8; 16] },
    /* What's missing, for the log and the error. */
    Incomplete(String),
}

impl Settings {
    fn from(plain: &std::collections::BTreeMap<String, String>, secret: impl Fn(&str) -> Option<String>) -> Settings {
        let Some(host) = plain.get("host").filter(|h| !h.is_empty()).cloned() else {
            return Settings::Incomplete("no address (the vacuum wasn't seen on this network)".into());
        };
        match plain.get("protocol").map(String::as_str).unwrap_or("tcp") {
            "miio" => match secret("token").as_deref().and_then(miio::parse_token) {
                Some(token) => Settings::Miio { host, token },
                None => Settings::Incomplete("no token (32 hex digits)".into()),
            },
            _ => match secret("local_key").filter(|k| !k.is_empty()) {
                Some(local_key) => Settings::Tcp { host, local_key },
                None => Settings::Incomplete("no local key: log in to the Roborock account".into()),
            },
        }
    }
}

/* ------------------------------------------------------------------ */
/* The connection                                                      */
/* ------------------------------------------------------------------ */

/* Why a request failed: the kind decides what the task reports. */
#[derive(Debug)]
enum Failure {
    /* Can't connect, connection lost: offline. */
    Unreachable(String),
    /* Connected, but no answer: offline for now, "unauthorized" if it
     * lasts (UNANSWERED_LIMIT). */
    Unanswered(String),
    /* The vacuum said no ("invalid status": e.g. "dock" while docked). */
    Refused(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Unreachable(e) | Failure::Unanswered(e) | Failure::Refused(e) => f.write_str(e),
        }
    }
}

/* A connection to a tcp vacuum. */
struct TcpLink {
    stream: TcpStream,
    decoder: Decoder,
    local_key: String,
    seq: u32,
    /* Pushes that arrived while waiting for an answer. */
    pushed: Vec<(Option<i64>, Option<i64>)>,
}

fn unix_now() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32)
}

fn random_u32(range: std::ops::Range<u32>) -> u32 {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 4];
    let _ = ring::rand::SystemRandom::new().fill(&mut bytes);
    range.start + u32::from_be_bytes(bytes) % (range.end - range.start)
}

impl TcpLink {
    async fn connect(host: &str, local_key: &str) -> Result<TcpLink, Failure> {
        let target = if host.contains(':') { host.to_string() } else { format!("{host}:{}", proto::LOCAL_PORT) };
        let stream = tokio::time::timeout(ANSWER_WAIT, TcpStream::connect(&target))
            .await
            .map_err(|_| Failure::Unreachable(format!("{host} isn't answering")))?
            .map_err(|e| Failure::Unreachable(format!("can't reach {host}: {e}")))?;
        let mut link = TcpLink {
            stream,
            decoder: Decoder::default(),
            local_key: local_key.to_string(),
            seq: 1,
            pushed: Vec::new(),
        };
        /* "Hello": the vacuum answers if it speaks "1.0". */
        let hello = Message {
            seq: 1,
            random: random_u32(10_000..32_767),
            timestamp: unix_now(),
            protocol: proto::HELLO_REQUEST,
            payload: Vec::new(),
        };
        link.send(&hello).await?;
        let deadline = tokio::time::Instant::now() + ANSWER_WAIT;
        loop {
            let message = tokio::time::timeout_at(deadline, link.receive())
                .await
                .map_err(|_| Failure::Unreachable(format!("{host} doesn't speak Roborock's \"1.0\" protocol")))??;
            if message.protocol == proto::HELLO_RESPONSE {
                return Ok(link);
            }
        }
    }

    async fn send(&mut self, message: &Message) -> Result<(), Failure> {
        self.stream
            .write_all(&proto::encode(message, &self.local_key))
            .await
            .map_err(|e| Failure::Unreachable(format!("connection lost: {e}")))
    }

    /* The next message. Cancel-safe (a select! may drop it): what was
     * read is kept in the decoder. */
    async fn receive(&mut self) -> Result<Message, Failure> {
        let mut buf = [0u8; 4096];
        loop {
            match self.decoder.next(&self.local_key) {
                Some(Ok(message)) => return Ok(message),
                /* Can't be read: most likely the key doesn't fit. */
                Some(Err(e)) => return Err(Failure::Unanswered(format!("unreadable answer: {e}"))),
                None => {}
            }
            let n = self
                .stream
                .read(&mut buf)
                .await
                .map_err(|e| Failure::Unreachable(format!("connection lost: {e}")))?;
            if n == 0 {
                return Err(Failure::Unreachable("the vacuum closed the connection".into()));
            }
            self.decoder.push(&buf[..n]);
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        let id = random_u32(10_000..32_767);
        self.seq = self.seq.wrapping_add(1);
        let timestamp = unix_now();
        let request = Message {
            seq: self.seq,
            random: random_u32(10_000..99_999),
            timestamp,
            protocol: proto::GENERAL,
            payload: proto::request_payload(id, method, params, timestamp),
        };
        self.send(&request).await?;
        let deadline = tokio::time::Instant::now() + ANSWER_WAIT;
        loop {
            let message = tokio::time::timeout_at(deadline, self.receive())
                .await
                .map_err(|_| Failure::Unanswered(format!("no answer to {method}")))??;
            match proto::parse_payload(&message.payload) {
                Payload::Reply { id: got, result } if got == id => return result.map_err(Failure::Refused),
                Payload::Push { state, battery } => self.pushed.push((state, battery)),
                _ => {}
            }
        }
    }
}

/* Either protocol, as the task uses it. */
enum Link {
    Tcp(TcpLink),
    Miio { host: String, token: [u8; 16] },
}

impl Link {
    async fn open(settings: &Settings) -> Result<Link, Failure> {
        match settings {
            Settings::Tcp { host, local_key } => Ok(Link::Tcp(TcpLink::connect(host, local_key).await?)),
            Settings::Miio { host, token } => Ok(Link::Miio { host: host.clone(), token: *token }),
            Settings::Incomplete(why) => Err(Failure::Refused(why.clone())),
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        match self {
            Link::Tcp(link) => link.call(method, params).await,
            Link::Miio { host, token } => miio::call(host, token, random_u32(1..9999), method, params)
                .await
                .map_err(|e| if e.contains("isn't answering") { Failure::Unreachable(e) } else { Failure::Unanswered(e) }),
        }
    }

    /* The next push (tcp only; miIO vacuums don't push). */
    async fn next_push(&mut self) -> Result<(Option<i64>, Option<i64>), Failure> {
        match self {
            Link::Tcp(link) => loop {
                if let Some(push) = link.pushed.pop() {
                    return Ok(push);
                }
                let message = link.receive().await?;
                if let Payload::Push { state, battery } = proto::parse_payload(&message.payload) {
                    return Ok((state, battery));
                }
            },
            Link::Miio { .. } => std::future::pending().await,
        }
    }
}

/* get_status -> the `vacuum` capability's value. */
fn vacuum_state(status: &Value) -> Result<Value, String> {
    /* An answer is a list with one object (both protocols). */
    let status = status.get(0).unwrap_or(status);
    let code = status["state"]
        .as_i64()
        .ok_or("the vacuum's status has no state: not a vacuum, or a protocol change")?;
    let (state, detail) = proto::state(code);
    let error = proto::error_text(status["error_code"].as_i64().unwrap_or(0));
    let state = if !error.is_empty() && state != VacuumState::Docked { VacuumState::Error } else { state };
    let mut value = json!({
        "state": state,
        "battery": status["battery"].as_u64().map(|b| b.min(100)),
        "detail": detail,
        "error": error,
    });
    /* Suction, water, mop route: the current one and the choices (only
     * what this vacuum reports; water only with the water tank in). */
    if let Some(code) = status["fan_power"].as_i64() {
        let mut fans: Vec<&str> = FANS.iter().filter(|(c, _)| *c <= 105).map(|(_, n)| *n).collect();
        if code == 108 {
            fans.push("max+");
        }
        value["fan"] = json!(mode_name(FANS, code));
        value["fans"] = json!(fans);
    }
    if let (Some(code), Some(1)) = (status["water_box_mode"].as_i64(), status["water_box_status"].as_i64()) {
        value["water"] = json!(mode_name(WATERS, code));
        value["waters"] = json!(WATERS.iter().filter(|(c, _)| *c <= 203).map(|(_, n)| *n).collect::<Vec<_>>());
    }
    if let Some(code) = status["mop_mode"].as_i64() {
        let mut mops = vec!["standard", "deep"];
        if code == 303 {
            mops.push("deep+");
        }
        value["mop"] = json!(mode_name(MOPS, code));
        value["mops"] = json!(mops);
    }
    /* This (or the last) run: mm² and seconds. */
    if let Some(area) = status["clean_area"].as_f64() {
        value["area_m2"] = json!((area / 10_000.0).round() / 100.0);
    }
    if let Some(seconds) = status["clean_time"].as_u64() {
        value["minutes"] = json!(seconds / 60);
    }
    Ok(value)
}

/* Roborock's mode codes -> the hub's words (and back, for the actions). */
const FANS: &[(i64, &str)] = &[(105, "off"), (101, "quiet"), (102, "balanced"), (103, "turbo"), (104, "max"), (108, "max+"), (106, "custom")];
const WATERS: &[(i64, &str)] = &[(200, "off"), (201, "low"), (202, "medium"), (203, "high"), (204, "custom"), (207, "custom")];
const MOPS: &[(i64, &str)] = &[(300, "standard"), (301, "deep"), (303, "deep+"), (302, "custom")];

fn mode_name(table: &[(i64, &'static str)], code: i64) -> &'static str {
    table.iter().find(|(c, _)| *c == code).map_or("custom", |(_, n)| n)
}

fn mode_code(table: &[(i64, &str)], name: &str) -> Option<i64> {
    table.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

/* The wearing parts: Roborock's counter, the hub's id and name, and the
 * life Roborock gives each (hours). */
const PARTS: &[(&str, &str, &str, f64)] = &[
    ("main_brush_work_time", "main_brush", "Main brush", 300.0),
    ("side_brush_work_time", "side_brush", "Side brush", 200.0),
    ("filter_work_time", "filter", "Filter", 150.0),
    ("sensor_dirty_time", "sensors", "Sensors (clean them)", 30.0),
];

/* get_consumable -> parts with their life left (percent). */
fn parts(consumable: &Value) -> Value {
    let used = consumable.get(0).unwrap_or(consumable);
    let list: Vec<Value> = PARTS
        .iter()
        .filter_map(|(key, id, name, hours)| {
            let seconds = used[*key].as_f64()?;
            let left = (100.0 - seconds / 3600.0 / hours * 100.0).clamp(0.0, 100.0).floor();
            Some(json!({"id": id, "name": name, "left": left as u8}))
        })
        .collect();
    json!(list)
}

/* get_clean_summary -> totals. Newer firmware answers an object, older a
 * list [seconds, mm², count, records]. */
fn totals(summary: &Value) -> Option<Value> {
    let (seconds, area, count) = match summary {
        Value::Object(o) => (o.get("clean_time")?.as_f64()?, o.get("clean_area")?.as_f64()?, o.get("clean_count")?.as_u64()?),
        Value::Array(a) => (a.first()?.as_f64()?, a.get(1)?.as_f64()?, a.get(2)?.as_u64()?),
        _ => return None,
    };
    Some(json!({
        "cleanings": count,
        "area_m2": (area / 1_000_000.0 * 10.0).round() / 10.0,
        "hours": (seconds / 3600.0 * 10.0).round() / 10.0,
    }))
}

/* get_dnd_timer -> "22:00-08:00", or null when off. */
fn quiet_hours(dnd: &Value) -> Value {
    let dnd = dnd.get(0).unwrap_or(dnd);
    if dnd["enabled"].as_i64() != Some(1) {
        return Value::Null;
    }
    let hm = |h: &str, m: &str| format!("{:02}:{:02}", dnd[h].as_i64().unwrap_or(0), dnd[m].as_i64().unwrap_or(0));
    json!(format!("{}-{}", hm("start_hour", "start_minute"), hm("end_hour", "end_minute")))
}

/* get_room_mapping ([[16, "27906924", ...], ...]: the vacuum's room number
 * and the cloud's room id) -> rooms, named with the names fetched at
 * setup (config "room_<cloud id>"). */
fn rooms(mapping: &Value, config: &std::collections::BTreeMap<String, String>) -> Value {
    let list: Vec<Value> = mapping
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get(0)?.as_u64()?;
            let cloud = entry.get(1).map(|c| c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string()))?;
            let name = config.get(&format!("room_{cloud}")).cloned().unwrap_or_else(|| format!("Room {id}"));
            Some(json!({"id": id, "name": name}))
        })
        .collect();
    json!(list)
}

/* ------------------------------------------------------------------ */
/* The task                                                            */
/* ------------------------------------------------------------------ */

/* The slow part of the state (see EXTRAS_EVERY). */
#[derive(Default)]
struct Extras {
    values: Vec<(&'static str, Value)>,
    read_at: Option<std::time::Instant>,
}

impl Extras {
    async fn read(&mut self, link: &mut Link, config: &std::collections::BTreeMap<String, String>) -> Result<(), Failure> {
        let mut values = Vec::new();
        /* A command an older vacuum doesn't know is skipped, not fatal. */
        let mut ask = async |method: &str| match link.call(method, json!([])).await {
            Ok(v) => Ok(Some(v)),
            Err(Failure::Refused(_)) => Ok(None),
            Err(e) => Err(e),
        };
        if let Some(v) = ask("get_consumable").await? {
            values.push(("parts", parts(&v)));
        }
        if let Some(t) = ask("get_clean_summary").await?.as_ref().and_then(totals) {
            values.push(("totals", t));
        }
        if let Some(v) = ask("get_dnd_timer").await? {
            values.push(("quiet_hours", quiet_hours(&v)));
        }
        if let Some(v) = ask("get_room_mapping").await? {
            values.push(("rooms", rooms(&v, config)));
        }
        self.values = values;
        self.read_at = Some(std::time::Instant::now());
        Ok(())
    }
}

struct Task {
    id: String,
    settings: Settings,
    /* For the room names (config "room_<id>"). */
    config: std::collections::BTreeMap<String, String>,
    /* The map (opt-in): the Roborock session it's fetched with, and the
     * last one fetched (a screen asking twice in a row gets it again). */
    map: Option<std::sync::Arc<MapSource>>,
    hub: Hub,
}

/* Where a vacuum's map comes from (roborock_map.rs). */
struct MapSource {
    /* The session at start: used only if the vacuum has no account. */
    session: Session,
    /* The account it belongs to: its session is read again for every
     * fetch, because each new Roborock login hands out new keys and makes
     * the old ones useless (Roborock's broker then refuses the hub). */
    account: Option<String>,
    hub: Hub,
    duid: String,
    local_key: String,
    state: tokio::sync::Mutex<MapState>,
}

#[derive(Default)]
struct MapState {
    /* The last map, and when it came. */
    last: Option<(std::time::Instant, Value)>,
    /* The broker connection, kept between maps (roborock_map.rs). */
    connection: Option<crate::adapters::roborock_map::Connection>,
    /* After a refusal: not before this, with this session (fingerprint),
     * and how long the next wait is. */
    refused: Option<(std::time::Instant, String)>,
    wait: Duration,
}

/* After Roborock's broker refuses the hub (usually its rate limit): the
 * first wait, doubled every refusal up to the last (python-roborock goes
 * up to 6 h too). A new login (another session) doesn't wait. */
const REFUSED_FIRST: Duration = Duration::from_secs(15 * 60);
const REFUSED_MAX: Duration = Duration::from_secs(6 * 3600);

/* A map fetched this recently is answered again (two screens, a quick
 * second tap): Roborock's cloud is asked at most this often. */
const MAP_FRESH: Duration = Duration::from_secs(8);

impl MapSource {
    async fn get(&self) -> Result<Value, String> {
        let mut state = self.state.lock().await;
        if let Some((at, map)) = state.last.as_ref() {
            if at.elapsed() < MAP_FRESH {
                return Ok(map.clone());
            }
        }
        let from_account = self
            .account
            .as_ref()
            .and_then(|a| self.hub.account_session(a))
            .and_then(|s| serde_json::from_str::<Session>(&s).ok());
        let source = match (&self.account, &from_account) {
            (Some(a), Some(_)) => format!("account {a}"),
            (Some(a), None) => format!("the vacuum's own copy (account {a} not found)"),
            (None, _) => "the vacuum's own copy (no account)".to_string(),
        };
        let session = from_account.unwrap_or_else(|| self.session.clone());

        /* Refused lately with this session: don't knock again yet. */
        if let Some((until, fingerprint)) = &state.refused {
            if *fingerprint == session.fingerprint() && std::time::Instant::now() < *until {
                let minutes = until.saturating_duration_since(std::time::Instant::now()).as_secs().div_ceil(60);
                return Err(format!("Roborock's cloud is refusing the hub for now; trying again in {minutes} min"));
            }
        }

        /* Over the kept connection; if that one went stale, once more on a
         * new one. A new connection that fails isn't tried again. */
        let mut result = Err(crate::adapters::roborock_map::FetchError { refused: false, text: String::new() });
        for _ in 0..2 {
            let reused = state.connection.as_ref().is_some_and(|c| c.usable(&session));
            if !reused {
                /* Logged: there should be one per visit to the vacuum's
                 * page, not one per map. */
                println!("roborock: map: new connection to Roborock's cloud (session {} from {source})", session.fingerprint());
                state.connection = Some(crate::adapters::roborock_map::Connection::new(&session).map_err(|e| e.text)?);
            }
            let connection = state.connection.as_mut().unwrap();
            result = connection.ask(&session, &self.duid, &self.local_key).await;
            if result.is_err() {
                state.connection = None;
            }
            match &result {
                Err(e) if reused && !e.refused => continue,
                _ => break,
            }
        }
        let raw = match result {
            Ok(raw) => {
                state.refused = None;
                state.wait = Duration::ZERO;
                raw
            }
            Err(e) => {
                /* Why: the whole session dead (Roborock's normal API
                 * refuses it too), or only the broker (its rate limit)? */
                let mut text = e.text.clone();
                if e.refused {
                    let api = crate::adapters::roborock_cloud::devices(&session).await;
                    state.wait = if state.wait.is_zero() { REFUSED_FIRST } else { (state.wait * 2).min(REFUSED_MAX) };
                    state.refused = Some((std::time::Instant::now() + state.wait, session.fingerprint()));
                    let minutes = state.wait.as_secs() / 60;
                    text = match &api {
                        Ok(_) => format!("Roborock's cloud is limiting the hub's connections; trying again in {minutes} min"),
                        Err(_) => "Roborock ended the hub's session: log in to the Roborock account again".to_string(),
                    };
                    println!(
                        "roborock: map refused with session {} from {source}, {}: {e}; the account API {}; next try in {minutes} min",
                        session.fingerprint(),
                        session.age(),
                        match &api {
                            Ok(_) => "still accepts it (a rate limit)".to_string(),
                            Err(e) => format!("refuses it too ({})", e.detail),
                        },
                    );
                } else {
                    println!("roborock: map failed with session {} from {source}: {e}", session.fingerprint());
                }
                return Err(text);
            }
        };
        let map = crate::adapters::roborock_map::parse(&raw)?.to_json();
        state.last = Some((std::time::Instant::now(), map.clone()));
        Ok(map)
    }
}

/* What changes slowly -- parts, totals, quiet hours, rooms: read every
 * EXTRAS_EVERY (and after a change), merged into every report. */
const EXTRAS_EVERY: Duration = Duration::from_secs(600);

impl Task {
    async fn run(self, mut commands: mpsc::Receiver<DeviceCmd>) {
        if let Settings::Incomplete(why) = &self.settings {
            println!("roborock: {}: {why}", self.id);
            self.hub.set_online(&self.id, Health::Offline).await;
            while let Some(cmd) = commands.recv().await {
                cmd.refuse(format!("{}: {why}", self.id));
            }
            return;
        }
        let mut retry = RETRY_MIN;
        let mut unanswered = 0;
        let mut logged_down = false;
        loop {
            let mut link = match Link::open(&self.settings).await {
                Ok(link) => link,
                Err(e) => {
                    if !logged_down {
                        println!("roborock: {}: {e} (will keep trying)", self.id);
                        logged_down = true;
                    }
                    self.hub.set_online(&self.id, Health::Offline).await;
                    if !self.wait_refusing(&mut commands, retry).await {
                        return;
                    }
                    retry = (retry * 2).min(RETRY_MAX);
                    continue;
                }
            };
            /* Connected: poll, push, commands -- until something breaks. */
            let mut wait = Duration::ZERO;
            let mut extras = Extras::default();
            let failure = loop {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {
                        match self.poll(&mut link, &mut extras).await {
                            Ok(busy) => {
                                if logged_down {
                                    println!("roborock: {} answers again", self.id);
                                    logged_down = false;
                                }
                                unanswered = 0;
                                retry = RETRY_MIN;
                                wait = if busy { POLL_BUSY } else { POLL_IDLE };
                            }
                            Err(e) => break e,
                        }
                    }
                    push = link.next_push() => match push {
                        /* A push says something changed: read it all now
                         * (one status has everything, errors included). */
                        Ok(_) => wait = Duration::ZERO,
                        Err(e) => break e,
                    },
                    cmd = commands.recv() => match cmd {
                        None => return,
                        Some(cmd) => {
                            if let Err(e) = self.handle(&mut link, cmd).await {
                                break e;
                            }
                            /* A part reset or a mode change: read it all again. */
                            extras.read_at = None;
                            /* The new state soon, whatever the command did. */
                            wait = Duration::from_secs(1);
                        }
                    },
                }
            };
            match &failure {
                Failure::Unanswered(_) => {
                    unanswered += 1;
                    if unanswered >= UNANSWERED_LIMIT {
                        println!("roborock: {}: connects but doesn't answer: its key changed? (Pair again)", self.id);
                        self.hub.set_online(&self.id, Health::Unauthorized).await;
                    }
                }
                _ => {
                    if !logged_down {
                        println!("roborock: {}: {failure} (will keep trying)", self.id);
                        logged_down = true;
                    }
                    self.hub.set_online(&self.id, Health::Offline).await;
                }
            }
            if !self.wait_refusing(&mut commands, retry).await {
                return;
            }
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    /* Waits `time`, refusing commands meanwhile; false if the hub dropped
     * the device. */
    async fn wait_refusing(&self, commands: &mut mpsc::Receiver<DeviceCmd>, time: Duration) -> bool {
        let sleep = tokio::time::sleep(time);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return true,
                cmd = commands.recv() => match cmd {
                    None => return false,
                    Some(cmd) => cmd.refuse(format!("{} isn't reachable right now", self.id)),
                },
            }
        }
    }

    /* get_status -> report; true while it's busy (cleaning, returning). */
    async fn poll(&self, link: &mut Link, extras: &mut Extras) -> Result<bool, Failure> {
        let status = link.call("get_status", json!([])).await?;
        let mut value = vacuum_state(&status).map_err(Failure::Refused)?;
        if extras.read_at.is_none_or(|t| t.elapsed() >= EXTRAS_EVERY) {
            extras.read(link, &self.config).await?;
        }
        for (key, extra) in &extras.values {
            value[key] = extra.clone();
        }
        value["map"] = json!(self.map.is_some());
        let busy = matches!(value["state"].as_str(), Some("cleaning") | Some("returning"));
        if let Err(e) = self.hub.report(&self.id, "vacuum", value).await {
            println!("roborock: {}: {e}", self.id);
        }
        self.hub.set_online(&self.id, Health::Online).await;
        Ok(busy)
    }

    /* A command for the vacuum. A refusal is the command's answer, not a
     * broken connection. */
    async fn handle(&self, link: &mut Link, cmd: DeviceCmd) -> Result<(), Failure> {
        let DeviceCmd::Action { capability, name, args, reply } = cmd else {
            cmd.refuse("a vacuum is steered with its actions: start, pause, stop, dock, locate");
            return Ok(());
        };
        if capability == "vacuum" && name == "map" {
            match &self.map {
                Some(map) => {
                    let map = map.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(map.get().await.map_err(|e| format!("no map: {e}")));
                    });
                }
                None => {
                    let _ = reply.send(Err("the map is off: turn it on with \"Pair again\"".into()));
                }
            }
            return Ok(());
        }
        /* (device.rs's check_action already checked the arguments.) */
        let text = |key: &str| args[key].as_str().unwrap_or_default().to_string();
        let code = |table: &[(i64, &str)], key: &str| mode_code(table, &text(key)).map(|c| json!([c]));
        let request: Option<(&str, Value)> = match (capability.as_str(), name.as_str()) {
            ("vacuum", "start") => Some(("app_start", json!([]))),
            ("vacuum", "pause") => Some(("app_pause", json!([]))),
            ("vacuum", "stop") => Some(("app_stop", json!([]))),
            ("vacuum", "dock") => Some(("app_charge", json!([]))),
            ("vacuum", "locate") => Some(("find_me", json!([]))),
            ("vacuum", "set_fan") => code(FANS, "fan").map(|c| ("set_custom_mode", c)),
            ("vacuum", "set_water") => code(WATERS, "water").map(|c| ("set_water_box_custom_mode", c)),
            ("vacuum", "set_mop") => code(MOPS, "mop").map(|c| ("set_mop_mode", c)),
            ("vacuum", "clean_rooms") => Some((
                "app_segment_clean",
                json!([{"segments": args["rooms"], "repeat": args["repeat"].as_u64().unwrap_or(1)}]),
            )),
            ("vacuum", "reset_part") => PARTS
                .iter()
                .find(|(_, id, _, _)| *id == text("part"))
                .map(|(key, _, _, _)| ("reset_consumable", json!([key]))),
            _ => None,
        };
        let Some((method, params)) = request else {
            let _ = reply.send(Err(format!("{capability} {name}: not something this vacuum does")));
            return Ok(());
        };
        match link.call(method, params).await {
            Ok(_) => {
                let _ = reply.send(Ok(json!({})));
                Ok(())
            }
            Err(Failure::Refused(e)) => {
                let _ = reply.send(Err(format!("the vacuum refused: {e}")));
                Ok(())
            }
            Err(e) => {
                let _ = reply.send(Err(e.to_string()));
                Err(e)
            }
        }
    }
}

/* ------------------------------------------------------------------ */
/* Setup                                                               */
/* ------------------------------------------------------------------ */

/* The test step: connect with these settings, read the status. */
async fn probe(values: &SetupValues) -> Result<Probe, SetupError> {
    let settings = Settings::from(&values.plain, |name| values.secret.get(name).map(|s| s.expose().to_string()));
    let mut link = Link::open(&settings).await.map_err(|e| match e {
        Failure::Unreachable(e) => SetupError::new(ErrorKind::Unreachable, e),
        other => SetupError::new(ErrorKind::Unsupported, other.to_string()),
    })?;
    let status = link.call("get_status", json!([])).await.map_err(|e| match e {
        Failure::Unreachable(e) => SetupError::new(ErrorKind::Unreachable, e),
        Failure::Unanswered(e) => SetupError::new(
            ErrorKind::Refused,
            format!("The vacuum is there, but doesn't accept this key ({e}). Log in again, or check the token."),
        ),
        Failure::Refused(e) => SetupError::new(ErrorKind::Unsupported, e),
    })?;
    let value = vacuum_state(&status).map_err(|e| SetupError::new(ErrorKind::Unsupported, e))?;
    let model = values.plain.get("model").cloned().unwrap_or_else(|| "Roborock".into());
    let battery = value["battery"].as_u64().map_or(String::new(), |b| format!(", battery {b} %"));
    Ok(Probe {
        values: Default::default(),
        name: None,
        summary: format!("{model}: {}{battery}", value["detail"].as_str().unwrap_or("")),
    })
}

fn need<'a>(values: &'a SetupValues, name: &str) -> Result<&'a str, SetupError> {
    values
        .plain
        .get(name)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| SetupError::new(ErrorKind::Unsupported, format!("{name} is missing")))
}

async fn action(name: &str, values: &SetupValues) -> Result<SetupValues, SetupError> {
    let mut out = SetupValues::default();
    match name {
        "send_code" => {
            let pending = roborock_cloud::send_code(need(values, "email")?).await?;
            out.plain.insert("login_base_url".into(), pending.base_url);
            out.plain.insert("login_country".into(), pending.country);
            out.plain.insert("login_country_code".into(), pending.country_code);
            out.plain.insert("login_client".into(), pending.client);
        }
        "login" => {
            let pending = Pending {
                base_url: need(values, "login_base_url")?.into(),
                country: need(values, "login_country")?.into(),
                country_code: need(values, "login_country_code")?.into(),
                client: need(values, "login_client")?.into(),
            };
            let session = roborock_cloud::login(need(values, "email")?, need(values, "code")?, &pending).await?;
            let json = serde_json::to_string(&session).unwrap_or_default();
            out.secret.insert("login_session".into(), Secret::new(json));
        }
        "list_devices" => {
            let session: Session = values
                .secret
                .get("login_session")
                .and_then(|s| serde_json::from_str(s.expose()).ok())
                .ok_or_else(|| SetupError::new(ErrorKind::Unsupported, "not logged in"))?;
            /* The home's room names, kept with the vacuum (config
             * "room_<id>": its room mapping refers to these ids). Not
             * fatal: rooms then show as "Room 16". */
            let room_names: Vec<(String, String)> = roborock_cloud::rooms(&session)
                .await
                .ok()
                .and_then(|r| r.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|r| Some((format!("room_{}", r["id"]), r["name"].as_str()?.chars().take(64).collect())))
                .take(64)
                .collect();
            for vacuum in roborock_cloud::devices(&session).await? {
                let supported = vacuum.protocol == "1.0";
                let model = if vacuum.model.is_empty() { "Roborock".to_string() } else { vacuum.model.clone() };
                out.cloud_devices.push(CloudDevice {
                    id: vacuum.duid.clone(),
                    name: vacuum.name.clone(),
                    detail: if supported {
                        format!("{model} - {}", if vacuum.online { "online" } else { "offline" })
                    } else {
                        format!("{model}: its protocol ({}) isn't supported yet", vacuum.protocol)
                    },
                    available: supported,
                    plain: [
                        ("duid".to_string(), vacuum.duid),
                        ("protocol".to_string(), "tcp".to_string()),
                        ("model".to_string(), model),
                    ]
                    .into_iter()
                    .chain(room_names.iter().cloned())
                    .collect(),
                    /* (The session itself stays with the account,
                     * accounts.rs: the map uses it from there.) */
                    secret: [("local_key".to_string(), Secret::new(vacuum.local_key))].into(),
                });
            }
        }
        other => return Err(SetupError::new(ErrorKind::Unsupported, format!("roborock has no action {other:?}"))),
    }
    Ok(out)
}

#[cfg(test)]
pub mod sim {
    /* A Roborock speaking "1.0" on a local TCP port, for tests: answers
     * hello and get_status, obeys app_start / app_charge, pushes its
     * state when it changes. */
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    pub struct Sim {
        pub host: String,
    }

    pub async fn start(local_key: &str) -> Sim {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(8));
        let key = local_key.to_string();
        let shared = state.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let key = key.clone();
                let state = shared.clone();
                tokio::spawn(async move {
                    let mut decoder = Decoder::default();
                    let mut buf = [0u8; 4096];
                    loop {
                        let Ok(n) = stream.read(&mut buf).await else { return };
                        if n == 0 {
                            return;
                        }
                        decoder.push(&buf[..n]);
                        /* A wrong key: unreadable, ignored like a real one. */
                        while let Some(Ok(message)) = decoder.next(&key) {
                            let mut reply = Message { payload: Vec::new(), ..message.clone() };
                            if message.protocol == proto::HELLO_REQUEST {
                                reply.protocol = proto::HELLO_RESPONSE;
                            } else {
                                let request: Value = serde_json::from_slice(&message.payload).unwrap();
                                let inner: Value = serde_json::from_str(request["dps"]["101"].as_str().unwrap()).unwrap();
                                let result = match inner["method"].as_str().unwrap() {
                                    "get_status" => json!([{"state": *state.lock().unwrap(), "battery": 91, "error_code": 0}]),
                                    "app_start" => {
                                        *state.lock().unwrap() = 5;
                                        json!(["ok"])
                                    }
                                    "app_charge" => {
                                        *state.lock().unwrap() = 6;
                                        json!(["ok"])
                                    }
                                    _ => json!(["ok"]),
                                };
                                let answer = json!({"id": inner["id"], "result": result}).to_string();
                                reply.payload = json!({"dps": {"102": answer}, "t": message.timestamp}).to_string().into_bytes();
                            }
                            if stream.write_all(&proto::encode(&reply, &key)).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        Sim { host }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_hub::TestHub;

    const KEY: &str = "abcdefghijklmnop";

    fn values(host: &str, key: &str) -> SetupValues {
        SetupValues {
            plain: [("host".to_string(), host.to_string()), ("protocol".to_string(), "tcp".to_string())].into(),
            secret: [("local_key".to_string(), Secret::new(key))].into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn probe_reads_the_status_and_spots_a_wrong_key() {
        let sim = sim::start(KEY).await;
        let probe = probe(&values(&sim.host, KEY)).await.unwrap();
        assert!(probe.summary.contains("charging") && probe.summary.contains("91 %"), "{}", probe.summary);
        let wrong = probe_err(&values(&sim.host, "ponmlkjihgfedcba")).await;
        assert_eq!(wrong.kind, ErrorKind::Refused);
    }

    async fn probe_err(values: &SetupValues) -> SetupError {
        probe(values).await.unwrap_err()
    }

    #[tokio::test]
    async fn the_task_reports_and_obeys() {
        let sim = sim::start(KEY).await;
        let device: Device = serde_json::from_value(json!({
            "id": "robo", "name": "Robo", "template": "roborock-vacuum", "source": "roborock",
            "config": {"host": sim.host, "protocol": "tcp"},
            "capabilities": {"vacuum": {}}
        }))
        .unwrap();
        let hub_secrets = [("local_key".to_string(), Secret::new(KEY))].into();
        let mut hub = TestHub::start_with_secrets(device, Box::new(Roborock), hub_secrets).await;
        let d = hub.until(|d| d.capabilities.vacuum.as_ref().is_some_and(|v| v.state == VacuumState::Docked)).await;
        assert_eq!(d.capabilities.vacuum.unwrap().battery, Some(91));
        hub.control.action("robo", "vacuum", "start", json!({})).await.unwrap();
        hub.until(|d| d.capabilities.vacuum.as_ref().is_some_and(|v| v.state == VacuumState::Cleaning)).await;
        hub.control.action("robo", "vacuum", "dock", json!({})).await.unwrap();
        hub.until(|d| d.capabilities.vacuum.as_ref().is_some_and(|v| v.state == VacuumState::Returning)).await;
    }

    /* LIVE, against the real Roborock account and vacuum, in two runs
     * (the code arrives by email in between). Needs a CA bundle on the PC
     * (Debian/Ubuntu have it at the same path as the image). Prints names,
     * models and the status -- never the key.
     *   ROBOROCK_EMAIL=you@example.com cargo test live_roborock_send_code -- --ignored --nocapture
     *   ROBOROCK_EMAIL=you@example.com ROBOROCK_CODE=123456 cargo test live_roborock_login -- --ignored --nocapture
     * Between the two, the non-secret login state waits in a temp file. */
    fn live_pending_file() -> std::path::PathBuf {
        std::env::temp_dir().join("roborock-live-pending.json")
    }

    #[tokio::test]
    #[ignore]
    async fn live_roborock_send_code() {
        let email = std::env::var("ROBOROCK_EMAIL").expect("ROBOROCK_EMAIL");
        let pending = roborock_cloud::send_code(&email).await.map_err(|e| e.detail).unwrap();
        std::fs::write(live_pending_file(), serde_json::to_string(&pending).unwrap()).unwrap();
        println!("code sent; account on {} ({})", pending.base_url, pending.country);
    }

    #[tokio::test]
    #[ignore]
    async fn live_roborock_login() {
        let email = std::env::var("ROBOROCK_EMAIL").expect("ROBOROCK_EMAIL");
        let code = std::env::var("ROBOROCK_CODE").expect("ROBOROCK_CODE");
        let pending: Pending = serde_json::from_str(&std::fs::read_to_string(live_pending_file()).unwrap()).unwrap();
        let session = roborock_cloud::login(&email, &code, &pending).await.map_err(|e| e.detail).unwrap();
        let _ = std::fs::remove_file(live_pending_file());
        println!("logged in");
        let vacuums = roborock_cloud::devices(&session).await.map_err(|e| e.detail).unwrap();
        for vacuum in &vacuums {
            println!("  {} | {} | protocol {} | online {} | duid {}", vacuum.name, vacuum.model, vacuum.protocol, vacuum.online, vacuum.duid);
        }
        /* The LAN: where it says it is (its broadcast), with its key. */
        let socket = tokio::net::UdpSocket::bind(("0.0.0.0", 58866)).await.unwrap();
        let mut buf = [0u8; 2048];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
            let Some((duid, ip)) = proto::decode_broadcast(&buf[..n]) else { continue };
            let Some(vacuum) = vacuums.iter().find(|v| v.duid == duid) else { continue };
            println!("{} broadcasts from {ip}", vacuum.name);
            let values = SetupValues {
                plain: [("host".to_string(), ip), ("protocol".to_string(), "tcp".to_string()), ("model".to_string(), vacuum.model.clone())].into(),
                secret: [("local_key".to_string(), Secret::new(vacuum.local_key.clone()))].into(),
                ..Default::default()
            };
            match probe(&values).await {
                Ok(p) => println!("LAN OK: {}", p.summary),
                Err(e) => println!("LAN FAILED: {:?} {}", e.kind, e.detail),
            }
            /* ROBOROCK_MAP=<file>: the map through Roborock's cloud, saved
             * raw to the file (for tests), and what it holds. */
            if let Ok(path) = std::env::var("ROBOROCK_MAP") {
                match crate::adapters::roborock_map::fetch(&session, &vacuum.duid, &vacuum.local_key).await {
                    Ok(raw) => {
                        std::fs::write(&path, &raw).unwrap();
                        println!("map: {} bytes raw, saved to {path}", raw.len());
                        match crate::adapters::roborock_map::parse(&raw) {
                            Ok(map) => {
                                let packed = map.to_json().to_string();
                                println!(
                                    "map: {}x{} px, rooms {:?}, dock {:?}, robot {:?}, path {} points, {} no-go, {} walls; {} bytes for a client",
                                    map.width, map.height, map.rooms, map.dock, map.robot, map.path.len(), map.no_go.len(), map.walls.len(), packed.len()
                                );
                            }
                            Err(e) => println!("map: PARSE FAILED {e}"),
                        }
                    }
                    Err(e) => println!("map: FETCH FAILED {e}"),
                }
            }
            /* ROBOROCK_DUMP=1: what the vacuum can tell, for building its
             * features (no secrets in any of these answers). */
            if std::env::var("ROBOROCK_DUMP").is_ok() {
                let settings = Settings::from(&values.plain, |n| values.secret.get(n).map(|s| s.expose().to_string()));
                let mut link = Link::open(&settings).await.map_err(|e| e.to_string()).unwrap();
                for method in ["get_status", "get_consumable", "get_room_mapping", "get_clean_summary", "get_clean_record_map",
                               "get_fan_power", "get_water_box_custom_mode", "get_mop_mode", "get_dnd_timer", "get_multi_maps_list"] {
                    match link.call(method, json!([])).await {
                        Ok(v) => println!("{method}: {v}"),
                        Err(e) => println!("{method}: FAILED {e}"),
                    }
                }
                println!("home rooms: {}", roborock_cloud::rooms(&session).await.map_err(|e| e.detail).unwrap_or_default());
            }
            return;
        }
        println!("no broadcast from these vacuums within 15 s");
    }

    /* The project's S7's real answers (2026-10-03). */
    #[test]
    fn a_real_s7_is_understood() {
        let status = json!([{"adbumper_status":[0,0,0],"auto_dust_collection":1,"battery":100,"clean_area":140000,"clean_time":14,
            "dnd_enabled":0,"dock_error_status":0,"dock_type":0,"error_code":0,"fan_power":104,"in_cleaning":0,"in_returning":0,
            "map_present":1,"mop_mode":300,"state":8,"water_box_carriage_status":1,"water_box_mode":203,"water_box_status":1}]);
        let value = vacuum_state(&status).unwrap();
        assert_eq!(value["state"], "docked");
        assert_eq!(value["fan"], "max");
        assert_eq!(value["fans"], json!(["off", "quiet", "balanced", "turbo", "max"]));
        assert_eq!(value["water"], "high");
        assert_eq!(value["mop"], "standard");
        assert_eq!(value["area_m2"], json!(0.14));
        assert_eq!(value["minutes"], json!(0));

        let consumable = json!([{"dust_collection_work_times":0,"filter_element_work_time":0,"filter_work_time":424697,
            "main_brush_work_time":424697,"sensor_dirty_time":146644,"side_brush_work_time":424697}]);
        assert_eq!(
            parts(&consumable),
            json!([{"id": "main_brush", "name": "Main brush", "left": 60}, {"id": "side_brush", "name": "Side brush", "left": 41},
                   {"id": "filter", "name": "Filter", "left": 21}, {"id": "sensors", "name": "Sensors (clean them)", "left": 0}])
        );
        let summary = json!({"clean_area":6977235000u64,"clean_count":272,"clean_time":423893,"records":[1791047028]});
        assert_eq!(totals(&summary).unwrap(), json!({"cleanings": 272, "area_m2": 6977.2, "hours": 117.7}));
        assert_eq!(quiet_hours(&json!([{"enabled":1,"end_hour":8,"end_minute":0,"start_hour":22,"start_minute":0}])), json!("22:00-08:00"));
        assert_eq!(quiet_hours(&json!([{"enabled":0}])), Value::Null);
        /* Its map has no rooms yet... */
        assert_eq!(rooms(&json!([]), &Default::default()), json!([]));
        /* ...a divided one would look like this. */
        let config = [("room_27906924".to_string(), "Cameră de zi".to_string())].into();
        assert_eq!(
            rooms(&json!([[16, "27906924", 14], [17, "999", 14]]), &config),
            json!([{"id": 16, "name": "Cameră de zi"}, {"id": 17, "name": "Room 17"}])
        );
        /* The whole value passes device.rs's rules. */
        let mut full = value.clone();
        full["parts"] = parts(&consumable);
        full["totals"] = totals(&summary).unwrap();
        let vacuum: crate::device::Vacuum = serde_json::from_value(full).unwrap();
        assert_eq!(vacuum.parts.len(), 4);
    }

    #[test]
    fn errors_win_over_other_states() {
        let value = vacuum_state(&json!([{"state": 5, "battery": 40, "error_code": 8}])).unwrap();
        assert_eq!(value["state"], "error");
        assert_eq!(value["error"], "Stuck: move it somewhere free");
        /* Docked with an error code (a dock problem): still docked. */
        assert_eq!(vacuum_state(&json!([{"state": 8, "error_code": 19}])).unwrap()["state"], "docked");
        assert!(vacuum_state(&json!([{}])).is_err());
    }
}
