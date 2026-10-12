/*
 * roborock_proto.rs -- the Roborock vacuums' own LAN protocol, "1.0"
 * (issue #74). Pure functions, no I/O: roborock.rs does the talking.
 *
 * Roborocks set up with the Roborock app speak this on TCP 58867 (the
 * older "miIO" of Xiaomi's Mi Home app is miio.rs). Nothing official:
 * reverse-engineered by the python-roborock project, whose code this
 * follows. Verified against the project's S7.
 *
 * A FRAME (every number big-endian):
 *
 *   length  u32   of what follows (TCP only: a stream needs it to know
 *                 where one message ends)
 *   version 3 B   "1.0"
 *   seq     u32   request number (a reply carries the request's)
 *   random  u32   a random number
 *   time    u32   Unix seconds -- also part of the key, below
 *   proto   u16   what it is: 0/1 hello, 2/3 ping, 4 request/reply,
 *                 (101/102: the same through Roborock's cloud)
 *   [ size  u16   the encrypted payload's size
 *     payload     AES-128-ECB, PKCS#7 padding ]   (none for hello, ping)
 *   crc     u32   CRC-32 of everything from `version` on
 *
 * THE KEY changes with every message: MD5( scrambled time | local_key |
 * SALT ). `local_key` is the device's 16-character secret, which only
 * Roborock's cloud hands out (roborock_cloud.rs) -- that's why a Roborock
 * needs the account once (pattern P4), and never again after.
 *
 * A REQUEST's payload is JSON with JSON inside:
 *   {"dps": {"101": "{\"id\":1234,\"method\":\"get_status\",\"params\":[]}"}, "t": 1700000000}
 * and the reply's under "102": {"id": 1234, "result": [...]} or "error".
 * The vacuum also pushes changes by itself: "121" = state, "122" =
 * battery.
 *
 * BROADCASTS (UDP 58866): every few seconds a vacuum announces itself to
 * the whole network, {"duid": "...", "ip": "192.168.1.134"}, encrypted
 * with a key that is the same for everyone (so: not a secret, just how
 * it's packed). That's how the hub learns a vacuum's address
 * (discovery.rs) without scanning.
 */
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use md5::{Digest, Md5};
use serde_json::{json, Value};

use crate::device::VacuumState;
use crate::store::crc32;

pub const LOCAL_PORT: u16 = 58867;
const VERSION: &[u8; 3] = b"1.0";
const SALT: &[u8] = b"TXdfu$jyZ#TZHsg4";
/* The broadcasts' key: the same in every Roborock, published by
 * python-roborock. */
const BROADCAST_KEY: &[u8; 16] = b"qWKYcdQWrbm9hPqe";
/* Nothing the vacuum sends is anywhere near this (a status reply is
 * ~1 KB); a bigger length means we lost track of the stream. */
const MAX_FRAME: usize = 256 * 1024;

pub const HELLO_REQUEST: u16 = 0;
pub const HELLO_RESPONSE: u16 = 1;
pub const GENERAL: u16 = 4;

/* One message, payload decrypted. */
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub seq: u32,
    pub random: u32,
    pub timestamp: u32,
    pub protocol: u16,
    pub payload: Vec<u8>,
}

/* The time, its hex digits shuffled -- part of every message's key. */
fn encode_timestamp(timestamp: u32) -> Vec<u8> {
    let hex = format!("{timestamp:08x}").into_bytes();
    [5, 6, 3, 7, 1, 2, 0, 4].iter().map(|&i| hex[i]).collect()
}

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Md5::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/* A message's key (see the top of the file). */
fn message_key(timestamp: u32, local_key: &str) -> [u8; 16] {
    md5(&[&encode_timestamp(timestamp), local_key.as_bytes(), SALT])
}

/* AES-128-ECB with PKCS#7 padding: each 16-byte block on its own. (ECB is
 * a weak mode -- equal blocks give equal output -- but it's what the
 * vacuum speaks.) */
pub fn ecb_encrypt(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(pad as u8, pad));
    for block in data.chunks_mut(16) {
        cipher.encrypt_block(block.into());
    }
    data
}

