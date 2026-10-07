/*
 * miio.rs -- Xiaomi's "miIO" LAN protocol (issue #74): Roborocks paired
 * with Xiaomi's Mi Home app (not the Roborock app), and other Xiaomi-made
 * devices. UDP port 54321. Followed from python-miio.
 *
 * Every packet has a 32-byte header, then (except for "hello") a payload:
 *
 *   magic     u16  0x2131
 *   length    u16  of the whole packet
 *   unknown   u32  0
 *   device id u32
 *   stamp     u32  the device's clock (seconds); a request must carry the
 *                  device's stamp + the time since the hello, or it's
 *                  ignored as a replay
 *   checksum  16 B MD5(header with this field = token, payload)
 *   payload        AES-128-CBC, key = MD5(token), iv = MD5(key + token),
 *                  PKCS#7 padding; JSON: {"id": 1, "method": "get_status",
 *                  "params": []} -> {"id": 1, "result": [...]}
 *
 * The TOKEN (32 hex digits = 16 bytes) is the device's secret. Mi Home
 * keeps it in Xiaomi's cloud; logging in there to fetch it is a follow-up
 * (Xiaomi's login often asks for a captcha) -- until then it's typed in,
 * e.g. from a token extractor tool.
 *
 * HELLO: a header of 0xFF bytes (length 32); the device answers with its
 * id and stamp. Done before every request burst: cheap, and it keeps the
 * stamp right.
 */
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

use super::roborock_proto::unpad;

pub const PORT: u16 = 54321;
const MAGIC: u16 = 0x2131;
/* A miIO device answers within milliseconds; UDP can lose a packet. */
const WAIT: Duration = Duration::from_secs(2);
const TRIES: u32 = 3;

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Md5::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/* "0123..." (32 hex digits) -> 16 bytes. */
pub fn parse_token(text: &str) -> Option<[u8; 16]> {
    let text = text.trim();
    if text.len() != 32 {
        return None;
    }
    let mut token = [0u8; 16];
    for (i, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(token)
}

fn key_iv(token: &[u8; 16]) -> ([u8; 16], [u8; 16]) {
    let key = md5(&[token]);
    let iv = md5(&[&key, token]);
    (key, iv)
}

pub fn encrypt(token: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let (key, iv) = key_iv(token);
    let cipher = Aes128::new(&key.into());
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(pad as u8, pad));
    let mut previous = iv;
    for block in data.chunks_mut(16) {
        for (b, p) in block.iter_mut().zip(previous) {
            *b ^= p;
        }
        cipher.encrypt_block(block.into());
        previous.copy_from_slice(block);
    }
    data
}

pub fn decrypt(token: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err("encrypted payload isn't whole blocks".into());
    }
    let (key, iv) = key_iv(token);
    let cipher = Aes128::new(&key.into());
    let mut out = data.to_vec();
    let mut previous = iv;
    for block in out.chunks_mut(16) {
        let this: [u8; 16] = block.try_into().unwrap();
        cipher.decrypt_block(block.into());
        for (b, p) in block.iter_mut().zip(previous) {
            *b ^= p;
        }
        previous = this;
    }
    unpad(out)
}

/* A packet: header + encrypted payload, checksum filled in. */
pub fn packet(token: &[u8; 16], device_id: u32, stamp: u32, payload: &[u8]) -> Vec<u8> {
    let encrypted = encrypt(token, payload);
    let mut out = Vec::with_capacity(32 + encrypted.len());
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(&((32 + encrypted.len()) as u16).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&device_id.to_be_bytes());
    out.extend_from_slice(&stamp.to_be_bytes());
    let checksum = md5(&[&out, token, &encrypted]);
    out.extend_from_slice(&checksum);
    out.extend_from_slice(&encrypted);
    out
}

/* A received packet: (device id, stamp, decrypted payload -- empty for a
 * hello answer). Err: not miIO, or the checksum says another token. */
pub fn parse(token: &[u8; 16], data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if data.len() < 32 || u16::from_be_bytes([data[0], data[1]]) != MAGIC {
        return Err("not a miIO packet".into());
    }
    let length = u16::from_be_bytes([data[2], data[3]]) as usize;
    if length != data.len() {
        return Err("miIO packet length mismatch".into());
    }
    let device_id = u32::from_be_bytes(data[8..12].try_into().unwrap());
    let stamp = u32::from_be_bytes(data[12..16].try_into().unwrap());
    if data.len() == 32 {
        return Ok((device_id, stamp, Vec::new()));
    }
    let checksum = md5(&[&data[..16], token, &data[32..]]);
    if checksum[..] != data[16..32] {
        return Err("the device answered, but not with this token".into());
    }
    Ok((device_id, stamp, decrypt(token, &data[32..])?))
}

fn hello() -> [u8; 32] {
    let mut packet = [0xFFu8; 32];
    packet[..4].copy_from_slice(&[0x21, 0x31, 0x00, 0x20]);
    packet
}

