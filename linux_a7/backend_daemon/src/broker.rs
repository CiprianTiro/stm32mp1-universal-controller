/*
 * broker.rs -- the hub's local MQTT broker (issue #72): devices that
 * connect to the hub instead of being connected to.
 *
 * WLED, Tasmota, ESPHome and many DIY devices can send their state to an
 * MQTT broker by themselves, the moment it changes, and take commands the
 * same way. The broker is Mosquitto, its own sandboxed service
 * (recipes-connectivity/mosquitto); this file is its owner:
 *
 *   LOGINS   every device gets its OWN user and password, made by the
 *            add-device wizard (add_device). The hub logs in as "hub".
 *            Mosquitto only sees hashes: passwd, lines
 *              user:$7$101$<salt>$<hash>
 *            -- PBKDF2-HMAC-SHA512, 101 rounds, 12-byte salt, 64-byte hash,
 *            standard base64: exactly what mosquitto_passwd writes
 *            (mosquitto 2.0.22, apps/mosquitto_passwd).
 *   RULES    acl: which topics each user may read and write. A device
 *            only gets its own ("tele/kitchen/#"), so nothing on the LAN
 *            can publish as another device. The hub may use all of them.
 *   TLS      port 8883 uses the hub's own certificate (tls.rs), copied in.
 *
 * The files live in /usr/local/etc/universal-controller/broker: written
 * by user hubd, readable by group mosquitto (the folder is setgid, files
 * 0640). After a change, hub-helper reloads mosquitto (it runs as another
 * user, the daemon can't signal it). Every value that goes into the files
 * is checked (names, topics): a newline in a topic would otherwise add a
 * rule of its own.
 *
 * What it remembers (broker.json in the data folder): each device user's
 * hash and rules -- never a password. The device's password itself is a
 * device secret (secrets.rs), so "change settings" can show it again.
 *
 * NEW DEVICE WAITING: Mosquitto publishes its own log lines on
 * $SYS/broker/log/... (log_dest topic; only the hub may read them). A
 * device knocking without a valid login shows up there:
 *     New connection from 192.168.1.145:55402 on port 1883.
 *     Client DVES_B0AECC disconnected, not authorised.
 * -> an announcement {address, client_id, client_kind: "DVES"} for
 * discovery.rs: the "found on your network" inbox offers the template
 * whose device_announce matches (Tasmota's ids start with DVES_).
 *
 * THE HUB'S OWN CONNECTION: a client task logs in as "hub" on 127.0.0.1,
 * subscribes to everything, and hands every message to whoever listens
 * (subscribe_messages: the device adapters, stage 2), and publishes their
 * commands (publish).
 */
use base64::Engine;
use ring::rand::SecureRandom;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

use crate::helper;

const DEFAULT_BROKER_DIR: &str = "/usr/local/etc/universal-controller/broker";
pub const BROKER_SCHEMA: u32 = 1;
/* The hub's own login. */
pub const HUB_USER: &str = "hub";
/* Where the hub connects: the broker on this board, plain (it never
 * leaves the board). */
const LOCAL_ADDR: &str = "127.0.0.1";
pub const PLAIN_PORT: u16 = 1883;
/* mosquitto_passwd's defaults (see the header). */
const ITERATIONS: u32 = 101;
const SALT_LEN: usize = 12;
const HASH_LEN: usize = 64;
/* Reconnecting to the broker: from 1 s, doubling, at most 30 s. */
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/* Overridable with HUB_BROKER_DIR (running the daemon on the PC). */
pub fn broker_dir() -> PathBuf {
    std::env::var_os("HUB_BROKER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BROKER_DIR))
}

/* ------------------------------------------------------------------ */
/* The model                                                           */
/* ------------------------------------------------------------------ */

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    Write,
    Readwrite,
}

/* One access rule: "may <access> <topic>" ("tele/kitchen/#"). */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub access: Access,
    pub topic: String,
}

/* One device's login, as remembered: never its password. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Login {
    /* "$7$101$<salt>$<hash>" (see the header). */
    pub hash: String,
    pub rules: Vec<Rule>,
    /* When it was made (Unix seconds): a login the wizard just made has no
     * device yet -- prune() leaves it alone for a while. */
    #[serde(default)]
    pub created: u64,
}

/* A login no device uses is removed after this long (prune). */
const UNUSED_GRACE_S: u64 = 30 * 60;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/* Everything remembered: the device users, by name. */
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Logins {
    pub users: BTreeMap<String, Login>,
}

