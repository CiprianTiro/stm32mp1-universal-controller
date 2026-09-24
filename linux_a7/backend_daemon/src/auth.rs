/*
 * auth.rs -- which LAN clients may control the hub (issue #35): pairing,
 * the paired clients, and checking a client's key.
 *
 * PAIRING (once per client, like a Bluetooth device):
 *   1. On the hub's touchscreen, someone taps "Pair a new device"
 *      (start_pairing): the hub shows a 6-digit code and a QR code.
 *   2. The client (the phone app, tools/hub_ws.py) connects to
 *      wss://<hub>:8443 and sends the code with a name for itself (pair).
 *   3. Right code: the client gets its own random KEY (a "token", 256 bits)
 *      and keeps it. The hub keeps only the key's SHA-256 hash -- so even
 *      someone who reads the hub's files can't use them to log in.
 * AFTERWARDS the client proves who it is with its key at every connection
 * (authenticate), inside the TLS encryption (tls.rs).
 *
 * WHY THE CODE CAN'T BE GUESSED: it only exists after someone at the hub
 * started pairing, lives 2 minutes, works once, and 5 wrong tries end the
 * pairing (it has to be started again at the hub). 5 guesses out of a
 * million possible codes = a 1-in-200,000 chance per tap on the hub's
 * screen. ws.rs additionally slows every failed attempt down by a second.
 *
 * WHY A KEY CAN'T BE GUESSED: 2^256 possibilities.
 *
 * The clients are saved with store.rs (clients.json on userfs: survives
 * reboots and reflashes). A client removed on the hub's screen (revoke)
 * is refused from then on -- and an open connection of it is closed at
 * once (ws.rs listens on `revocations`).
 *
 * The state sits behind a Mutex, not in an actor task like state.rs: every
 * operation here is a few microseconds of plain computation with no
 * waiting inside, so a lock held that briefly can never block anything --
 * and the code stays much simpler than messages with reply envelopes.
 */
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, watch};

/* How long a pairing code is valid. */
const PAIRING_TTL: Duration = Duration::from_secs(120);
/* Wrong codes before the pairing ends. */
const MAX_WRONG_CODES: u32 = 5;
/* last_seen is written to the flash at most this often per client (every
 * connection would be a flash write otherwise). */
const LAST_SEEN_SAVE_INTERVAL: u64 = 3600;

/* A paired client as stored. */
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Client {
    id: String,
    name: String,
    /* SHA-256 of its key, hex. The key itself is never stored. */
    token_sha256: String,
    /* Unix time (seconds). */
    created: u64,
    last_seen: u64,
}

/* What anyone outside this file sees of a client (no hash). */
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ClientInfo {
    pub id: String,
    pub name: String,
    pub created: u64,
    pub last_seen: u64,
}

/* A new client's credentials, handed out once, at pairing. */
#[derive(Serialize, Debug, Clone)]
pub struct Paired {
    pub client_id: String,
    pub token: String,
}

/* What the hub's pairing screen shows. */
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PairingStatus {
    /* "none", "waiting", "paired", "locked" (too many wrong codes) or
     * "expired". */
    pub state: &'static str,
    /* Only while waiting. */
    pub code: Option<String>,
    pub seconds_left: u64,
    /* After "paired": the new client's name. */
    pub client_name: Option<String>,
}

struct Pairing {
    code: String,
    expires: Instant,
    wrong: u32,
    /* Some(name) once a client paired with this code. */
    paired: Option<String>,
}

struct Inner {
    clients: Vec<Client>,
    pairing: Option<Pairing>,
}

pub struct Auth {
    inner: Mutex<Inner>,
    save_tx: watch::Sender<Vec<u8>>,
    revoked_tx: broadcast::Sender<String>,
    rng: SystemRandom,
}

