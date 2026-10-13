/*
 * addon.rs -- what the hub's own ESP32 add-ons have in common (issue #42;
 * shared since #86): the IR blaster (ir_blaster.rs) and the room sensor
 * (room_sensor.rs). Their firmware shares the same code too
 * (firmware_common/uc_addon/), and the protocol is the IR blaster's
 * (firmware_ir_blaster/PROTOCOL.md; firmware_sensor/PROTOCOL.md lists the
 * sensor's differences).
 *
 * Here:
 *   open / SetupLink   a pinned mutual-TLS connection (the hub connects,
 *                      with its own identity certificate), one JSON object
 *                      per line, request -> reply by "id"
 *   probe              the wizard's test step: "hello" through the pinned
 *                      connection
 *   pair               the wizard's code_from_device step: both sides prove
 *                      they know the pairing code, bound to this very
 *                      connection's certificates -> pin
 *   provision          the wizard's provision_ble step: the WiFi over
 *                      Bluetooth (esp_prov/), then pair
 *   read_lines         a device task's reader: lines -> a channel
 *
 * The add-ons differ only in names (a `Kind`): the pairing label, the
 * Bluetooth service, what the person calls it. A proof made for one kind
 * is never valid for another.
 */
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;

use super::net::{self, NetError};
use super::{Probe, SetupError, SetupValues};
use crate::templates::ErrorKind;
use crate::tls;

/* The add-on's TCP port (PROTOCOL.md). */
pub const PORT: u16 = 7443;

/* The add-on closes a connection silent for 60 s: ping well before. */
pub const PING_EVERY: Duration = Duration::from_secs(20);
/* Nothing heard for this long (not even a ping reply) = gone. */
pub const SILENT_LIMIT: Duration = Duration::from_secs(45);
/* How long a request may take. A blaster's send is ~0.1 s, with 20 NEC
 * repeats ~2.3 s; a sensor in modem sleep answers within ~0.3 s. */
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/* Waiting between connection attempts: from RETRY_MIN, doubling up to
 * RETRY_MAX. */
pub const RETRY_MIN: Duration = Duration::from_secs(2);
pub const RETRY_MAX: Duration = Duration::from_secs(60);
/* The longest line accepted from an add-on (PROTOCOL.md: 16 KiB). */
pub const MAX_LINE: usize = 16 * 1024;

pub type Stream = TlsStream<TcpStream>;

/* One kind of add-on: its names (firmware: uc_addon_config_t). */
pub struct Kind {
    /* The adapter's id, for the log: "ir-blaster". */
    pub adapter: &'static str,
    /* What a person calls it: "IR blaster" (the test step's summary). */
    pub product: &'static str,
    /* "blaster", "sensor": in sentences ("the blaster refused"), and its
     * role in the pairing proof. */
    pub short: &'static str,
    /* The pairing proof's label: "uc-irb-pair-v1". */
    pub pair_label: &'static str,
    /* The Bluetooth setup service, when the template's provision_ble step
     * doesn't name one. */
    pub ble_service: &'static str,
    /* The config key the add-on's id is kept under: "blaster_id". */
    pub id_key: &'static str,
}

/* Opens a connection: pinned to `pin` (None = any certificate, for
 * pairing), always with the hub's own certificate. */
pub async fn open(host: &str, pin: Option<&str>) -> Result<(Stream, String), NetError> {
    let me = tls::client_identity(&tls::tls_dir()).map_err(|e| NetError::new(ErrorKind::Unsupported, e))?;
    net::tls_open(host, PORT, pin, Some(&me)).await
}

/* A request/reply connection for the wizard's steps. */
pub struct SetupLink {
    stream: BufReader<Stream>,
    next_id: u64,
    short: &'static str,
}

impl SetupLink {
    pub async fn request(&mut self, op: &str, fields: Value) -> Result<Value, NetError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({"id": id, "op": op});
        if let (Some(msg), Some(fields)) = (msg.as_object_mut(), fields.as_object()) {
            msg.extend(fields.clone());
        }
        let lost = |e: String| NetError::new(ErrorKind::Unreachable, e);
        let short = self.short;
        let exchange = async {
            let mut line = msg.to_string();
            line.push('\n');
            self.stream.get_mut().write_all(line.as_bytes()).await.map_err(|e| lost(e.to_string()))?;
            loop {
                let mut reply = Vec::new();
                let n = (&mut self.stream)
                    .take(MAX_LINE as u64 + 1)
                    .read_until(b'\n', &mut reply)
                    .await
                    .map_err(|e| lost(e.to_string()))?;
                if n == 0 {
                    /* A paired add-on closes the connection of a hub it
                     * doesn't know right after the handshake. */
                    return Err(NetError::new(ErrorKind::Refused, format!("the {short} closed the connection")));
                }
                let reply: Value = serde_json::from_slice(&reply).map_err(|e| lost(format!("bad reply: {e}")))?;
                if reply["id"] == json!(id) {
                    return Ok(reply);
                }
                /* An event ("heard", "reading") meanwhile: not ours. */
            }
        };
        match tokio::time::timeout(REPLY_TIMEOUT, exchange).await {
            Ok(result) => result,
            Err(_) => Err(NetError::new(ErrorKind::Timeout, format!("no answer to {op}"))),
        }
    }
}