/* A user name: 1-64 of a-z, 0-9, "-", "_" (device ids are of that kind).
 * Nothing that could end a line or a field in Mosquitto's files. */
pub fn valid_user(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/* A topic for a rule: 1-128 printable ASCII characters without spaces;
 * "+" only as a whole level, "#" only as the last one ("tele/x/#"). An
 * empty level ("a//b") or a leading "$" (the broker's own topics) isn't
 * allowed either. */
pub fn valid_topic(topic: &str) -> bool {
    if topic.is_empty() || topic.len() > 128 || topic.starts_with('$') {
        return false;
    }
    if !topic.bytes().all(|b| b.is_ascii_graphic()) {
        return false;
    }
    let levels: Vec<&str> = topic.split('/').collect();
    levels.iter().enumerate().all(|(i, level)| match *level {
        "" => false,
        "#" => i == levels.len() - 1,
        "+" => true,
        other => !other.contains(['+', '#']),
    })
}

/* ------------------------------------------------------------------ */
/* Mosquitto's files (pure, unit-tested)                               */
/* ------------------------------------------------------------------ */

/* "$7$101$<salt>$<hash>" for this password and salt. */
fn hash_password(password: &str, salt: &[u8; SALT_LEN]) -> String {
    let mut hash = [0u8; HASH_LEN];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA512,
        std::num::NonZeroU32::new(ITERATIONS).expect("not zero"),
        salt,
        password.as_bytes(),
        &mut hash,
    );
    let b64 = base64::engine::general_purpose::STANDARD;
    format!("$7${ITERATIONS}${}${}", b64.encode(salt), b64.encode(hash))
}

/* The passwd file: the hub, then every device user. */
fn passwd_file(hub_hash: &str, logins: &Logins) -> String {
    let mut out = format!("{HUB_USER}:{hub_hash}\n");
    for (user, login) in &logins.users {
        out.push_str(&format!("{user}:{}\n", login.hash));
    }
    out
}

/* The acl file: the hub may use everything; each device its own rules. */
fn acl_file(logins: &Logins) -> String {
    let mut out = String::from(
        "# Written by backend-daemon (broker.rs, issue #72): changes made here are overwritten.\n\n",
    );
    out.push_str(&format!("user {HUB_USER}\ntopic readwrite #\ntopic read $SYS/broker/log/#\n"));
    for (user, login) in &logins.users {
        out.push_str(&format!("\nuser {user}\n"));
        for rule in &login.rules {
            let access = match rule.access {
                Access::Read => "read",
                Access::Write => "write",
                Access::Readwrite => "readwrite",
            };
            out.push_str(&format!("topic {access} {}\n", rule.topic));
        }
    }
    out
}

/* A random password: 20 letters and digits (no look-alikes: 0/O, 1/l/I),
 * easy enough to type into a device's web page. */
fn new_password() -> Result<String, String> {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 20];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "no random numbers".to_string())?;
    /* 256 isn't a multiple of 57: the first letters are very slightly
     * more likely. Irrelevant at 20 characters (over 110 bits). */
    Ok(bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect())
}

fn new_salt() -> Result<[u8; SALT_LEN], String> {
    let mut salt = [0u8; SALT_LEN];
    ring::rand::SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| "no random numbers".to_string())?;
    Ok(salt)
}

/* Writes a file mosquitto must read: temporary file, mode 0640 (the
 * folder's setgid bit gives it group mosquitto), then renamed over the
 * old one in one step. */
fn write_shared(path: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut file = std::fs::File::create(&tmp).map_err(fail)?;
    file.write_all(content).map_err(fail)?;
    file.sync_all().map_err(fail)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)
}

/* ------------------------------------------------------------------ */
/* The broker's owner                                                  */
/* ------------------------------------------------------------------ */

