/*
 * roborock_map.rs -- a Roborock's map (issue #74): fetched through
 * Roborock's cloud, parsed, and packed small for the screens.
 *
 * WHY THE CLOUD. Everything else about the vacuum comes over the LAN
 * (roborock.rs). The map doesn't: the vacuum uploads it to Roborock, and
 * even the Roborock app downloads it from there. So the map is OPT-IN (a
 * toggle at setup): the hub then keeps a Roborock session (keys, never the
 * password) as the vacuum's secret "cloud_session", and the map works only
 * while the hub has internet. The rest keeps working without.
 *
 * FETCHING (python-roborock's way, v1_channel.py / map_content.py):
 *   - Roborock's MQTT broker (rriot.r.m), user and password derived from
 *     the session's keys: MD5(u:k)[2..10], MD5(s:k)[16..];
 *   - the hub publishes "get_map_v1" to rr/m/i/<u>/<user>/<duid> as an
 *     ordinary "1.0" message (protocol 101, roborock_proto.rs) with a
 *     "security" part: an endpoint (who asks: from the session's k) and a
 *     random 16-byte NONCE made up for this request;
 *   - the vacuum answers on rr/m/o/<u>/<user>/<duid> with protocol 301:
 *     24 bytes of header (endpoint, request id), then the map, encrypted
 *     with AES-128-CBC under that nonce (so only who asked can read it),
 *     then gzip.
 *
 * THE MAP FORMAT ("RRMap", vacuum-map-parser-roborock, Apache-2.0): a
 * header, then blocks [type u16, header length u16, data length u32, ...],
 * little-endian. The ones drawn: 1 the dock, 2 the image, 3 the path the
 * vacuum drove, 8 where it is now, 9 no-go areas, 10 virtual walls. The
 * image: one byte per 5 cm square -- 0 outside, 1 wall, 0xFF floor, 7
 * scanned, and for a room (low 3 bits 7) its number in the upper 5 bits.
 * Positions are in mm-ish map units: /50 = image pixels; y grows up.
 *
 * FOR THE SCREENS (Map::to_json): the image cropped to what's there,
 * each pixel a class -- 0 outside, 1 wall, 2 floor, 10+n room n --
 * run-length packed and base64 (a whole flat: a few KB), positions in
 * that image's pixels, y down like a screen.
 */
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use base64::Engine;
use md5::{Digest, Md5};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

use super::roborock_cloud::Session;
use super::roborock_proto::{self as proto, Message};

/* Connecting, asking, and the vacuum uploading: a few seconds. */
const FETCH_LIMIT: Duration = Duration::from_secs(20);
/* A map is ~10-100 KB; a whole house with years of data well under 1 MB. */
const MAX_MAP: usize = 4 * 1024 * 1024;
const RPC_REQUEST: u16 = 101;
const MAP_RESPONSE: u16 = 301;

fn md5hex(text: &str) -> String {
    Md5::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/* "Who asks", as the vacuum sees it: base64(MD5(k)[8..14]). */
fn endpoint(session: &Session) -> String {
    base64::engine::general_purpose::STANDARD.encode(&Md5::digest(session.k.as_bytes())[8..14])
}

/* AES-128-CBC with a zero IV, PKCS#7 (the map's inner encryption). */
fn decrypt_cbc(key: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err("map payload isn't whole blocks".into());
    }
    let cipher = Aes128::new(key.into());
    let mut out = data.to_vec();
    let mut previous = [0u8; 16];
    for block in out.chunks_mut(16) {
        let this: [u8; 16] = block.try_into().unwrap();
        cipher.decrypt_block(block.into());
        for (b, p) in block.iter_mut().zip(previous) {
            *b ^= p;
        }
        previous = this;
    }
    proto::unpad(out)
}

/* A map answer's payload -> the raw map (see the top of the file). None if
 * it's someone else's (another app asked: another endpoint) or another
 * request's. */