/* One request: hello, then the command; its "result". */
pub async fn call(host: &str, token: &[u8; 16], id: u32, method: &str, params: Value) -> Result<Value, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
    let target = if host.contains(':') { host.to_string() } else { format!("{host}:{PORT}") };
    socket.connect(&target).await.map_err(|e| format!("{host}: {e}"))?;
    let mut buf = vec![0u8; 8192];
    let mut receive = async |socket: &UdpSocket| -> Result<Vec<u8>, String> {
        match tokio::time::timeout(WAIT, socket.recv(&mut buf)).await {
            Ok(Ok(n)) => Ok(buf[..n].to_vec()),
            Ok(Err(e)) => Err(format!("{host}: {e}")),
            Err(_) => Err(format!("{host} isn't answering")),
        }
    };
    let mut last_error = String::new();
    for _ in 0..TRIES {
        if let Err(e) = socket.send(&hello()).await {
            last_error = e.to_string();
            continue;
        }
        let answer = match receive(&socket).await {
            Ok(answer) => answer,
            Err(e) => {
                last_error = e;
                continue;
            }
        };
        let (device_id, stamp, _) = parse(token, &answer)?;
        let sent_at = Instant::now();
        let payload = json!({"id": id, "method": method, "params": params}).to_string();
        let now = stamp.wrapping_add(sent_at.elapsed().as_secs() as u32 + 1);
        socket
            .send(&packet(token, device_id, now, payload.as_bytes()))
            .await
            .map_err(|e| e.to_string())?;
        /* Skip late answers to earlier tries (another id). */
        while let Ok(answer) = receive(&socket).await {
            let (_, _, reply) = parse(token, &answer)?;
            let reply: Value = serde_json::from_slice(&reply).map_err(|e| format!("not JSON: {e}"))?;
            if reply["id"].as_u64() != Some(id.into()) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(error["message"].as_str().map_or_else(|| error.to_string(), str::to_string));
            }
            return Ok(reply["result"].clone());
        }
        last_error = format!("{host} took the command but didn't answer (wrong token?)");
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /* A miIO device in a test: answers hellos and get_status. */
    pub async fn sim(token: [u8; 16]) -> String {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                let Ok((n, from)) = socket.recv_from(&mut buf).await else { return };
                let data = &buf[..n];
                if data == hello() {
                    let mut answer = [0u8; 32];
                    answer[..4].copy_from_slice(&[0x21, 0x31, 0x00, 0x20]);
                    answer[8..12].copy_from_slice(&0x0A0B_0C0Du32.to_be_bytes());
                    answer[12..16].copy_from_slice(&1000u32.to_be_bytes());
                    answer[16..].copy_from_slice(&[0xFF; 16]);
                    let _ = socket.send_to(&answer, from).await;
                    continue;
                }
                /* A wrong token: ignored, like a real device does. */
                let Ok((_, _, request)) = parse(&token, data) else { continue };
                let request: Value = serde_json::from_slice(&request).unwrap();
                let result = match request["method"].as_str() {
                    Some("get_status") => json!([{"state": 8, "battery": 100, "error_code": 0}]),
                    _ => json!(["ok"]),
                };
                let reply = json!({"id": request["id"], "result": result}).to_string();
                let _ = socket.send_to(&packet(&token, 0x0A0B_0C0D, 1001, reply.as_bytes()), from).await;
            }
        });
        address
    }

    /* Cross-checked with OpenSSL: aes-128-cbc, key MD5(token), iv
     * MD5(key + token). */
    #[test]
    fn encryption_matches_openssl() {
        let token = parse_token("00112233445566778899aabbccddeeff").unwrap();
        let encrypted = encrypt(&token, br#"{"id":1,"method":"get_status","params":[]}"#);
        let hex: String = encrypted.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "a5516ec6151955dc2bb2d43e7c84c183bbabebda6a820d0a55fcfe476c51a42cdb4e99776d93c2afe2d14dcacc7585b5"
        );
    }

    #[test]
    fn payloads_survive_a_round_trip() {
        let token = parse_token("00112233445566778899aabbccddeeff").unwrap();
        let data = packet(&token, 7, 99, b"{\"id\":1}");
        assert_eq!(parse(&token, &data).unwrap(), (7, 99, b"{\"id\":1}".to_vec()));
        let other = parse_token("ffeeddccbbaa99887766554433221100").unwrap();
        assert!(parse(&other, &data).is_err());
        assert!(parse_token("1234").is_none());
    }

    #[tokio::test]
    async fn a_simulated_device_answers() {
        let token = parse_token("00112233445566778899aabbccddeeff").unwrap();
        let address = sim(token).await;
        let status = call(&address, &token, 5, "get_status", json!([])).await.unwrap();
        assert_eq!(status[0]["state"], 8);
        /* Another token: the device stays silent; we say so. */
        let wrong = parse_token("ffeeddccbbaa99887766554433221100").unwrap();
        let error = call(&address, &wrong, 6, "get_status", json!([])).await.unwrap_err();
        assert!(error.contains("wrong token"), "{error}");
    }
}