pub struct Broker {
    dir: PathBuf,
    logins: Mutex<Logins>,
    /* The hub's own password (from hub.secret, see open). */
    hub_password: String,
    /* store::writer's channel for broker.json. */
    save_tx: watch::Sender<Vec<u8>>,
    /* Every message the hub receives from the broker: (topic, payload). */
    messages: broadcast::Sender<(String, Vec<u8>)>,
    /* The hub's client, once connected (publish). */
    client: Mutex<Option<AsyncClient>>,
    /* The last message on every topic: a device's task that starts after
     * it arrived (e.g. Tasmota's retained "Online", delivered once at the
     * hub's subscribe) still gets it (last_messages). */
    last: Mutex<HashMap<String, Vec<u8>>>,
    /* Where "new device waiting" goes (discovery.rs). */
    announcer: OnceLock<tokio::sync::mpsc::Sender<crate::discovery::Vars>>,
    /* The address of the latest connection the log told about. */
    last_connection: Mutex<Option<String>>,
    /* Tests: publishes go here instead of to a broker. */
    #[cfg(test)]
    test_sink: Option<tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>>,
}

/* At most this many topics remembered (last): a device flooding new
 * topics can't grow it without limit. */
const MAX_REMEMBERED: usize = 2000;

impl Broker {
    /* Takes over the broker's folder: the hub's password (made once,
     * kept in hub.secret, 0600 -- mosquitto can't read it), the TLS
     * certificate, and passwd/acl from what's remembered. */
    pub fn open(dir: PathBuf, logins: Logins, save_tx: watch::Sender<Vec<u8>>, tls_dir: &Path) -> Result<Broker, String> {
        let secret = dir.join("hub.secret");
        let hub_password = match std::fs::read_to_string(&secret) {
            Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => {
                let password = new_password()?;
                write_shared(&secret, password.as_bytes(), 0o600)?;
                password
            }
        };
        /* The hub's certificate for port 8883 (tls.rs made it). */
        for (from, to) in [("hub.crt", "server.crt"), ("hub.key", "server.key")] {
            match std::fs::read(tls_dir.join(from)) {
                Ok(bytes) => write_shared(&dir.join(to), &bytes, 0o640)?,
                Err(e) => println!("broker: no {from} for the TLS port yet: {e}"),
            }
        }
        let broker = Broker {
            dir,
            logins: Mutex::new(logins),
            hub_password,
            save_tx,
            messages: broadcast::channel(256).0,
            client: Mutex::new(None),
            last: Mutex::new(HashMap::new()),
            announcer: OnceLock::new(),
            last_connection: Mutex::new(None),
            #[cfg(test)]
            test_sink: None,
        };
        broker.write_files()?;
        Ok(broker)
    }

    /* passwd and acl from the current logins. The hub's hash is made new
     * each time (a new salt): nothing about its password is stored but
     * hub.secret. */
    fn write_files(&self) -> Result<(), String> {
        let logins = self.logins.lock().unwrap().clone();
        let hub_hash = hash_password(&self.hub_password, &new_salt()?);
        write_shared(&self.dir.join("passwd"), passwd_file(&hub_hash, &logins).as_bytes(), 0o640)?;
        write_shared(&self.dir.join("acl"), acl_file(&logins).as_bytes(), 0o640)
    }

    /* After a change: remember it, rewrite the files, reload mosquitto. */
    async fn apply(&self) -> Result<(), String> {
        let json = serde_json::to_vec(&*self.logins.lock().unwrap()).expect("logins are valid JSON");
        self.save_tx.send_replace(json);
        /* The tests' broker (for_tests) has no folder and no Mosquitto. */
        if self.dir.as_os_str().is_empty() {
            return Ok(());
        }
        self.write_files()?;
        reload().await
    }

    /* A new device login (or a new password for an existing one): the
     * password, to put into the device (and keep as its secret). */
    pub async fn add_device(&self, user: &str, rules: Vec<Rule>) -> Result<String, String> {
        if !valid_user(user) || user == HUB_USER {
            return Err(format!("user name {user:?}: 1-64 of a-z, 0-9, - and _ (not \"hub\")"));
        }
        if rules.is_empty() || rules.len() > 16 {
            return Err("a device login needs 1-16 topic rules".into());
        }
        if let Some(bad) = rules.iter().find(|r| !valid_topic(&r.topic)) {
            return Err(format!("topic {:?} isn't allowed in a rule", bad.topic));
        }
        let password = new_password()?;
        let hash = hash_password(&password, &new_salt()?);
        self.logins
            .lock()
            .unwrap()
            .users
            .insert(user.to_string(), Login { hash, rules, created: unix_now() });
        self.apply().await?;
        println!("broker: login for {user} set");
        Ok(password)
    }