fn unpack(payload: &[u8], my_endpoint: &str, request_id: u16, nonce: &[u8; 16]) -> Option<Result<Vec<u8>, String>> {
    if payload.len() < 24 || !payload[..8].starts_with(my_endpoint.as_bytes()) {
        return None;
    }
    if u16::from_le_bytes([payload[16], payload[17]]) != request_id {
        return None;
    }
    Some(decrypt_cbc(nonce, &payload[24..]).and_then(|gz| {
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .take(MAX_MAP as u64)
            .read_to_end(&mut raw)
            .map_err(|e| format!("map not gzip: {e}"))?;
        Ok(raw)
    }))
}

/* Why a fetch failed. `refused`: Roborock's broker said "not authorized"
 * -- which it also says when it RATE-LIMITS an account that connects too
 * often (python-roborock, mqtt/session.py), so the caller backs off. */
#[derive(Debug)]
pub struct FetchError {
    pub refused: bool,
    pub text: String,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

fn other(text: impl Into<String>) -> FetchError {
    FetchError { refused: false, text: text.into() }
}

/* An idle connection is dropped after this: nothing polls it between
 * maps, so its keep-alive pings stop, and the broker would drop it at
 * 1.5 x KEEP_ALIVE anyway. */
const IDLE: Duration = Duration::from_secs(60);
const KEEP_ALIVE: Duration = Duration::from_secs(90);

/* ONE CONNECTION, KEPT (issue #74 bug: "connection refused"). Roborock's
 * broker limits how often an account connects; a new connection per map
 * (every 10 s while the vacuum cleans) got the hub refused. So a vacuum
 * keeps one connection, asks for every map over it, and drops it after
 * IDLE without maps -- like python-roborock's long-lived MQTT session. */
pub struct Connection {
    client: AsyncClient,
    events: rumqttc::EventLoop,
    /* Subscribed to the vacuum's answers: maps can be asked for. */
    ready: bool,
    /* Which session it logged in with (Session::fingerprint). */
    session: String,
    used: std::time::Instant,
}

impl Connection {
    /* Sets it up; it connects on the first ask. */
    pub fn new(session: &Session) -> Result<Connection, FetchError> {
        let url = session.mqtt.strip_prefix("ssl://").ok_or_else(|| other("no Roborock MQTT address in the session: log in again"))?;
        let (host, port) = url.rsplit_once(':').ok_or_else(|| other("bad MQTT address"))?;
        let port: u16 = port.parse().map_err(|_| other("bad MQTT port"))?;
        let (user, password) = credentials(session);
        let mut random = [0u8; 6];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random).map_err(|_| other("no randomness"))?;
        let client_id = format!("hub-{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>());

        let mut options = MqttOptions::new(client_id, host, port);
        options.set_credentials(user, password);
        options.set_keep_alive(KEEP_ALIVE);
        options.set_max_packet_size(MAX_MAP, 64 * 1024);
        options.set_transport(Transport::Tls(TlsConfiguration::Rustls(super::cloud::tls_config().map_err(other)?)));
        let (client, events) = AsyncClient::new(options, 8);
        Ok(Connection { client, events, ready: false, session: session.fingerprint(), used: std::time::Instant::now() })
    }

    /* Still worth asking over: same session, not idle too long. */
    pub fn usable(&self, session: &Session) -> bool {
        self.session == session.fingerprint() && self.used.elapsed() < IDLE
    }

    /* Asks the vacuum for its map; the raw map. After an Err the
     * connection is spent: the caller drops it. */
    pub async fn ask(&mut self, session: &Session, duid: &str, local_key: &str) -> Result<Vec<u8>, FetchError> {
        self.used = std::time::Instant::now();
        match tokio::time::timeout(FETCH_LIMIT, self.ask_inner(session, duid, local_key)).await {
            Ok(result) => result,
            Err(_) => Err(other(format!("no map from Roborock's cloud within {} s", FETCH_LIMIT.as_secs()))),
        }
    }