pub fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(format!("encrypted payload of {} bytes isn't whole blocks", data.len()));
    }
    let cipher = Aes128::new(key.into());
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        cipher.decrypt_block(block.into());
    }
    unpad(out)
}

/* Removes PKCS#7 padding: the last byte says how many bytes were added,
 * each of that value. Anything else means a wrong key. */
pub fn unpad(mut data: Vec<u8>) -> Result<Vec<u8>, String> {
    let pad = *data.last().ok_or("empty")? as usize;
    if pad == 0 || pad > 16 || pad > data.len() || !data[data.len() - pad..].iter().all(|&b| b as usize == pad) {
        return Err("bad padding (wrong key?)".into());
    }
    data.truncate(data.len() - pad);
    Ok(data)
}

/* A message as it goes over TCP: length prefix, body, CRC. */
pub fn encode(message: &Message, local_key: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(VERSION);
    body.extend_from_slice(&message.seq.to_be_bytes());
    body.extend_from_slice(&message.random.to_be_bytes());
    body.extend_from_slice(&message.timestamp.to_be_bytes());
    body.extend_from_slice(&message.protocol.to_be_bytes());
    if !message.payload.is_empty() {
        let encrypted = ecb_encrypt(&message_key(message.timestamp, local_key), &message.payload);
        body.extend_from_slice(&(encrypted.len() as u16).to_be_bytes());
        body.extend_from_slice(&encrypted);
    }
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend(body);
    frame
}

/* The same message without the length prefix: how it travels over
 * Roborock's cloud (MQTT already says where a message ends). */
pub fn encode_unprefixed(message: &Message, local_key: &str) -> Vec<u8> {
    encode(message, local_key)[4..].to_vec()
}

/* One frame's body (without the length prefix) -- also a whole message
 * from the cloud. */
pub fn decode_body(body: &[u8], local_key: &str) -> Result<Message, String> {
    const HEADER: usize = 3 + 4 + 4 + 4 + 2;
    if body.len() < HEADER || &body[..3] != VERSION {
        return Err(format!("not a \"1.0\" message ({} bytes)", body.len()));
    }
    let u32_at = |i: usize| u32::from_be_bytes(body[i..i + 4].try_into().unwrap());
    let mut message = Message {
        seq: u32_at(3),
        random: u32_at(7),
        timestamp: u32_at(11),
        protocol: u16::from_be_bytes([body[15], body[16]]),
        payload: Vec::new(),
    };
    /* Without a payload: the header, maybe a CRC. */
    if body.len() <= HEADER + 4 {
        return Ok(message);
    }
    let size = u16::from_be_bytes([body[HEADER], body[HEADER + 1]]) as usize;
    let end = HEADER + 2 + size;
    if body.len() < end + 4 {
        return Err(format!("message cut short ({} of {} bytes)", body.len(), end + 4));
    }
    let crc = u32_at(end);
    if crc != crc32(&body[..end]) {
        return Err("checksum mismatch".into());
    }
    if size > 0 {
        message.payload = ecb_decrypt(&message_key(message.timestamp, local_key), &body[HEADER + 2..end])?;
    }
    Ok(message)
}

/* Collects TCP bytes, hands out whole messages. */
#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /* The next whole message, if one has arrived. Err: it couldn't be read
     * (the stream is unusable then: reconnect). */
    pub fn next(&mut self, local_key: &str) -> Option<Result<Message, String>> {
        if self.buffer.len() < 4 {
            return None;
        }
        let length = u32::from_be_bytes(self.buffer[..4].try_into().unwrap()) as usize;
        if length > MAX_FRAME {
            self.buffer.clear();
            return Some(Err(format!("frame of {length} bytes: lost track of the stream")));
        }
        if self.buffer.len() < 4 + length {
            return None;
        }
        let body: Vec<u8> = self.buffer.drain(..4 + length).skip(4).collect();
        Some(decode_body(&body, local_key))
    }
}

