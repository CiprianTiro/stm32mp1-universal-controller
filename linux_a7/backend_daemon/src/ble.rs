/*
 * ble.rs -- setting the hub up over Bluetooth Low Energy (issue #36): the
 * phone app sends the home WiFi's name and password over Bluetooth, and is
 * paired with the hub (#35) in the same step.
 *
 * WHEN: the hub is only visible over Bluetooth while a pairing code is
 * shown on its screen (Settings -> Paired devices -> Pair a new device),
 * and for as long as a setup that began then is still running (see
 * `busy_until`). The rest of the time the Bluetooth radio is off.
 *
 * WHY THE PROTECTION IS IN THE PROTOCOL: Bluetooth traffic can be picked up
 * by anyone within ~10 m, and Bluetooth LE's own "Just Works" pairing
 * doesn't stop someone in the middle. So the hub and the app agree on an
 * encryption key with SPAKE2, a "password-authenticated key exchange": both
 * derive the SAME key only if both used the SAME code -- the code on the
 * hub's screen -- and the code itself is never sent. Someone listening
 * learns nothing; someone pretending to be the hub or the app gets exactly
 * one guess per attempt and can't test codes offline. Each attempt counts
 * against the pairing's 5 tries (auth.rs), so with a 6-digit code that's a
 * 1-in-200,000 chance, as for typed codes. (Matter, the smart-home
 * standard, protects its setup the same way.) The WiFi details then travel
 * encrypted and tamper-proof with that key (AES-256-GCM).
 *
 * THE PROTOCOL. One GATT service with two characteristics: the app WRITES
 * requests to RX and READS the answer from TX (reading again until the
 * answer it waits for is there). Requests are framed [type][length: 2
 * bytes, big-endian][payload], so a request split over several Bluetooth
 * writes is put back together (see Session::push).
 *
 *   app -> RX  0x01 START   SPAKE2 message A (33 bytes, identities
 *                           "uc-app" / "uc-hub", code = password)
 *   hub -> TX  0x01         SPAKE2 message B (33) + confirm (32): HMAC with
 *                           the derived confirm key over A||B -- the app
 *                           checks it and knows whether the code was right
 *                           BEFORE sending anything secret
 *   app -> RX  0x02 SETUP   nonce (12) + AES-GCM of
 *                           {"ssid", "password", "client_name"}
 *   hub -> TX  0x02         nonce + AES-GCM of {"client_id", "token",
 *                           "hub_fingerprint"}: the app is paired (#35),
 *                           and the hub starts joining the WiFi
 *   hub -> TX  0x03         nonce + AES-GCM of {"wifi": "joined"} or
 *                           {"wifi": "failed", "error": "..."}
 *   hub -> TX  0xFF         an error, plain text (no pairing running, too
 *                           many attempts, couldn't decrypt, ...)
 *
 * Keys: HKDF-SHA256 over the SPAKE2 result, salt "uc-ble-v1", info
 * "enc" -> the AES-256-GCM key, "confirm" -> the HMAC-SHA256 key. Every
 * AES-GCM message has a fresh random nonce and the associated data
 * "uc-ble-v1". tools/ble_setup.py is a working client, and the reference
 * for the phone app.
 */
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicRead, CharacteristicWrite,
    CharacteristicWriteMethod, ReqError, Service,
};
use bluer::Address;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::auth::Auth;
use crate::network;

/* The service's identifiers (random, ours). The phone app looks for the
 * service UUID when scanning. */
pub const SERVICE_UUID: bluer::Uuid = bluer::Uuid::from_u128(0x7f1c_0000_5a3b_4c6e_9d2a_1b3c_5d7e_9f00);
pub const RX_UUID: bluer::Uuid = bluer::Uuid::from_u128(0x7f1c_0001_5a3b_4c6e_9d2a_1b3c_5d7e_9f00);
pub const TX_UUID: bluer::Uuid = bluer::Uuid::from_u128(0x7f1c_0002_5a3b_4c6e_9d2a_1b3c_5d7e_9f00);

const SALT: &[u8] = b"uc-ble-v1";
const AAD: &[u8] = b"uc-ble-v1";
const ID_APP: &[u8] = b"uc-app";
const ID_HUB: &[u8] = b"uc-hub";
/* After the WiFi details arrived, Bluetooth stays on this long for the app
 * to read the answers (the WiFi attempt takes up to ~25 s)... */