pub async fn setup_link(kind: &Kind, host: &str, pin: Option<&str>) -> Result<(SetupLink, String), NetError> {
    let (stream, fingerprint) = open(host, pin).await?;
    Ok((
        SetupLink {
            stream: BufReader::new(stream),
            next_id: 1,
            short: kind.short,
        },
        fingerprint,
    ))
}

/* The wizard's test step: the pinned add-on answers "hello" and says
 * it's paired. Also returns the hello itself, for what the add-on adds. */
pub async fn probe(kind: &Kind, values: &SetupValues) -> Result<(Probe, SetupLink), SetupError> {
    let host = values
        .plain
        .get("host")
        .ok_or_else(|| SetupError::new(ErrorKind::Unreachable, "no address"))?;
    let pin = values
        .plain
        .get("fingerprint")
        .ok_or_else(|| SetupError::new(ErrorKind::Refused, "not paired yet"))?;
    let (mut link, _) = setup_link(kind, host, Some(pin)).await?;
    let hello = link.request("hello", json!({})).await?;
    if hello["paired"] != json!(true) {
        return Err(SetupError::new(ErrorKind::Refused, format!("the {} forgot the pairing", kind.short)));
    }
    let device_id = hello["device"].as_str().unwrap_or_default().to_string();
    let fw = hello["fw"].as_str().unwrap_or("?");
    let probe = Probe {
        values: [(kind.id_key.to_string(), device_id.clone())].into(),
        name: None,
        summary: format!("{} {device_id}, firmware {fw}", kind.product),
    };
    Ok((probe, link))
}

/* ------------------------------------------------------------------ */
/* Pairing (PROTOCOL.md, "Pairing")                                    */
/* ------------------------------------------------------------------ */

/* The code as the add-on keeps it: 16 characters of Crockford base32,
 * upper case, without the dashes people type. */
pub fn normalise_code(code: &str) -> Option<String> {
    let code: String = code
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid = code.len() == 16 && code.chars().all(|c| c.is_ascii_digit() || (c.is_ascii_uppercase() && !"ILOU".contains(c)));
    valid.then_some(code)
}

/* "<label> <role> <FA> <FH>": FA the add-on's certificate fingerprint,
 * FH the hub's. */
fn proof_message(label: &str, role: &str, addon_fp: &str, hub_fp: &str) -> String {
    format!("{label} {role} {addon_fp} {hub_fp}")
}

pub fn proof(label: &str, code: &str, role: &str, addon_fp: &str, hub_fp: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, code.as_bytes());
    let tag = ring::hmac::sign(&key, proof_message(label, role, addon_fp, hub_fp).as_bytes());
    tag.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/* Checks the add-on's proof in constant time (ring::hmac::verify). */
pub fn proof_is_valid(kind: &Kind, code: &str, addon_fp: &str, hub_fp: &str, hex_proof: &str) -> bool {
    let Some(bytes) = decode_hex(hex_proof) else { return false };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, code.as_bytes());
    let message = proof_message(kind.pair_label, kind.short, addon_fp, hub_fp);
    ring::hmac::verify(&key, message.as_bytes(), &bytes).is_ok()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn code_from(values: &SetupValues) -> Result<String, SetupError> {
    values
        .secret
        .get("pairing_code")
        .and_then(|c| normalise_code(c.expose()))
        .ok_or_else(|| SetupError::new(ErrorKind::Refused, "the pairing code has 16 letters and digits (XXXX-XXXX-XXXX-XXXX)"))
}

/* The wizard's code_from_device step. Returns the add-on's fingerprint
 * (pinned from now on) and its id. */