/* A broadcast: the vacuum's device id ("duid") and address. */
pub fn decode_broadcast(packet: &[u8]) -> Option<(String, String)> {
    /* version 3, seq 4, protocol 2, size 2, payload, crc 4 */
    if packet.len() < 15 || &packet[..3] != VERSION {
        return None;
    }
    let size = u16::from_be_bytes([packet[9], packet[10]]) as usize;
    let end = 11 + size;
    if packet.len() < end + 4 || u32::from_be_bytes(packet[end..end + 4].try_into().ok()?) != crc32(&packet[..end]) {
        return None;
    }
    let json: Value = serde_json::from_slice(&ecb_decrypt(BROADCAST_KEY, &packet[11..end]).ok()?).ok()?;
    Some((json["duid"].as_str()?.to_string(), json["ip"].as_str()?.to_string()))
}

/* A command's payload (see the top of the file). */
pub fn request_payload(id: u32, method: &str, params: Value, timestamp: u32) -> Vec<u8> {
    let inner = json!({"id": id, "method": method, "params": params});
    json!({"dps": {"101": inner.to_string()}, "t": timestamp}).to_string().into_bytes()
}

/* What a payload says. */
#[derive(Debug, PartialEq)]
pub enum Payload {
    /* The answer to request `id`: its result, or the vacuum's error. */
    Reply { id: u32, result: Result<Value, String> },
    /* Pushed by the vacuum: state code and/or battery. */
    Push { state: Option<i64>, battery: Option<i64> },
    Other,
}

pub fn parse_payload(payload: &[u8]) -> Payload {
    let Ok(json) = serde_json::from_slice::<Value>(payload) else { return Payload::Other };
    let dps = &json["dps"];
    if let Some(reply) = dps["102"].as_str().and_then(|r| serde_json::from_str::<Value>(r).ok()) {
        let Some(id) = reply["id"].as_u64() else { return Payload::Other };
        let result = if let Some(error) = reply.get("error") {
            Err(error["message"].as_str().map_or_else(|| error.to_string(), str::to_string))
        } else {
            match &reply["result"] {
                Value::String(s) if s == "unknown_method" => Err("the vacuum doesn't know this command".into()),
                Value::Null => Err("empty answer".into()),
                other => Ok(other.clone()),
            }
        };
        return Payload::Reply { id: id as u32, result };
    }
    let number = |key: &str| dps[key].as_i64().or_else(|| dps[key].as_str().and_then(|s| s.parse().ok()));
    let (state, battery) = (number("121"), number("122"));
    if state.is_some() || battery.is_some() {
        return Payload::Push { state, battery };
    }
    Payload::Other
}

/* A Roborock state code -> the hub's state and the vendor's word. The
 * same codes in both protocols (miIO too). */
pub fn state(code: i64) -> (VacuumState, &'static str) {
    use VacuumState::*;
    match code {
        8 => (Docked, "charging"),
        100 => (Docked, "charged"),
        22 => (Docked, "emptying the bin"),
        23 | 25 => (Docked, "washing the mop"),
        1 => (Cleaning, "starting"),
        4 => (Cleaning, "remote control"),
        5 => (Cleaning, "cleaning"),
        7 => (Cleaning, "manual mode"),
        11 => (Cleaning, "spot cleaning"),
        16 => (Cleaning, "going to target"),
        17 => (Cleaning, "zone cleaning"),
        18 => (Cleaning, "room cleaning"),
        29 => (Cleaning, "mapping"),
        6 => (Returning, "returning to dock"),
        15 => (Returning, "docking"),
        26 => (Returning, "going to wash the mop"),
        10 => (Paused, "paused"),
        2 => (Idle, "off the charger"),
        3 => (Idle, "idle"),
        13 => (Idle, "shutting down"),
        14 => (Idle, "updating"),
        9 => (Error, "charging problem"),
        12 => (Error, "error"),
        _ => (Unknown, "unknown"),
    }
}