    async fn ask_inner(&mut self, session: &Session, duid: &str, local_key: &str) -> Result<Vec<u8>, FetchError> {
        let (user, _) = credentials(session);
        let incoming = format!("rr/m/o/{}/{user}/{duid}", session.u);
        let outgoing = format!("rr/m/i/{}/{user}/{duid}", session.u);

        let mut random = [0u8; 18];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random).map_err(|_| other("no randomness"))?;
        let nonce: [u8; 16] = random[..16].try_into().unwrap();
        let request_id = 10_000 + u16::from_be_bytes([random[16], random[17]]) % 20_000;
        let my_endpoint = endpoint(session);
        let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
        let inner = json!({
            "id": request_id,
            "method": "get_map_v1",
            "params": [],
            "security": {"endpoint": my_endpoint, "nonce": nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()},
        });
        let request = Message {
            seq: 100_000 + u32::from(request_id),
            random: 10_000 + u32::from(request_id),
            timestamp,
            protocol: RPC_REQUEST,
            payload: json!({"dps": {"101": inner.to_string()}, "t": timestamp}).to_string().into_bytes(),
        };
        let publish = |client: AsyncClient| {
            let (outgoing, body) = (outgoing.clone(), proto::encode_unprefixed(&request, local_key));
            async move { client.publish(&outgoing, QoS::AtMostOnce, false, body).await.map_err(|e| other(e.to_string())) }
        };

        /* Already subscribed: ask right away. Else after the SubAck. */
        let mut asked = false;
        if self.ready {
            publish(self.client.clone()).await?;
            asked = true;
        }
        loop {
            let event = self.events.poll().await.map_err(|e| match e {
                /* The broker said no: rate limit or a dead session. */
                rumqttc::ConnectionError::ConnectionRefused(code) => FetchError {
                    refused: true,
                    text: format!("Roborock's cloud refused the hub ({code:?})"),
                },
                e => other(format!("Roborock's cloud: {e}")),
            })?;
            match event {
                Event::Incoming(Packet::ConnAck(_)) => {
                    self.ready = false;
                    self.client.subscribe(&incoming, QoS::AtMostOnce).await.map_err(|e| other(e.to_string()))?;
                }
                Event::Incoming(Packet::SubAck(_)) => {
                    self.ready = true;
                    if !asked {
                        publish(self.client.clone()).await?;
                        asked = true;
                    }
                }
                /* Answers to earlier asks (one that timed out) have
                 * another request id: unpack skips them. */
                Event::Incoming(Packet::Publish(p)) if p.topic == incoming => {
                    let Ok(message) = proto::decode_body(&p.payload, local_key) else { continue };
                    if message.protocol != MAP_RESPONSE {
                        continue;
                    }
                    if let Some(result) = unpack(&message.payload, &my_endpoint, request_id, &nonce) {
                        self.used = std::time::Instant::now();
                        return result.map_err(other);
                    }
                }
                _ => {}
            }
        }
    }
}

/* The broker's user and password, from the session's keys. */
fn credentials(session: &Session) -> (String, String) {
    (
        md5hex(&format!("{}:{}", session.u, session.k))[2..10].to_string(),
        md5hex(&format!("{}:{}", session.s, session.k))[16..].to_string(),
    )
}

/* One map, on a connection of its own (the ROBOROCK_MAP test tool). */
#[cfg(test)]
pub async fn fetch(session: &Session, duid: &str, local_key: &str) -> Result<Vec<u8>, String> {
    let mut connection = Connection::new(session).map_err(|e| e.text)?;
    let result = connection.ask(session, duid, local_key).await.map_err(|e| e.text);
    let _ = connection.client.disconnect().await;
    result
}

/* ------------------------------------------------------------------ */
/* Parsing                                                             */
/* ------------------------------------------------------------------ */