    /* Removes the logins no device uses any more (a device's config holds
     * its "mqtt_user"): a removed Tasmota can't log in again. Logins made
     * in the last UNUSED_GRACE_S are kept -- the wizard makes them before
     * the device exists. Returns the ones removed. */
    pub async fn prune(&self, in_use: &[String]) -> Vec<String> {
        let now = unix_now();
        let removed: Vec<String> = {
            let mut logins = self.logins.lock().unwrap();
            let gone: Vec<String> = logins
                .users
                .iter()
                .filter(|(user, login)| !in_use.contains(user) && now.saturating_sub(login.created) > UNUSED_GRACE_S)
                .map(|(user, _)| user.clone())
                .collect();
            for user in &gone {
                logins.users.remove(user);
            }
            gone
        };
        if !removed.is_empty() {
            if let Err(e) = self.apply().await {
                println!("broker: {e}");
            }
            println!("broker: removed unused logins: {}", removed.join(", "));
        }
        removed
    }

    #[cfg(test)]
    pub fn has_login(&self, user: &str) -> bool {
        self.logins.lock().unwrap().users.contains_key(user)
    }

    /* Every message from devices, from now on. */
    pub fn subscribe_messages(&self) -> broadcast::Receiver<(String, Vec<u8>)> {
        self.messages.subscribe()
    }

    /* The last message on each of these topics, if any (see `last`). */
    pub fn last_messages(&self, topics: &[String]) -> Vec<(String, Vec<u8>)> {
        let last = self.last.lock().unwrap();
        topics.iter().filter_map(|t| last.get(t).map(|p| (t.clone(), p.clone()))).collect()
    }

    pub fn set_announcer(&self, announcer: tokio::sync::mpsc::Sender<crate::discovery::Vars>) {
        let _ = self.announcer.set(announcer);
    }

    /* One of Mosquitto's log lines (see the header). */
    fn log_line(&self, line: &str) {
        match parse_log(line) {
            Some(LogLine::Connection(address)) => *self.last_connection.lock().unwrap() = Some(address),
            Some(LogLine::Refused(client_id)) => {
                let Some(address) = self.last_connection.lock().unwrap().clone() else { return };
                if address.starts_with("127.") {
                    return;
                }
                let kind: String = client_id.chars().take_while(|c| *c != '_' && *c != '-').collect();
                let vars = crate::discovery::Vars::from([
                    ("address".to_string(), address),
                    ("client_id".to_string(), client_id),
                    ("client_kind".to_string(), kind),
                ]);
                if let Some(announcer) = self.announcer.get() {
                    let _ = announcer.try_send(vars);
                }
            }
            None => {}
        }
    }

    /* A message arrived from the broker: remembered, then passed on. */
    fn received(&self, topic: String, payload: Vec<u8>) {
        if topic.starts_with("$SYS/broker/log/") {
            self.log_line(&String::from_utf8_lossy(&payload));
            return;
        }
        {
            let mut last = self.last.lock().unwrap();
            if last.len() < MAX_REMEMBERED || last.contains_key(&topic) {
                last.insert(topic.clone(), payload.clone());
            }
        }
        let _ = self.messages.send((topic, payload));
    }

    /* A message to a device (a command). Fails while not connected. */
    pub async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), String> {
        #[cfg(test)]
        if let Some(sink) = &self.test_sink {
            return sink.send((topic.to_string(), payload)).map_err(|_| "test sink closed".to_string());
        }
        let client = self.client.lock().unwrap().clone().ok_or("the local MQTT broker isn't connected")?;
        client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
            .map_err(|e| format!("local MQTT: {e}"))
    }
}

enum LogLine {
    /* "New connection from <ip>:<port> on port 1883." */
    Connection(String),
    /* "Client <id> disconnected, not authorised." */
    Refused(String),
}

/* Mosquitto's log lines start with a timestamp ("1790963229: "). Client
 * ids are what devices chose: only printable ones of sane length count. */
fn parse_log(line: &str) -> Option<LogLine> {
    let text = line.split_once(": ").map_or(line, |(_, rest)| rest).trim();
    if let Some(rest) = text.strip_prefix("New connection from ") {
        let address = rest.split_whitespace().next()?;
        let ip = address.rsplit_once(':').map_or(address, |(ip, _)| ip);
        return ip.parse::<std::net::IpAddr>().ok().map(|ip| LogLine::Connection(ip.to_string()));
    }
    let id = text.strip_prefix("Client ")?.strip_suffix(" disconnected, not authorised.")?;
    let ok = (1..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_graphic());
    ok.then(|| LogLine::Refused(id.to_string()))
}