const SETUP_GRACE: Duration = Duration::from_secs(60);
/* ...and after the WiFi result is known, this long more. */
const RESULT_GRACE: Duration = Duration::from_secs(15);

/* The longest request accepted (a WiFi password is at most 63 bytes; this
 * leaves plenty of room for the JSON and the encryption overhead). */
const MAX_REQUEST: usize = 1024;

/* The two keys derived from one exchange. */
struct Keys {
    enc: LessSafeKey,
    confirm: ring::hmac::Key,
}

fn derive(spake_key: &[u8]) -> Keys {
    let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, SALT).extract(spake_key);
    let expand = |info: &[u8]| {
        let mut out = [0u8; 32];
        /* 32 bytes from HKDF-SHA256 can't fail (the limit is 255 * 32). */
        prk.expand(&[info], ring::hkdf::HKDF_SHA256)
            .and_then(|okm| okm.fill(&mut out))
            .expect("HKDF expand of 32 bytes");
        out
    };
    Keys {
        enc: LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &expand(b"enc")).expect("a 32-byte AES key")),
        confirm: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &expand(b"confirm")),
    }
}

/* nonce || ciphertext+tag */
fn seal(key: &LessSafeKey, plaintext: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 12];
    SystemRandom::new().fill(&mut nonce).expect("system random generator failed");
    let mut data = plaintext.to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(AAD), &mut data)
        .expect("AES-GCM seal");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&data);
    out
}

fn open(key: &LessSafeKey, data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 12 + 16 {
        return None;
    }
    let (nonce, rest) = data.split_at(12);
    let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
    let mut rest = rest.to_vec();
    let plain = key.open_in_place(nonce, Aad::from(AAD), &mut rest).ok()?;
    Some(plain.to_vec())
}

/* One app's setup, from START to the WiFi result. */
#[derive(Default)]
struct Session {
    /* Which Bluetooth device this session belongs to; a request from
     * another one starts over. */
    device: Option<Address>,
    /* A request being put back together from several writes. */
    buffer: Vec<u8>,
    /* After START: the encryption key and the code it was made with. */
    enc: Option<LessSafeKey>,
    code: Option<String>,
    /* What a read of TX returns. */
    response: Vec<u8>,
}

impl Session {
    /* Adds one write's bytes at `offset` (Bluetooth splits long writes);
     * returns the complete request once all its bytes are there. */
    fn push(&mut self, value: &[u8], offset: usize) -> Result<Option<(u8, Vec<u8>)>, String> {
        if offset == 0 {
            self.buffer.clear();
        }
        if offset != self.buffer.len() || offset + value.len() > MAX_REQUEST {
            self.buffer.clear();
            return Err("malformed request".into());
        }
        self.buffer.extend_from_slice(value);
        if self.buffer.len() < 3 {
            return Ok(None);
        }
        let length = usize::from(u16::from_be_bytes([self.buffer[1], self.buffer[2]]));
        match self.buffer.len().cmp(&(3 + length)) {
            std::cmp::Ordering::Less => Ok(None),
            std::cmp::Ordering::Equal => {
                let request = (self.buffer[0], self.buffer[3..].to_vec());
                self.buffer.clear();
                Ok(Some(request))
            }
            std::cmp::Ordering::Greater => {
                self.buffer.clear();
                Err("malformed request".into())
            }
        }
    }
}

#[derive(Deserialize)]
struct Setup {
    ssid: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    client_name: String,
}

pub struct Ble {
    auth: Arc<Auth>,
    network_tx: mpsc::Sender<network::Cmd>,
    fingerprint: String,
    /* The name the hub advertises (the same as its setup hotspot's). */
    name: String,
    session: Mutex<Session>,
    /* Bluetooth stays on until then, even though the pairing code is gone
     * (the successful pairing ends it). Found on the DK2: the hub switched
     * Bluetooth off the moment the app was paired, while the app was still
     * reading the answer ("Unlikely Error" on its side). */
    busy_until: Mutex<Option<std::time::Instant>>,
}