/* A map, as the screens get it (see the top of the file). */
#[derive(Debug, Default, PartialEq)]
pub struct Map {
    pub width: usize,
    pub height: usize,
    /* Row by row from the top: 0 outside, 1 wall, 2 floor, 10+n room n. */
    pub pixels: Vec<u8>,
    pub dock: Option<(f32, f32)>,
    /* x, y, heading in degrees (null if not known). */
    pub robot: Option<(f32, f32, Option<i32>)>,
    pub path: Vec<(f32, f32)>,
    pub no_go: Vec<[(f32, f32); 4]>,
    pub walls: Vec<[(f32, f32); 2]>,
    /* The room numbers seen in the image. */
    pub rooms: Vec<u8>,
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/* The longest path kept: a long run has tens of thousands of points; a
 * screen draws a few thousand just as well. */
const MAX_PATH: usize = 3000;

pub fn parse(raw: &[u8]) -> Result<Map, String> {
    let bad = || "not a Roborock map (truncated block)".to_string();
    if raw.len() < 0x14 || &raw[..2] != b"rr" {
        return Err("not a Roborock map".into());
    }
    let mut pos = u16_at(raw, 2).ok_or_else(bad)? as usize;
    let mut image: Option<(usize, usize, i64, i64, &[u8])> = None;
    let mut dock = None;
    let mut robot = None;
    let mut path: Vec<(f32, f32)> = Vec::new();
    let mut no_go = Vec::new();
    let mut walls = Vec::new();
    while pos + 8 <= raw.len() {
        let kind = u16_at(raw, pos).ok_or_else(bad)?;
        let header_len = u16_at(raw, pos + 2).ok_or_else(bad)? as usize;
        let data_len = u32_at(raw, pos + 4).ok_or_else(bad)? as usize;
        let header = raw.get(pos..pos + header_len).ok_or_else(bad)?;
        let data = raw.get(pos + header_len..pos + header_len + data_len).ok_or_else(bad)?;
        let i16_at = |d: &[u8], at: usize| u16_at(d, at).map(|v| v as i16 as f32);
        match kind {
            1 => dock = Some((u32_at(data, 0).ok_or_else(bad)? as f32, u32_at(data, 4).ok_or_else(bad)? as f32)),
            2 => {
                let top = u32_at(header, header_len - 16).ok_or_else(bad)? as i64;
                let left = u32_at(header, header_len - 12).ok_or_else(bad)? as i64;
                let height = u32_at(header, header_len - 8).ok_or_else(bad)? as usize;
                let width = u32_at(header, header_len - 4).ok_or_else(bad)? as usize;
                if width * height > data.len() || width > 4096 || height > 4096 {
                    return Err("map image size doesn't fit its data".into());
                }
                image = Some((width, height, top, left, data));
            }
            3 => {
                let points: Vec<(f32, f32)> = data
                    .chunks_exact(4)
                    .filter_map(|c| Some((i16_at(c, 0)?, i16_at(c, 2)?)))
                    .collect();
                let step = points.len().div_ceil(MAX_PATH).max(1);
                path = points.into_iter().step_by(step).collect();
            }
            8 => {
                let heading = if data_len > 8 {
                    u32_at(data, 8).map(|a| if a > 0xFF { (a & 0xFF) as i32 - 256 } else { a as i32 })
                } else {
                    None
                };
                robot = Some((u32_at(data, 0).ok_or_else(bad)? as f32, u32_at(data, 4).ok_or_else(bad)? as f32, heading));
            }
            9 => {
                let count = u16_at(header, 8).unwrap_or(0) as usize;
                for area in data.chunks_exact(16).take(count) {
                    let p = |i: usize| (i16_at(area, i).unwrap_or(0.0), i16_at(area, i + 2).unwrap_or(0.0));
                    no_go.push([p(0), p(4), p(8), p(12)]);
                }
            }
            10 => {
                let count = u16_at(header, 8).unwrap_or(0) as usize;
                for wall in data.chunks_exact(8).take(count) {
                    let p = |i: usize| (i16_at(wall, i).unwrap_or(0.0), i16_at(wall, i + 2).unwrap_or(0.0));
                    walls.push([p(0), p(4)]);
                }
            }
            _ => {}
        }
        /* The next block: after this one's data (header length as one
         * byte at +2, like the original parser). */
        let next = pos + data_len + raw[pos + 2] as usize;
        if next <= pos {
            break;
        }
        pos = next;
    }
    let (width, height, top, left, data) = image.ok_or("the map has no image yet (still mapping?)")?;

    /* Classes, flipped so row 0 is the top (the vacuum's y grows up). */
    let mut classes = vec![0u8; width * height];
    let mut rooms = std::collections::BTreeSet::new();
    for y in 0..height {
        for x in 0..width {
            let pixel = data[x + width * y];
            let class = match pixel {
                0x00 => 0,
                0x01 => 1,
                0xFF | 0x07 => 2,
                p if p & 0x07 == 7 => {
                    rooms.insert(p >> 3);
                    10 + (p >> 3)
                }
                p if p & 0x07 <= 1 => 1,
                _ => 2,
            };
            classes[x + width * (height - 1 - y)] = class;
        }
    }
    /* Cropped to what's there. */
    let (mut x0, mut y0, mut x1, mut y1) = (width, height, 0, 0);
    for y in 0..height {
        for x in 0..width {
            if classes[x + width * y] != 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 > x1 {
        return Err("the map is empty".into());
    }
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut pixels = Vec::with_capacity(w * h);
    for y in y0..=y1 {
        pixels.extend_from_slice(&classes[x0 + width * y..=x1 + width * y]);
    }
    /* Map units -> cropped image pixels, y down. */
    let to_px = |(mx, my): (f32, f32)| -> (f32, f32) {
        let ix = mx / 50.0 - left as f32;
        let iy = my / 50.0 - top as f32;
        (ix - x0 as f32, (height as f32 - 1.0 - iy) - y0 as f32)
    };
    Ok(Map {
        width: w,
        height: h,
        pixels,
        dock: dock.map(to_px),
        robot: robot.map(|(x, y, a)| {
            let (px, py) = to_px((x, y));
            (px, py, a)
        }),
        path: path.into_iter().map(to_px).collect(),
        no_go: no_go.into_iter().map(|a| a.map(to_px)).collect(),
        walls: walls.into_iter().map(|w| w.map(to_px)).collect(),
        rooms: rooms.into_iter().collect(),
    })
}

/* Runs of the same byte: [value, count (1-255)]... */
pub fn rle(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < pixels.len() {
        let value = pixels[i];
        let mut run = 1;
        while run < 255 && i + run < pixels.len() && pixels[i + run] == value {
            run += 1;
        }
        out.extend_from_slice(&[value, run as u8]);
        i += run;
    }
    out
}

impl Map {
    /* What a client gets (the "map" action's answer). */
    pub fn to_json(&self) -> Value {
        let point = |(x, y): (f32, f32)| json!([(x * 10.0).round() / 10.0, (y * 10.0).round() / 10.0]);
        json!({
            "width": self.width,
            "height": self.height,
            "pixels": base64::engine::general_purpose::STANDARD.encode(rle(&self.pixels)),
            "dock": self.dock.map(point),
            "robot": self.robot.map(|(x, y, a)| json!([(x * 10.0).round() / 10.0, (y * 10.0).round() / 10.0, a])),
            "path": self.path.iter().map(|p| point(*p)).collect::<Vec<_>>(),
            "no_go": self.no_go.iter().map(|a| a.iter().map(|p| point(*p)).collect::<Vec<_>>()).collect::<Vec<_>>(),
            "walls": self.walls.iter().map(|w| w.iter().map(|p| point(*p)).collect::<Vec<_>>()).collect::<Vec<_>>(),
            "rooms": self.rooms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* A tiny map built by hand in the RRMap layout: a 4x3 image with a
     * wall row, floor and a room-16 pixel; dock and robot. */
    pub fn tiny_map() -> Vec<u8> {
        let mut raw = Vec::new();
        /* header: "rr", header length 0x14, ... */
        raw.extend_from_slice(b"rr");
        raw.extend_from_slice(&0x14u16.to_le_bytes());
        raw.resize(0x14, 0);
        /* block 1, the dock at (100*50+25, 101*50+25) map units */
        let block = |kind: u16, header_extra: &[u8], data: &[u8]| {
            let mut b = Vec::new();
            b.extend_from_slice(&kind.to_le_bytes());
            b.extend_from_slice(&((8 + header_extra.len()) as u16).to_le_bytes());
            b.extend_from_slice(&(data.len() as u32).to_le_bytes());
            b.extend_from_slice(header_extra);
            b.extend_from_slice(data);
            b
        };
        let mut dock = Vec::new();
        dock.extend_from_slice(&(101u32 * 50).to_le_bytes());
        dock.extend_from_slice(&(201u32 * 50).to_le_bytes());
        raw.extend(block(1, &[], &dock));
        /* the image: top 200, left 100, height 3, width 4; rows from the
         * BOTTOM: [wall x4], [floor, room16, floor, outside], [outside x4] */
        let mut header = Vec::new();
        for v in [200u32, 100, 3, 4] {
            header.extend_from_slice(&v.to_le_bytes());
        }
        let room16 = (16u8 << 3) | 7;
        let pixels = [1, 1, 1, 1, 0xFF, room16, 0xFF, 0, 0, 0, 0, 0];
        raw.extend(block(2, &header, &pixels));
        let mut robot = Vec::new();
        robot.extend_from_slice(&(102u32 * 50).to_le_bytes());
        robot.extend_from_slice(&(201u32 * 50).to_le_bytes());
        robot.extend_from_slice(&90u32.to_le_bytes());
        raw.extend(block(8, &[], &robot));
        raw
    }

    #[test]
    fn a_map_is_parsed_flipped_and_cropped() {
        let map = parse(&tiny_map()).unwrap();
        /* The empty top row (outside) is cropped: 4x2, y down. */
        assert_eq!((map.width, map.height), (4, 2));
        assert_eq!(map.pixels, vec![2, 26, 2, 0, 1, 1, 1, 1]);
        assert_eq!(map.rooms, vec![16]);
        /* The dock at image (1, 1) from the bottom-left -> cropped (1, 0). */
        assert_eq!(map.dock, Some((1.0, 0.0)));
        assert_eq!(map.robot, Some((2.0, 0.0, Some(90))));
        assert!(parse(b"not a map").is_err());
    }

    /* A real map file -> what a client gets (`ROBOROCK_MAP_FILE=raw
     * ROBOROCK_MAP_JSON=out cargo test map_file_to_json -- --ignored`):
     * for previews, without committing anyone's floor plan. */
    #[test]
    #[ignore]
    fn map_file_to_json() {
        let raw = std::fs::read(std::env::var("ROBOROCK_MAP_FILE").unwrap()).unwrap();
        let json = parse(&raw).unwrap().to_json();
        std::fs::write(std::env::var("ROBOROCK_MAP_JSON").unwrap(), json.to_string()).unwrap();
    }

    #[test]
    fn runs_are_packed() {
        assert_eq!(rle(&[0, 0, 0, 2, 1, 1]), vec![0, 3, 2, 1, 1, 2]);
        assert_eq!(rle(&vec![5u8; 300]), vec![5, 255, 5, 45]);
    }

    #[test]
    fn map_answers_are_unpacked() {
        /* Build an answer as the vacuum would: header, CBC(nonce, gzip). */
        use aes::cipher::BlockEncrypt;
        let nonce = [7u8; 16];
        let raw = tiny_map();
        let mut gz = Vec::new();
        {
            use std::io::Write;
            let mut encoder = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            encoder.write_all(&raw).unwrap();
        }
        let pad = 16 - gz.len() % 16;
        gz.extend(std::iter::repeat_n(pad as u8, pad));
        let cipher = Aes128::new(&nonce.into());
        let mut previous = [0u8; 16];
        for block in gz.chunks_mut(16) {
            for (b, p) in block.iter_mut().zip(previous) {
                *b ^= p;
            }
            cipher.encrypt_block(block.into());
            previous.copy_from_slice(block);
        }
        let mut payload = b"ABCDEFGH".to_vec();
        payload.extend_from_slice(&[0; 8]);
        payload.extend_from_slice(&12345u16.to_le_bytes());
        payload.extend_from_slice(&[0; 6]);
        payload.extend(gz);
        assert_eq!(unpack(&payload, "ABCDEFGH", 12345, &nonce).unwrap().unwrap(), raw);
        /* Another app's or another request's answer: not ours. */
        assert!(unpack(&payload, "XXXXXXXX", 12345, &nonce).is_none());
        assert!(unpack(&payload, "ABCDEFGH", 1, &nonce).is_none());
    }
}