/* hub-helper makes mosquitto re-read its files (or starts it). Logged,
 * not fatal: the files are right, the next reload or restart uses them. */
async fn reload() -> Result<(), String> {
    match helper::run(helper::Command::BrokerReload).await {
        Ok(()) => Ok(()),
        Err(e) => {
            println!("broker: reloading mosquitto failed: {e}");
            Err(format!("the local MQTT broker couldn't reload: {e}"))
        }
    }
}

/* For store::load_or_default. */
pub fn decode(schema: u32, payload: &[u8]) -> Result<Logins, String> {
    if schema != BROKER_SCHEMA {
        return Err(format!("unsupported broker schema {schema}"));
    }
    serde_json::from_slice(payload).map_err(|e| e.to_string())
}

/* Runs for the daemon's lifetime (spawned from main): the reload once at
 * start (the files were just written), then the hub's own connection --
 * logged in as "hub", subscribed to everything, reconnecting with backoff. */
pub async fn run(broker: Arc<Broker>) {
    let _ = reload().await;
    let mut options = MqttOptions::new("hub-backend", LOCAL_ADDR, PLAIN_PORT);
    options.set_credentials(HUB_USER, broker.hub_password.clone());
    options.set_keep_alive(Duration::from_secs(30));
    let (client, mut eventloop) = AsyncClient::new(options, 64);
    let mut retry = RETRY_MIN;
    let mut connected = false;
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                println!("broker: the hub is connected to the local MQTT broker");
                connected = true;
                retry = RETRY_MIN;
                /* A clean session forgets subscriptions: again, every time. */
                if let Err(e) = client.try_subscribe("#", QoS::AtLeastOnce) {
                    println!("broker: can't subscribe: {e}");
                }
                /* "#" never matches $SYS: Mosquitto's own log, separately
                 * ("new device waiting"). */
                if let Err(e) = client.try_subscribe("$SYS/broker/log/#", QoS::AtMostOnce) {
                    println!("broker: can't subscribe to the broker's log: {e}");
                }
                *broker.client.lock().unwrap() = Some(client.clone());
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                broker.received(publish.topic.clone(), publish.payload.to_vec());
            }
            Ok(_) => {}
            Err(e) => {
                if connected {
                    println!("broker: lost the local MQTT broker: {e}");
                }
                connected = false;
                *broker.client.lock().unwrap() = None;
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(RETRY_MAX);
            }
        }
    }
}

/* Tests (the device adapters'): a broker without files or Mosquitto.
 * What the hub publishes comes out of the returned receiver; what a
 * device sends goes in with inject(). */
#[cfg(test)]
impl Broker {
    pub fn for_tests() -> (Arc<Broker>, tokio::sync::mpsc::UnboundedReceiver<(String, Vec<u8>)>) {
        let (sink, published) = tokio::sync::mpsc::unbounded_channel();
        let broker = Broker {
            dir: PathBuf::new(),
            logins: Mutex::new(Logins::default()),
            hub_password: String::new(),
            save_tx: watch::channel(Vec::new()).0,
            messages: broadcast::channel(256).0,
            client: Mutex::new(None),
            last: Mutex::new(HashMap::new()),
            announcer: OnceLock::new(),
            last_connection: Mutex::new(None),
            test_sink: Some(sink),
        };
        (Arc::new(broker), published)
    }