impl Ble {
    pub fn new(auth: Arc<Auth>, network_tx: mpsc::Sender<network::Cmd>, fingerprint: String, name: String) -> Arc<Self> {
        Arc::new(Ble {
            auth,
            network_tx,
            fingerprint,
            name,
            session: Mutex::new(Session::default()),
            busy_until: Mutex::new(None),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|p| p.into_inner())
    }

    /* Keeps Bluetooth on for at least `grace` from now. */
    fn stay_on_for(&self, grace: Duration) {
        let until = std::time::Instant::now() + grace;
        let mut busy = self.busy_until.lock().unwrap_or_else(|p| p.into_inner());
        if busy.map_or(true, |b| b < until) {
            *busy = Some(until);
        }
    }

    fn busy(&self) -> bool {
        let busy = self.busy_until.lock().unwrap_or_else(|p| p.into_inner());
        busy.is_some_and(|until| std::time::Instant::now() < until)
    }

    /* A write to RX. */
    fn on_write(self: &Arc<Self>, device: Address, value: &[u8], offset: usize) {
        let mut session = self.lock();
        if session.device != Some(device) {
            *session = Session {
                device: Some(device),
                ..Session::default()
            };
        }
        let request = match session.push(value, offset) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(e) => {
                session.response = error(&e);
                return;
            }
        };
        session.response = match request {
            (0x01, message_a) => self.start(&mut session, &message_a),
            (0x02, sealed) => self.setup(&mut session, &sealed),
            _ => error("unknown request"),
        };
    }

    /* START: the key exchange. */
    fn start(&self, session: &mut Session, message_a: &[u8]) -> Vec<u8> {
        session.enc = None;
        session.code = None;
        let code = match self.auth.begin_code_attempt() {
            Ok(code) => code,
            Err(e) => return error(&e),
        };
        let (state, message_b) =
            Spake2::<Ed25519Group>::start_b(&Password::new(code.as_bytes()), &Identity::new(ID_APP), &Identity::new(ID_HUB));
        let Ok(key) = state.finish(message_a) else {
            return error("malformed key exchange message");
        };
        let keys = derive(&key);
        let mut transcript = message_a.to_vec();
        transcript.extend_from_slice(&message_b);
        let confirm = ring::hmac::sign(&keys.confirm, &transcript);
        session.enc = Some(keys.enc);
        session.code = Some(code);
        let mut response = vec![0x01];
        response.extend_from_slice(&message_b);
        response.extend_from_slice(confirm.as_ref());
        response
    }

    /* SETUP: the encrypted WiFi details. */
    fn setup(self: &Arc<Self>, session: &mut Session, sealed: &[u8]) -> Vec<u8> {
        let (Some(enc), Some(code)) = (session.enc.take(), session.code.take()) else {
            return error("send START first");
        };
        let Some(plain) = open(&enc, sealed) else {
            /* The app's key differs: it used another code (the attempt was
             * already counted at START). */
            println!("ble: setup message could not be decrypted (wrong code?)");
            return error("could not decrypt: was the code right?");
        };
        let setup: Setup = match serde_json::from_slice(&plain) {
            Ok(s) => s,
            Err(e) => return error(&format!("invalid setup message: {e}")),
        };
        let name = if setup.client_name.trim().is_empty() { "Phone (Bluetooth setup)" } else { setup.client_name.trim() };
        let paired = match self.auth.pair_proven(&code, name) {
            Ok(p) => p,
            Err(e) => return error(&e),
        };
        println!("ble: {name:?} paired as {} over Bluetooth, joining WiFi {:?}", paired.client_id, setup.ssid);
        self.stay_on_for(SETUP_GRACE);
        let reply = serde_json::json!({
            "client_id": paired.client_id,
            "token": paired.token,
            "hub_fingerprint": self.fingerprint,
        });
        let mut response = vec![0x02];
        response.extend_from_slice(&seal(&enc, reply.to_string().as_bytes()));

        /* Join the WiFi in the background; the result replaces the
         * response when it's there (the app keeps reading). */
        let ble = self.clone();
        tokio::spawn(async move {
            let result = connect(&ble.network_tx, setup.ssid.clone(), setup.password).await;
            let body = match &result {
                Ok(()) => serde_json::json!({ "wifi": "joined" }),
                Err(e) => serde_json::json!({ "wifi": "failed", "error": e }),
            };
            match &result {
                Ok(()) => println!("ble: the hub joined {:?}", setup.ssid),
                Err(e) => println!("ble: joining {:?} failed: {e}", setup.ssid),
            }
            let mut response = vec![0x03];
            response.extend_from_slice(&seal(&enc, body.to_string().as_bytes()));
            ble.lock().response = response;
            ble.stay_on_for(RESULT_GRACE);
        });
        response
    }