pub async fn pair(kind: &Kind, values: &SetupValues) -> Result<SetupValues, SetupError> {
    let host = values
        .plain
        .get("host")
        .ok_or_else(|| SetupError::new(ErrorKind::Unreachable, "no address"))?;
    let code = code_from(values)?;
    let hub_fp = tls::client_identity(&tls::tls_dir())
        .map_err(|e| SetupError::new(ErrorKind::Unsupported, e))?
        .fingerprint;
    let short = kind.short;

    let not_ours = || {
        SetupError::new(
            ErrorKind::Refused,
            format!("the {short} belongs to another hub: type `unpair` in its console (or reset it), then try again"),
        )
    };

    let (mut link, addon_fp) = setup_link(kind, host, None).await?;
    let hello = link.request("hello", json!({})).await.map_err(|e| match e.kind {
        ErrorKind::Refused => not_ours(),
        _ => e.into(),
    })?;
    if hello["paired"] == json!(true) {
        /* It let us in, so it's paired with THIS hub already (a device
         * removed and added again). Unpair and pair afresh: that also
         * proves, with the code, that it's the add-on the person means. */
        link.request("unpair", json!({})).await?;
        drop(link);
        let (again, fp) = setup_link(kind, host, None).await?;
        if fp != addon_fp {
            return Err(SetupError::new(ErrorKind::Refused, format!("the {short}'s certificate changed during pairing")));
        }
        link = again;
    }

    let reply = link
        .request("pair", json!({"proof": proof(kind.pair_label, &code, "hub", &addon_fp, &hub_fp)}))
        .await?;
    if reply["ok"] != json!(true) {
        let error = reply["error"].as_str().unwrap_or("failed");
        return Err(match error {
            "bad_proof" => SetupError::new(ErrorKind::Refused, "wrong pairing code"),
            "locked" => SetupError::new(ErrorKind::Refused, format!("too many wrong codes: restart the {short}, then try again")),
            _ => SetupError::new(ErrorKind::Unsupported, format!("pairing failed: {error}")),
        });
    }
    /* The add-on proves it knows the code too -- and saw the same two
     * certificates, so nobody sits in between. */
    if !proof_is_valid(kind, &code, &addon_fp, &hub_fp, reply["proof"].as_str().unwrap_or_default()) {
        return Err(SetupError::new(
            ErrorKind::Refused,
            format!("the {short}'s answer doesn't match the code: another device may be pretending to be it"),
        ));
    }

    let mut result = SetupValues::default();
    result.plain.insert("fingerprint".into(), addon_fp);
    if let Some(id) = hello["device"].as_str() {
        result.plain.insert(kind.id_key.into(), id.to_string());
    }
    Ok(result)
}

/* ------------------------------------------------------------------ */
/* A new add-on: WiFi over Bluetooth, then pairing (issue #42)         */
/* ------------------------------------------------------------------ */

/* After joining the WiFi the add-on needs a moment before its TLS server
 * answers: pairing is tried this many times, this far apart. */
const PAIR_TRIES: u32 = 10;
const PAIR_RETRY: Duration = Duration::from_secs(2);

/* The wizard's provision_ble step: the WiFi over Bluetooth, then pairing
 * over the WiFi -- with the one pairing code the person typed. */
pub async fn provision(kind: &Kind, values: &SetupValues) -> Result<SetupValues, SetupError> {
    let code = code_from(values)?;
    let ssid = values
        .plain
        .get("wifi_ssid")
        .ok_or_else(|| SetupError::new(ErrorKind::Refused, "no WiFi name"))?;
    let password = values.secret.get("wifi_password").map(|p| p.expose().to_string()).unwrap_or_default();
    let service = values.plain.get("ble_service").map(String::as_str).unwrap_or(kind.ble_service);
    let service = bluer::Uuid::parse_str(service).map_err(|e| SetupError::new(ErrorKind::Unsupported, format!("service: {e}")))?;

    use super::esp_prov::ProvError;
    let (name, ip) = super::esp_prov::ble::provision_nearby(service, &code, ssid, &password)
        .await
        .map_err(|e| match e {
            ProvError::WrongCode => SetupError::new(ErrorKind::Refused, "wrong pairing code"),
            ProvError::WifiPassword => SetupError::new(ErrorKind::Refused, "the WiFi password is wrong"),
            ProvError::WifiNotFound => SetupError::new(
                ErrorKind::Unreachable,
                format!("the {} doesn't see the WiFi \"{ssid}\" (it needs a 2.4 GHz network)", kind.short),
            ),
            ProvError::Timeout => SetupError::new(ErrorKind::Timeout, e.to_string()),
            ProvError::Link(why) => SetupError::new(ErrorKind::Unreachable, why),
        })?;
    println!("{}: {name} joined the WiFi at {ip}, pairing", kind.adapter);

    /* Now on the WiFi: pair as for an add-on found there. */
    let mut values = values.clone();
    values.plain.insert("host".into(), ip.clone());
    let mut tries = 0;
    let paired = loop {
        match pair(kind, &values).await {
            Ok(paired) => break paired,
            Err(e) if matches!(e.kind, ErrorKind::Unreachable | ErrorKind::Timeout) && tries < PAIR_TRIES => {
                tries += 1;
                tokio::time::sleep(PAIR_RETRY).await;
            }
            Err(e) => return Err(e),
        }
    };
    let mut result = paired;
    result.plain.insert("host".into(), ip);
    Ok(result)
}