    pub fn inject(&self, topic: &str, payload: &str) {
        self.received(topic.to_string(), payload.as_bytes().to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_mosquitto_passwd() {
        /* The same PBKDF2-HMAC-SHA512 computed by Python's hashlib (an
         * independent implementation) for password "secret" and the salt
         * 0x00..0x0b, 101 rounds:
         *   hashlib.pbkdf2_hmac("sha512", b"secret", bytes(range(12)), 101) */
        let salt: [u8; SALT_LEN] = core::array::from_fn(|i| i as u8);
        let line = hash_password("secret", &salt);
        let parts: Vec<&str> = line.split('$').collect();
        assert_eq!(&parts[..3], &["", "7", "101"]);
        assert_eq!(parts[3], "AAECAwQFBgcICQoL");
        assert_eq!(parts[4].len(), 88, "64 bytes, base64 with padding");
        assert_eq!(parts[4], EXPECTED_HASH);
    }

    /* From Python, see above. */
    const EXPECTED_HASH: &str = "Xr99N9ym9ys8TWxis5ajETJq6EVYzmc6nb8t3pUYnPBbCAbICS4xejTltonXWCtJNBvA+By+TXUqU6qCblO00w==";

    #[test]
    fn files_are_rendered() {
        let mut logins = Logins::default();
        logins.users.insert(
            "kitchen".into(),
            Login {
                created: 0,
                hash: "$7$101$x$y".into(),
                rules: vec![
                    Rule { access: Access::Write, topic: "tele/kitchen/#".into() },
                    Rule { access: Access::Read, topic: "cmnd/kitchen/#".into() },
                ],
            },
        );
        assert_eq!(passwd_file("$7$101$a$b", &logins), "hub:$7$101$a$b\nkitchen:$7$101$x$y\n");
        let acl = acl_file(&logins);
        assert!(acl.contains("user hub\ntopic readwrite #\ntopic read $SYS/broker/log/#\n"));
        assert!(acl.contains("user kitchen\ntopic write tele/kitchen/#\ntopic read cmnd/kitchen/#\n"));
    }

    #[tokio::test]
    async fn refused_devices_are_announced() {
        let (broker, _published) = Broker::for_tests();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        broker.set_announcer(tx);
        let log = |line: &str| broker.received("$SYS/broker/log/N".into(), line.as_bytes().to_vec());
        log("1790963229: New connection from 192.168.1.145:55402 on port 1883.");
        log("1790963229: Client DVES_B0AECC disconnected, not authorised.");
        let vars = rx.recv().await.unwrap();
        assert_eq!(vars["address"], "192.168.1.145");
        assert_eq!(vars["client_id"], "DVES_B0AECC");
        assert_eq!(vars["client_kind"], "DVES");
        /* The hub itself, and ordinary lines: nothing. */
        log("1: New connection from 127.0.0.1:4000 on port 1883.");
        log("1: Client hub-backend disconnected, not authorised.");
        log("1: New client connected from 192.168.1.9:1 as x (p2, c1, k30, u'x').");
        assert!(rx.try_recv().is_err());
        /* Log lines are not device messages. */
        assert!(broker.last_messages(&["$SYS/broker/log/N".into()]).is_empty());
    }

    #[test]
    fn names_and_topics_are_checked() {
        assert!(valid_user("wled-kitchen") && valid_user("tasmota_1"));
        assert!(!valid_user("") && !valid_user("Kitchen") && !valid_user("a b") && !valid_user("a\nuser x"));
        for ok in ["tele/kitchen/#", "wled/strip/+/state", "a", "stat/x/RESULT"] {
            assert!(valid_topic(ok), "{ok}");
        }
        for bad in ["", "#/x", "a/#/b", "a/b#", "a+/b", "a//b", "$SYS/#", "a b", "a\ntopic readwrite #", "é"] {
            assert!(!valid_topic(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn unused_logins_are_pruned_after_a_while() {
        let (broker, _published) = Broker::for_tests();
        let rule = || vec![Rule { access: Access::Write, topic: "tele/x/#".into() }];
        broker.add_device("fresh", rule()).await.unwrap();
        broker.add_device("used", rule()).await.unwrap();
        broker.add_device("old", rule()).await.unwrap();
        broker.logins.lock().unwrap().users.get_mut("old").unwrap().created = 1;
        broker.logins.lock().unwrap().users.get_mut("used").unwrap().created = 1;
        /* "used" is some device's; "fresh" was just made (a wizard). */
        assert_eq!(broker.prune(&["used".to_string()]).await, vec!["old".to_string()]);
        assert!(broker.has_login("fresh") && broker.has_login("used") && !broker.has_login("old"));
    }

    #[test]
    fn passwords_are_typeable_and_random() {
        let a = new_password().unwrap();
        let b = new_password().unwrap();
        assert_eq!(a.len(), 20);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() && !"0O1lI".contains(c)));
        assert_ne!(a, b);
    }

    #[test]
    fn decode_reads_what_is_saved() {
        let mut logins = Logins::default();
        logins.users.insert("x".into(), Login { hash: "h".into(), rules: vec![Rule { access: Access::Read, topic: "a".into() }], created: 7 });
        let bytes = serde_json::to_vec(&logins).unwrap();
        assert_eq!(decode(BROKER_SCHEMA, &bytes).unwrap(), logins);
        assert!(decode(2, &bytes).is_err());
    }
}