impl Auth {
    /* `stored`: clients.json's content (decode_clients); `save_tx`:
     * store.rs's writer for it. */
    pub fn new(stored: Vec<StoredClient>, save_tx: watch::Sender<Vec<u8>>) -> Self {
        let (revoked_tx, _) = broadcast::channel(16);
        Auth {
            inner: Mutex::new(Inner {
                clients: stored.into_iter().map(|c| c.0).collect(),
                pairing: None,
            }),
            save_tx,
            revoked_tx,
            rng: SystemRandom::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        /* A poisoned lock (a panic while holding it) can't leave the data
         * half-changed here -- every change is a single assignment -- so
         * carry on with it rather than crash the daemon. */
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /* Starts a new pairing (replacing any running one) and returns its
     * code. Only the hub's own screen may call this (ws.rs). */
    pub fn start_pairing(&self) -> PairingStatus {
        self.start_pairing_for(PAIRING_TTL)
    }

    fn start_pairing_for(&self, ttl: Duration) -> PairingStatus {
        let code = self.random_code();
        let mut inner = self.lock();
        inner.pairing = Some(Pairing {
            code,
            expires: Instant::now() + ttl,
            wrong: 0,
            paired: None,
        });
        status_of(inner.pairing.as_ref())
    }

    pub fn cancel_pairing(&self) {
        self.lock().pairing = None;
    }

    pub fn pairing_status(&self) -> PairingStatus {
        status_of(self.lock().pairing.as_ref())
    }

    /* A client offers `code`. On success it's paired: a new client entry
     * with a fresh key, which is returned (and never again). */
    pub fn pair(&self, code: &str, client_name: &str) -> Result<Paired, String> {
        let name = client_name.trim();
        if name.is_empty() || name.chars().count() > 40 || name.chars().any(char::is_control) {
            return Err("client_name must be 1-40 characters".into());
        }
        let mut inner = self.lock();
        let Some(pairing) = inner.pairing.as_mut() else {
            return Err("no pairing in progress: start it on the hub's screen".into());
        };
        match status_of(Some(pairing)).state {
            "waiting" => {}
            "paired" => return Err("this code was already used: start a new pairing on the hub's screen".into()),
            "locked" => return Err("too many wrong codes: start a new pairing on the hub's screen".into()),
            _ => return Err("the code expired: start a new pairing on the hub's screen".into()),
        }
        if !same(code.trim().as_bytes(), pairing.code.as_bytes()) {
            pairing.wrong += 1;
            let left = MAX_WRONG_CODES.saturating_sub(pairing.wrong);
            return Err(if left == 0 {
                "wrong code; too many wrong codes: start a new pairing on the hub's screen".into()
            } else {
                format!("wrong code ({left} attempts left)")
            });
        }
        pairing.paired = Some(name.to_string());

        let id = format!("c-{}", hex(&self.random_bytes::<4>()));
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.random_bytes::<32>());
        let now = unix_now();
        inner.clients.push(Client {
            id: id.clone(),
            name: name.to_string(),
            token_sha256: sha256_hex(token.as_bytes()),
            created: now,
            last_seen: now,
        });
        self.save(&inner);
        Ok(Paired { client_id: id, token })
    }

    /* Which client this key belongs to, if any. */
    pub fn authenticate(&self, token: &str) -> Option<ClientInfo> {
        let hash = sha256_hex(token.as_bytes());
        let mut inner = self.lock();
        /* Every stored hash is compared, in constant time, whether or not
         * an earlier one matched: the time taken says nothing about which
         * part of which hash was right. */
        let mut found = None;
        for (i, client) in inner.clients.iter().enumerate() {
            if same(hash.as_bytes(), client.token_sha256.as_bytes()) {
                found = Some(i);
            }
        }
        let i = found?;
        let now = unix_now();
        let save = now.saturating_sub(inner.clients[i].last_seen) >= LAST_SEEN_SAVE_INTERVAL;
        inner.clients[i].last_seen = now;
        if save {
            self.save(&inner);
        }
        Some(info(&inner.clients[i]))
    }

    pub fn list(&self) -> Vec<ClientInfo> {
        self.lock().clients.iter().map(info).collect()
    }

    /* Removes a paired client; its open connections are closed (ws.rs
     * hears it through `revocations`). */
    pub fn revoke(&self, id: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let before = inner.clients.len();
        inner.clients.retain(|c| c.id != id);
        if inner.clients.len() == before {
            return Err(format!("unknown client {id:?}"));
        }
        self.save(&inner);
        drop(inner);
        let _ = self.revoked_tx.send(id.to_string());
        Ok(())
    }

    /* Ids of clients as they're revoked. */
    pub fn revocations(&self) -> broadcast::Receiver<String> {
        self.revoked_tx.subscribe()
    }

    fn save(&self, inner: &Inner) {
        self.save_tx.send_replace(encode_clients(&inner.clients));
    }

    /* A uniformly random 6-digit code. Rejection sampling: numbers from
     * the top end of the u32 range that would make some codes slightly
     * more likely than others are simply drawn again. */
    fn random_code(&self) -> String {
        const LIMIT: u32 = u32::MAX - (u32::MAX % 1_000_000);
        loop {
            let n = u32::from_le_bytes(self.random_bytes::<4>());
            if n < LIMIT {
                return format!("{:06}", n % 1_000_000);
            }
        }
    }

    fn random_bytes<const N: usize>(&self) -> [u8; N] {
        let mut bytes = [0u8; N];
        /* The kernel's random generator (getrandom); it only fails if the
         * system is badly broken, and then no key must be made at all. */
        self.rng.fill(&mut bytes).expect("system random generator failed");
        bytes
    }
}

fn status_of(pairing: Option<&Pairing>) -> PairingStatus {
    let Some(p) = pairing else {
        return PairingStatus {
            state: "none",
            code: None,
            seconds_left: 0,
            client_name: None,
        };
    };
    let left = p.expires.saturating_duration_since(Instant::now()).as_secs();
    let state = if p.paired.is_some() {
        "paired"
    } else if p.wrong >= MAX_WRONG_CODES {
        "locked"
    } else if Instant::now() >= p.expires {
        "expired"
    } else {
        "waiting"
    };
    PairingStatus {
        state,
        code: (state == "waiting").then(|| p.code.clone()),
        seconds_left: if state == "waiting" { left } else { 0 },
        client_name: p.paired.clone(),
    }
}

fn info(c: &Client) -> ClientInfo {
    ClientInfo {
        id: c.id.clone(),
        name: c.name.clone(),
        created: c.created,
        last_seen: c.last_seen,
    }
}

/* Compares two byte strings in constant time: always looks at every byte,
 * so how long it takes doesn't reveal how many leading bytes matched
 * (which would let an attacker find a code or hash byte by byte). */
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn sha256_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/* ------------------------------------------------------------------ */
/* clients.json (store.rs), schema 1: a JSON list of clients           */
/* ------------------------------------------------------------------ */

pub const CLIENTS_SCHEMA: u32 = 1;

/* A stored client, opaque outside this file (only decode/new use it). */
pub struct StoredClient(Client);

fn encode_clients(clients: &[Client]) -> Vec<u8> {
    serde_json::to_vec_pretty(clients).expect("clients serialize")
}

pub fn decode_clients(schema: u32, payload: &[u8]) -> Result<Vec<StoredClient>, String> {
    match schema {
        1 => serde_json::from_slice::<Vec<Client>>(payload)
            .map(|list| list.into_iter().map(StoredClient).collect())
            .map_err(|e| format!("invalid client list: {e}")),
        other => Err(format!("client list schema {other} is newer than this daemon understands")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> (Auth, watch::Receiver<Vec<u8>>) {
        let (save_tx, save_rx) = watch::channel(Vec::new());
        (Auth::new(Vec::new(), save_tx), save_rx)
    }

    #[test]
    fn pair_then_authenticate() {
        let (a, mut saves) = auth();
        let status = a.start_pairing();
        assert_eq!(status.state, "waiting");
        let code = status.code.unwrap();
        assert_eq!(code.len(), 6);

        let paired = a.pair(&code, "Phone").unwrap();
        assert!(saves.has_changed().unwrap());
        /* The file has the hash, never the key. */
        let file = String::from_utf8(saves.borrow_and_update().clone()).unwrap();
        assert!(!file.contains(&paired.token));
        assert!(file.contains(&sha256_hex(paired.token.as_bytes())));

        let who = a.authenticate(&paired.token).unwrap();
        assert_eq!((who.id.as_str(), who.name.as_str()), (paired.client_id.as_str(), "Phone"));
        assert!(a.authenticate("not-a-token").is_none());
        assert_eq!(a.pairing_status().state, "paired");
        assert_eq!(a.pairing_status().client_name.as_deref(), Some("Phone"));
    }

    #[test]
    fn a_code_works_only_once() {
        let (a, _) = auth();
        let code = a.start_pairing().code.unwrap();
        a.pair(&code, "Phone").unwrap();
        /* Replaying the same code: refused. */
        assert!(a.pair(&code, "Attacker").unwrap_err().contains("already used"));
        assert_eq!(a.list().len(), 1);
    }

    #[test]
    fn wrong_codes_lock_the_pairing() {
        let (a, _) = auth();
        let code = a.start_pairing().code.unwrap();
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for left in (1..MAX_WRONG_CODES).rev() {
            assert_eq!(a.pair(wrong, "X").unwrap_err(), format!("wrong code ({left} attempts left)"));
        }
        assert!(a.pair(wrong, "X").unwrap_err().contains("too many"));
        /* Even the right code no longer works. */
        assert!(a.pair(&code, "X").unwrap_err().contains("too many"));
        assert_eq!(a.pairing_status().state, "locked");
        assert!(a.list().is_empty());
    }

    #[test]
    fn expired_and_missing_pairings_are_refused() {
        let (a, _) = auth();
        assert!(a.pair("123456", "X").unwrap_err().contains("no pairing in progress"));
        let code = a.start_pairing_for(Duration::ZERO).code;
        /* Expired at once: no code is even shown. */
        assert!(code.is_none());
        assert_eq!(a.pairing_status().state, "expired");
        assert!(a.pair("123456", "X").unwrap_err().contains("expired"));
    }

    #[test]
    fn revoked_clients_are_refused_and_announced() {
        let (a, _) = auth();
        let mut revoked = a.revocations();
        let code = a.start_pairing().code.unwrap();
        let paired = a.pair(&code, "Old phone").unwrap();
        a.revoke(&paired.client_id).unwrap();
        assert!(a.authenticate(&paired.token).is_none());
        assert_eq!(revoked.try_recv().unwrap(), paired.client_id);
        assert!(a.revoke(&paired.client_id).is_err());
    }

    #[test]
    fn names_are_checked() {
        let (a, _) = auth();
        let code = a.start_pairing().code.unwrap();
        assert!(a.pair(&code, "").is_err());
        assert!(a.pair(&code, &"x".repeat(41)).is_err());
        assert!(a.pair(&code, "a\nb").is_err());
        /* A bad name doesn't use up an attempt. */
        assert!(a.pair(&code, "Phone").is_ok());
    }

    #[test]
    fn clients_survive_a_restart() {
        let (a, mut saves) = auth();
        let code = a.start_pairing().code.unwrap();
        let paired = a.pair(&code, "Phone").unwrap();
        let stored = decode_clients(CLIENTS_SCHEMA, &saves.borrow_and_update()).unwrap();
        let (save_tx, _) = watch::channel(Vec::new());
        let restarted = Auth::new(stored, save_tx);
        assert_eq!(restarted.authenticate(&paired.token).unwrap().name, "Phone");
    }

    #[test]
    fn codes_are_six_digits_and_vary() {
        let (a, _) = auth();
        let codes: std::collections::HashSet<String> = (0..20).map(|_| a.random_code()).collect();
        assert!(codes.iter().all(|c| c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit())));
        assert!(codes.len() > 15);
    }

    #[test]
    fn constant_time_compare() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