    /* The GATT application: the service with RX and TX. */
    fn application(self: &Arc<Self>) -> Application {
        let writer = self.clone();
        let reader = self.clone();
        Application {
            services: vec![Service {
                uuid: SERVICE_UUID,
                primary: true,
                characteristics: vec![
                    Characteristic {
                        uuid: RX_UUID,
                        write: Some(CharacteristicWrite {
                            write: true,
                            method: CharacteristicWriteMethod::Fun(Box::new(move |value, request| {
                                writer.on_write(request.device_address, &value, usize::from(request.offset));
                                Box::pin(async { Ok(()) })
                            })),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    Characteristic {
                        uuid: TX_UUID,
                        read: Some(CharacteristicRead {
                            read: true,
                            fun: Box::new(move |request| {
                                /* Long values are read in pieces, each
                                 * starting at `offset`. */
                                let session = reader.lock();
                                let result = if session.device == Some(request.device_address) {
                                    let offset = usize::from(request.offset).min(session.response.len());
                                    Ok(session.response[offset..].to_vec())
                                } else {
                                    Err(ReqError::NotPermitted)
                                };
                                Box::pin(async move { result })
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

fn error(message: &str) -> Vec<u8> {
    let mut response = vec![0xFF];
    response.extend_from_slice(message.as_bytes());
    response
}

async fn connect(network_tx: &mpsc::Sender<network::Cmd>, ssid: String, password: String) -> Result<(), String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    network_tx
        .send(network::Cmd::Connect {
            ssid,
            password: Some(password),
            reply: reply_tx,
        })
        .await
        .map_err(|_| "network actor unavailable".to_string())?;
    reply_rx.await.map_err(|_| "network actor dropped the reply".to_string())?
}

/* Runs for the daemon's life: switches Bluetooth on and offers the setup
 * service while a pairing code is shown, and switches it off again
 * afterwards. Checks once a second. */
pub async fn run(ble: Arc<Ble>) {
    let adapter = match async {
        let session = bluer::Session::new().await?;
        session.default_adapter().await
    }
    .await
    {
        Ok(adapter) => adapter,
        Err(e) => {
            println!("ble: Bluetooth not available ({e}); Bluetooth setup disabled");
            return;
        }
    };
    /* While Some: the service is offered and the hub is advertised; the
     * handles unregister both when dropped. */
    let mut offered: Option<(ApplicationHandle, bluer::adv::AdvertisementHandle)> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let waiting = ble.auth.pairing_status().state == "waiting" || ble.busy();
        if waiting && offered.is_none() {
            let result = async {
                adapter.set_powered(true).await?;
                let app = adapter.serve_gatt_application(ble.application()).await?;
                let advertisement = bluer::adv::Advertisement {
                    advertisement_type: bluer::adv::Type::Peripheral,
                    service_uuids: [SERVICE_UUID].into_iter().collect(),
                    local_name: Some(ble.name.clone()),
                    discoverable: Some(true),
                    ..Default::default()
                };
                let adv = adapter.advertise(advertisement).await?;
                bluer::Result::Ok((app, adv))
            }
            .await;
            match result {
                Ok(handles) => {
                    println!("ble: Bluetooth setup available as {:?}", ble.name);
                    offered = Some(handles);
                }
                Err(e) => println!("ble: could not offer Bluetooth setup: {e}"),
            }
        } else if !waiting && offered.is_some() {
            offered = None;
            *ble.lock() = Session::default();
            let _ = adapter.set_powered(false).await;
            println!("ble: Bluetooth setup closed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* Plays the app's side of the protocol against the hub's functions,
     * without Bluetooth: the whole exchange, as tools/ble_setup.py does it. */
    fn app_start(code: &str) -> (spake2::Spake2<Ed25519Group>, Vec<u8>) {
        Spake2::<Ed25519Group>::start_a(&Password::new(code.as_bytes()), &Identity::new(ID_APP), &Identity::new(ID_HUB))
    }

    fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn setup_ble() -> (Arc<Ble>, Arc<Auth>, String) {
        let (save_tx, _) = tokio::sync::watch::channel(Vec::new());
        let auth = Arc::new(Auth::new(Vec::new(), save_tx));
        let code = auth.start_pairing().code.unwrap();
        let (network_tx, _) = mpsc::channel(1);
        let ble = Ble::new(auth.clone(), network_tx, "fp".into(), "UC-Setup-TEST".into());
        (ble, auth, code)
    }

    const DEVICE: Address = Address::new([1, 2, 3, 4, 5, 6]);

    #[tokio::test]
    async fn full_exchange_with_the_right_code() {
        let (ble, auth, code) = setup_ble();
        let (app, message_a) = app_start(&code);
        ble.on_write(DEVICE, &frame(0x01, &message_a), 0);
        let response = ble.lock().response.clone();
        assert_eq!(response[0], 0x01);
        let (message_b, confirm) = response[1..].split_at(33);

        /* The app derives the same keys and checks the hub's confirm. */
        let keys = derive(&app.finish(message_b).unwrap());
        let mut transcript = message_a.clone();
        transcript.extend_from_slice(message_b);
        assert!(ring::hmac::verify(&keys.confirm, &transcript, confirm).is_ok());

        /* The encrypted WiFi details, split over two writes. */
        let sealed = seal(&keys.enc, br#"{"ssid":"Home","password":"secret12","client_name":"Test phone"}"#);
        let request = frame(0x02, &sealed);
        let (first, second) = request.split_at(10);
        ble.on_write(DEVICE, first, 0);
        ble.on_write(DEVICE, second, 10);
        let response = ble.lock().response.clone();
        assert_eq!(response[0], 0x02, "{:?}", String::from_utf8_lossy(&response));
        let reply: serde_json::Value = serde_json::from_slice(&open(&keys.enc, &response[1..]).unwrap()).unwrap();
        /* The app got a working key for the LAN (#35). */
        let token = reply["token"].as_str().unwrap();
        assert_eq!(auth.authenticate(token).unwrap().name, "Test phone");
        assert_eq!(reply["hub_fingerprint"], "fp");
    }

    #[tokio::test]
    async fn a_wrong_code_is_detected_and_counted() {
        let (ble, auth, code) = setup_ble();
        let wrong = if code == "000000" { "111111" } else { "000000" };
        let (app, message_a) = app_start(wrong);
        ble.on_write(DEVICE, &frame(0x01, &message_a), 0);
        let response = ble.lock().response.clone();
        let (message_b, confirm) = response[1..].split_at(33);
        let keys = derive(&app.finish(message_b).unwrap());
        let mut transcript = message_a.clone();
        transcript.extend_from_slice(message_b);
        /* The app sees at once that the code was wrong... */
        assert!(ring::hmac::verify(&keys.confirm, &transcript, confirm).is_err());
        /* ...and even if it sent its details anyway, the hub can't read them. */
        ble.on_write(DEVICE, &frame(0x02, &seal(&keys.enc, b"{}")), 0);
        assert_eq!(ble.lock().response[0], 0xFF);
        assert!(auth.list().is_empty());
    }

    #[tokio::test]
    async fn attempts_are_limited() {
        let (ble, _, code) = setup_ble();
        for _ in 0..5 {
            ble.on_write(DEVICE, &frame(0x01, &app_start(&code).1), 0);
            assert_eq!(ble.lock().response[0], 0x01);
        }
        ble.on_write(DEVICE, &frame(0x01, &app_start(&code).1), 0);
        let response = ble.lock().response.clone();
        assert_eq!(response[0], 0xFF);
        assert!(String::from_utf8_lossy(&response).contains("too many"));
    }

    #[test]
    fn requests_are_reassembled_and_bounded() {
        let mut s = Session::default();
        assert_eq!(s.push(&[0x01, 0x00], 0), Ok(None));
        assert_eq!(s.push(&[0x02, 0xAA, 0xBB], 2), Ok(Some((0x01, vec![0xAA, 0xBB]))));
        /* A write at the wrong offset, or longer than announced: refused. */
        assert!(s.push(&[0x01, 0x00, 0x01], 5).is_err());
        assert!(s.push(&[0x01, 0x00, 0x01, 0xAA, 0xBB], 0).is_err());
        assert!(s.push(&vec![0u8; MAX_REQUEST + 1], 0).is_err());
    }
}