/* A Roborock error code -> what a person should do about it. */
pub fn error_text(code: i64) -> &'static str {
    match code {
        0 => "",
        1 => "Laser sensor blocked",
        2 => "Bumper stuck",
        3 => "Wheels off the floor",
        4 => "Cliff sensor dirty",
        5 => "Main brush jammed",
        6 => "Side brush jammed",
        7 => "Wheels jammed",
        8 => "Stuck: move it somewhere free",
        9 => "Dust bin missing",
        10 => "Filter wet or blocked",
        11 => "Strong magnetic field nearby",
        12 => "Battery low",
        13 => "Charging problem",
        14 => "Battery problem",
        15 => "Wall sensor dirty",
        16 => "Tilted: put it on a flat floor",
        17 => "Side brush problem",
        18 => "Fan problem",
        19 => "Dock has no power",
        22 => "Can't find the dock",
        23 => "Couldn't get back to the dock",
        24 => "In a no-go zone",
        26 => "Wall sensor problem",
        29 => "Filter blocked",
        32 => "Internal error: restart it",
        38 => "Check the clean water tank",
        39 => "Check the dirty water tank",
        41 => "Clean water tank empty",
        _ => "Needs attention (see the Roborock app)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* A real broadcast from the project's S7, captured on the LAN. */
    const S7_BROADCAST: &str = "312e3000000001000200405147d3125a037af1048f76b58ab3364283d85fd64fd2a0d9d2d4297809733c7c0eb58ead51ee622eb86fa1a65beaefde5ae33352541dbe1e876117e66295924cb510b3b6";

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn a_real_broadcast_is_read() {
        let (duid, ip) = decode_broadcast(&hex(S7_BROADCAST)).unwrap();
        assert_eq!(duid, "1tV5069KevcrlKa4Snsyyq");
        assert_eq!(ip, "192.168.1.134");
        /* One bit changed: the checksum catches it. */
        let mut broken = hex(S7_BROADCAST);
        broken[20] ^= 1;
        assert!(decode_broadcast(&broken).is_none());
    }

    #[test]
    fn timestamps_are_scrambled_like_python_roborock() {
        /* 0x12345678 -> hex "12345678", positions 5,6,3,7,1,2,0,4 */
        assert_eq!(encode_timestamp(0x1234_5678), b"67482315");
    }

    #[test]
    fn messages_survive_a_round_trip() {
        let key = "abcdefghijklmnop";
        let message = Message {
            seq: 7,
            random: 12345,
            timestamp: 1_700_000_000,
            protocol: GENERAL,
            payload: request_payload(4242, "get_status", json!([]), 1_700_000_000),
        };
        let frame = encode(&message, key);
        let mut decoder = Decoder::default();
        /* Arriving in two pieces, like TCP may deliver it. */
        decoder.push(&frame[..10]);
        assert!(decoder.next(key).is_none());
        decoder.push(&frame[10..]);
        assert_eq!(decoder.next(key).unwrap().unwrap(), message);
        /* The wrong key can't read it. */
        decoder.push(&frame);
        assert!(decoder.next("ponmlkjihgfedcba").unwrap().is_err());
        /* No payload (hello): header and CRC only. */
        let hello = Message { payload: Vec::new(), protocol: HELLO_REQUEST, ..message };
        decoder.push(&encode(&hello, key));
        assert_eq!(decoder.next(key).unwrap().unwrap(), hello);
    }

    #[test]
    fn replies_and_pushes_are_told_apart() {
        let reply = json!({"dps": {"102": json!({"id": 4242, "result": [{"state": 8, "battery": 100}]}).to_string()}});
        assert_eq!(
            parse_payload(reply.to_string().as_bytes()),
            Payload::Reply { id: 4242, result: Ok(json!([{"state": 8, "battery": 100}])) }
        );
        let error = json!({"dps": {"102": json!({"id": 9, "error": {"code": -10007, "message": "invalid status"}}).to_string()}});
        assert_eq!(
            parse_payload(error.to_string().as_bytes()),
            Payload::Reply { id: 9, result: Err("invalid status".into()) }
        );
        let push = json!({"dps": {"121": 5, "122": 87}, "t": 1});
        assert_eq!(parse_payload(push.to_string().as_bytes()), Payload::Push { state: Some(5), battery: Some(87) });
        assert_eq!(state(8), (VacuumState::Docked, "charging"));
    }
}