/* ------------------------------------------------------------------ */
/* A device task's connection                                          */
/* ------------------------------------------------------------------ */

/* Reads lines from the add-on, forever, as their own task: a line may
 * arrive in pieces, and reading it inside the task's select! would lose
 * the pieces whenever another branch wins. Ends when the connection does
 * (then `tx` closes, which the session sees). */
pub async fn read_lines(reader: ReadHalf<Stream>, tx: mpsc::Sender<Value>) {
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            return; /* longer than MAX_LINE, or cut off: give up on it */
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            if tx.send(value).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLASTER: Kind = Kind {
        adapter: "ir-blaster",
        product: "IR blaster",
        short: "blaster",
        pair_label: "uc-irb-pair-v1",
        ble_service: "",
        id_key: "blaster_id",
    };

    #[test]
    fn codes_are_normalised() {
        assert_eq!(normalise_code("5mcn-wm19-04ch-q2gt").as_deref(), Some("5MCNWM1904CHQ2GT"));
        assert_eq!(normalise_code("5MCN WM19 04CH Q2GT").as_deref(), Some("5MCNWM1904CHQ2GT"));
        /* Too short, and letters Crockford base32 leaves out. */
        assert_eq!(normalise_code("5MCN-WM19"), None);
        assert_eq!(normalise_code("IIII-LLLL-OOOO-UUUU"), None);
    }

    /* The expected values were computed independently, with Python's
     * hmac module -- the same way tools/hub_sim.py does, whose proofs the
     * real firmware accepted on the bench. So Rust, Python and the C
     * firmware agree on PROTOCOL.md's formula. (Unchanged since the code
     * moved here for #86: an already paired blaster keeps working.) */
    #[test]
    fn proofs_match_the_protocol() {
        let code = "5MCNWM1904CHQ2GT";
        let fb = "4c3f7fa94ec5c219b549dc4baf53926057c0c05930d8d8a4b15b938048f401ac";
        let fh = "0".repeat(64);
        let hub = proof(BLASTER.pair_label, code, "hub", fb, &fh);
        assert_eq!(hub, "280d2b06ae47cf86d87090f87f1fe8f125eb7149a89f0f0b4753f9ad1de956dd");
        let blaster = proof(BLASTER.pair_label, code, "blaster", fb, &fh);
        assert_eq!(blaster, "796c229f53d5546ff701214199e60d5c1ab108e04952d18347eac2ca186410e1");
        assert!(proof_is_valid(&BLASTER, code, fb, &fh, &blaster));
        assert!(!proof_is_valid(&BLASTER, code, fb, &fh, &hub));
        assert!(!proof_is_valid(&BLASTER, "AAAAAAAAAAAAAAAA", fb, &fh, &blaster));
        assert!(!proof_is_valid(&BLASTER, code, fb, &fh, "zz"));
    }

    /* A proof made for one kind of add-on is useless for another: the
     * labels differ (#86). */
    #[test]
    fn proofs_differ_per_kind() {
        let sensor = Kind {
            short: "sensor",
            pair_label: "uc-ths-pair-v1",
            ..BLASTER
        };
        let code = "5MCNWM1904CHQ2GT";
        let fa = "4c3f7fa94ec5c219b549dc4baf53926057c0c05930d8d8a4b15b938048f401ac";
        let fh = "0".repeat(64);
        let as_blaster = proof(BLASTER.pair_label, code, "sensor", fa, &fh);
        assert!(!proof_is_valid(&sensor, code, fa, &fh, &as_blaster));
        let as_sensor = proof(sensor.pair_label, code, "sensor", fa, &fh);
        assert!(proof_is_valid(&sensor, code, fa, &fh, &as_sensor));
    }
}
