/*
 * tapo.rs -- TP-Link Tapo cameras: their settings (issue #74, pattern P4:
 * the Tapo ACCOUNT password, used locally -- no cloud at all).
 *
 * A Tapo camera's own HTTPS API (port 443, the one the Tapo app uses on
 * the LAN) is logged in to as user "admin" with the TAPO ACCOUNT password
 * -- not the "Camera Account" set in the app for RTSP video, which is a
 * different one (and gets "Invalid authentication data" here). Followed
 * from pytapo (MIT), its classic "secure connection" (encrypt_type 3):
 *
 *   1. login {cnonce, encrypt_type 3} -> the camera's nonce and a
 *      device_confirm = SHA256(cnonce + H(password) + nonce) + nonce +
 *      cnonce, H being MD5 or SHA256 (uppercase hex) depending on the
 *      firmware. The hub checks it HERE: a wrong password is caught
 *      before any real login attempt, so it never counts toward the
 *      camera's lockout (10 failed logins = 30 minutes locked out).
 *   2. login {digest_passwd: SHA256(H + cnonce + nonce) + cnonce + nonce}
 *      -> "stok" (the session, in the URL) and "start_seq".
 *   3. every request: POST /stok=<stok>/ds {"method": "securePassthrough",
 *      "params": {"request": base64(AES-128-CBC(lsk, ivb, json))}}, with
 *      headers Seq (start_seq, +1 each request) and Tapo_tag =
 *      SHA256(SHA256(H + cnonce) + body + seq); the answer is encrypted
 *      the same way. lsk/ivb = SHA256("lsk"/"ivb" + cnonce + nonce +
 *      SHA256(cnonce + H + nonce))[..16].
 * The camera's certificate is self-signed: pinned at setup ("cert_sha256",
 * trust on first use, net::https_pinned) -- and re-pinned when it changes
 * (a firmware update makes a new one) if the camera passes the password
 * proof (Task::conn).
 *
 * WHAT THE HUB SHOWS:
 *   switch   on = the camera watches; off = PRIVACY MODE (lens covered:
 *            no video, no recording) -- the tile's power button
 *   options  motion detection, alarm on motion (siren + light), the status
 *            LED, night vision (auto / on / off)
 *   camera   with the app's "Camera Account" (stream_user/stream_password,
 *            optional): snapshots from the low-resolution RTSP stream
 *            (rtsp://.../stream2, camera.rs: key frames only), and the
 *            stream URLs (stream1 HD, stream2 SD) for apps to play
 */
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use base64::Engine;
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

use super::net::https_pinned;
use super::roborock_proto::unpad;
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Device, Health};
use crate::templates::ErrorKind;

const PORT: u16 = 443;
const USER: &str = "admin";
/* A camera's settings change only from the Tapo app (or here): read this
 * often. */
const POLL: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_secs(600);

/* Why a login or request failed. */
#[derive(Debug, PartialEq)]
pub enum Failure {
    Unreachable(String),
    /* Not the Tapo account password (caught before a real attempt). */
    WrongPassword,
    /* Too many failed logins: locked for this many seconds. */
    Suspended(u64),
    /* The certificate changed since setup: not the same camera. */
    OtherCamera,
    Refused(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Unreachable(e) | Failure::Refused(e) => f.write_str(e),
            Failure::WrongPassword => f.write_str("That isn't the Tapo account password"),
            Failure::Suspended(s) => write!(f, "the camera locked logins for {s} s (too many wrong passwords)"),
            Failure::OtherCamera => f.write_str("a different certificate than at setup: another camera at this address?"),
        }
    }
}

fn upper_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn sha256_upper(parts: &[&[u8]]) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for p in parts {
        ctx.update(p);
    }
    upper_hex(ctx.finish().as_ref())
}

fn sha256_raw(parts: &[&[u8]]) -> [u8; 32] {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for p in parts {
        ctx.update(p);
    }
    ctx.finish().as_ref().try_into().unwrap()
}

/* The two ways firmwares hash the password (uppercase hex). */
fn password_hashes(password: &str) -> [String; 2] {
    [sha256_upper(&[password.as_bytes()]), upper_hex(&Md5::digest(password.as_bytes()))]
}

/* Which hash this camera uses: the one its device_confirm was made with.
 * None: the password is wrong. */
pub fn confirm(password: &str, cnonce: &str, nonce: &str, device_confirm: &str) -> Option<String> {
    password_hashes(password).into_iter().find(|hashed| {
        let expected = format!("{}{nonce}{cnonce}", sha256_upper(&[cnonce.as_bytes(), hashed.as_bytes(), nonce.as_bytes()]));
        expected == device_confirm
    })
}

fn token(kind: &str, cnonce: &str, nonce: &str, hashed: &str) -> [u8; 16] {
    let hashed_key = sha256_upper(&[cnonce.as_bytes(), hashed.as_bytes(), nonce.as_bytes()]);
    sha256_raw(&[kind.as_bytes(), cnonce.as_bytes(), nonce.as_bytes(), hashed_key.as_bytes()])[..16]
        .try_into()
        .unwrap()
}

fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(pad as u8, pad));
    let mut previous = *iv;
    for block in data.chunks_mut(16) {
        for (b, p) in block.iter_mut().zip(previous) {
            *b ^= p;
        }
        cipher.encrypt_block(block.into());
        previous.copy_from_slice(block);
    }
    data
}

fn cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err("encrypted answer isn't whole blocks".into());
    }
    let cipher = Aes128::new(key.into());
    let mut out = data.to_vec();
    let mut previous = *iv;
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

/* A logged-in connection to one camera. */
pub struct Conn {
    host: String,
    fingerprint: Option<String>,
    cnonce: String,
    hashed: String,
    lsk: [u8; 16],
    ivb: [u8; 16],
    seq: i64,
    stok: String,
}

fn random_cnonce() -> String {
    let mut bytes = [0u8; 8];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes);
    upper_hex(&bytes)
}

async fn post(host: &str, path: &str, headers: &[(&str, String)], body: &Value, fingerprint: Option<&str>) -> Result<(Value, Option<String>), Failure> {
    let (bytes, _, seen) = https_pinned(host, PORT, path, headers, body.to_string().into_bytes(), fingerprint)
        .await
        .map_err(|e| if e.kind == ErrorKind::Refused { Failure::OtherCamera } else { Failure::Unreachable(e.to_string()) })?;
    let json = serde_json::from_slice(&bytes).map_err(|_| Failure::Refused(format!("{host} didn't answer like a Tapo camera")))?;
    Ok((json, seen))
}

/* The lockout, as both login answers can say it. */
fn suspended(reply: &Value) -> Option<u64> {
    [&reply["result"]["data"], &reply["data"]]
        .into_iter()
        .find_map(|d| d["sec_left"].as_u64().filter(|s| *s > 0))
}

impl Conn {
    /* Logs in (see the top of the file). `fingerprint`: the pinned
     * certificate; None at setup. Returns the connection and the
     * certificate seen. */
    pub async fn login(host: &str, password: &str, fingerprint: Option<&str>) -> Result<(Conn, Option<String>), Failure> {
        let headers = [("Referer", format!("https://{host}:{PORT}")), ("requestByApp", "true".to_string())];
        let cnonce = random_cnonce();
        let (reply, seen) = post(
            host,
            "/",
            &headers,
            &json!({"method": "login", "params": {"cnonce": cnonce, "encrypt_type": "3", "username": USER}}),
            fingerprint,
        )
        .await?;
        if let Some(seconds) = suspended(&reply) {
            return Err(Failure::Suspended(seconds));
        }
        let data = &reply["result"]["data"];
        let (Some(nonce), Some(device_confirm)) = (data["nonce"].as_str(), data["device_confirm"].as_str()) else {
            return Err(Failure::Refused(format!("{host} doesn't offer the Tapo secure login (another protocol or model)")));
        };
        let hashed = confirm(password, &cnonce, nonce, device_confirm).ok_or(Failure::WrongPassword)?;
        let digest = sha256_upper(&[hashed.as_bytes(), cnonce.as_bytes(), nonce.as_bytes()]);
        let (reply, _) = post(
            host,
            "/",
            &headers,
            &json!({"method": "login", "params": {
                "cnonce": cnonce, "encrypt_type": "3", "username": USER,
                "digest_passwd": format!("{digest}{cnonce}{nonce}"),
            }}),
            seen.as_deref(),
        )
        .await?;
        if let Some(seconds) = suspended(&reply) {
            return Err(Failure::Suspended(seconds));
        }
        let result = &reply["result"];
        let (Some(stok), Some(seq)) = (result["stok"].as_str(), result["start_seq"].as_i64()) else {
            return Err(Failure::Refused(format!("{host} refused the login (code {})", reply["error_code"])));
        };
        /* Logged in as a shared ("third party") user, not the owner: its
         * encrypted control doesn't work (pytapo #456). */
        if result["user_group"].as_str().is_some_and(|g| g != "root") {
            return Err(Failure::Refused("this Tapo account is only shared on the camera: use the owner's".into()));
        }
        Ok((
            Conn {
                host: host.to_string(),
                fingerprint: seen.clone(),
                lsk: token("lsk", &cnonce, nonce, &hashed),
                ivb: token("ivb", &cnonce, nonce, &hashed),
                cnonce,
                hashed,
                seq,
                stok: stok.to_string(),
            },
            seen,
        ))
    }

    /* One request (already shaped: {"method": "multipleRequest", ...} or
     * {"method": "set", ...}); the decrypted answer. */
    pub async fn send(&mut self, request: &Value) -> Result<Value, Failure> {
        let encrypted = cbc_encrypt(&self.lsk, &self.ivb, request.to_string().as_bytes());
        let body = json!({"method": "securePassthrough", "params": {
            "request": base64::engine::general_purpose::STANDARD.encode(encrypted),
        }});
        let body_text = body.to_string();
        let tag_key = sha256_upper(&[self.hashed.as_bytes(), self.cnonce.as_bytes()]);
        let tag = sha256_upper(&[tag_key.as_bytes(), body_text.as_bytes(), self.seq.to_string().as_bytes()]);
        let headers = [
            ("Referer", format!("https://{}:{PORT}", self.host)),
            ("requestByApp", "true".to_string()),
            ("Seq", self.seq.to_string()),
            ("Tapo_tag", tag),
        ];
        self.seq += 1;
        let (reply, _) = post(&self.host, &format!("/stok={}/ds", self.stok), &headers, &body, self.fingerprint.as_deref()).await?;
        let Some(response) = reply["result"]["response"].as_str() else {
            /* Not encrypted: an error about the session itself. */
            return Err(Failure::Refused(format!("the camera answered code {}", reply["error_code"])));
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(response)
            .map_err(|_| Failure::Refused("bad answer encoding".into()))?;
        let plain = cbc_decrypt(&self.lsk, &self.ivb, &bytes).map_err(Failure::Refused)?;
        serde_json::from_slice(&plain).map_err(|_| Failure::Refused("the answer isn't JSON".into()))
    }

    /* Several methods at once; each answer's "result" (None if the
     * camera doesn't know that one). */
    pub async fn multiple(&mut self, requests: &[(&str, Value)]) -> Result<Vec<Option<Value>>, Failure> {
        let list: Vec<Value> = requests.iter().map(|(m, p)| json!({"method": m, "params": p})).collect();
        let reply = self.send(&json!({"method": "multipleRequest", "params": {"requests": list}})).await?;
        let responses = reply["result"]["responses"].as_array().cloned().unwrap_or_default();
        Ok((0..requests.len())
            .map(|i| {
                let r = responses.get(i)?;
                (r["error_code"].as_i64().unwrap_or(0) == 0).then(|| r["result"].clone())
            })
            .collect())
    }
}

/* ------------------------------------------------------------------ */
/* Settings                                                            */
/* ------------------------------------------------------------------ */

const READS: [(&str, &str); 6] = [
    ("getDeviceInfo", r#"{"device_info": {"name": ["basic_info"]}}"#),
    ("getLensMaskConfig", r#"{"lens_mask": {"name": ["lens_mask_info"]}}"#),
    ("getDetectionConfig", r#"{"motion_detection": {"name": ["motion_det"]}}"#),
    ("getLastAlarmInfo", r#"{"msg_alarm": {"name": ["chn1_msg_alarm_info"]}}"#),
    ("getLedStatus", r#"{"led": {"name": ["config"]}}"#),
    ("getDayNightModeConfig", r#"{"image": {"name": "common"}}"#),
];

/* What the hub knows of a camera after a read. */
#[derive(Debug, Default, PartialEq)]
pub struct State {
    pub model: String,
    pub mac: String,
    pub firmware: String,
    /* Privacy mode on = the camera is "off". */
    pub privacy: Option<bool>,
    pub options: Vec<Value>,
}

fn on(v: &Value) -> Option<bool> {
    v.as_str().map(|s| s == "on")
}

/* The answers of READS -> State. A camera that doesn't have a setting
 * (no siren, no LED) just doesn't get that option. */
pub fn parse_state(answers: &[Option<Value>]) -> State {
    let get = |i: usize| answers.get(i).cloned().flatten().unwrap_or(Value::Null);
    let info = get(0);
    let basic = &info["device_info"]["basic_info"];
    let mut options = Vec::new();
    if let Some(enabled) = on(&get(2)["motion_detection"]["motion_det"]["enabled"]) {
        options.push(json!({"id": "motion_detection", "name": "Motion detection", "on": enabled}));
    }
    if let Some(enabled) = on(&get(3)["msg_alarm"]["chn1_msg_alarm_info"]["enabled"]) {
        options.push(json!({"id": "alarm", "name": "Alarm on motion (siren and light)", "on": enabled}));
    }
    if let Some(enabled) = on(&get(4)["led"]["config"]["enabled"]) {
        options.push(json!({"id": "status_led", "name": "Status light", "on": enabled}));
    }
    if let Some(mode) = get(5)["image"]["common"]["inf_type"].as_str() {
        options.push(json!({"id": "night_vision", "name": "Night vision", "value": mode, "choices": ["auto", "on", "off"]}));
    }
    State {
        model: basic["device_model"].as_str().or(basic["dev_model"].as_str()).unwrap_or("Tapo camera").to_string(),
        mac: basic["mac"].as_str().unwrap_or_default().to_lowercase().replace('-', ":"),
        firmware: basic["sw_version"].as_str().unwrap_or_default().to_string(),
        privacy: on(&get(1)["lens_mask"]["lens_mask_info"]["enabled"]),
        options,
    }
}

async fn read_state(conn: &mut Conn) -> Result<State, Failure> {
    let requests: Vec<(&str, Value)> = READS.iter().map(|(m, p)| (*m, serde_json::from_str(p).unwrap())).collect();
    Ok(parse_state(&conn.multiple(&requests).await?))
}

/* One change: privacy (the switch) or an option. */
async fn write(conn: &mut Conn, option: &str, on: bool, value: &str) -> Result<(), Failure> {
    let flag = if on { "on" } else { "off" };
    let reply = match option {
        "privacy" => conn.multiple(&[("setLensMaskConfig", json!({"lens_mask": {"lens_mask_info": {"enabled": flag}}}))]).await?,
        "motion_detection" => conn.multiple(&[("setDetectionConfig", json!({"motion_detection": {"motion_det": {"enabled": flag}}}))]).await?,
        "status_led" => conn.multiple(&[("setLedStatus", json!({"led": {"config": {"enabled": flag}}}))]).await?,
        "night_vision" => conn.multiple(&[("setDayNightModeConfig", json!({"image": {"common": {"inf_type": value}}}))]).await?,
        "alarm" => {
            /* Not a multipleRequest on these cameras (pytapo's setAlarm). */
            let answer = conn
                .send(&json!({"method": "set", "msg_alarm": {"chn1_msg_alarm_info": {
                    "alarm_type": "0", "light_type": "0", "enabled": flag, "alarm_mode": ["sound", "light"],
                }}}))
                .await?;
            vec![(answer["error_code"].as_i64().unwrap_or(0) == 0).then_some(answer)]
        }
        other => return Err(Failure::Refused(format!("a Tapo camera has no option {other:?}"))),
    };
    match reply.first() {
        Some(Some(_)) => Ok(()),
        _ => Err(Failure::Refused(format!("the camera refused to change {option}"))),
    }
}

/* ------------------------------------------------------------------ */
/* The adapter                                                         */
/* ------------------------------------------------------------------ */

pub struct Tapo;

impl Adapter for Tapo {
    fn id(&self) -> &'static str {
        "tapo"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            host: device.config.get("host").cloned().unwrap_or_default(),
            fingerprint: device.config.get("cert_sha256").cloned(),
            password: hub.secrets(&device.id).get("password").map(|p| p.expose().to_string()).unwrap_or_default(),
            has_options: device.capabilities.options.is_some(),
            video: video(&device.config, &hub.secrets(&device.id).get("stream_password").map(|p| p.expose().to_string()).unwrap_or_default()),
            has_camera: device.capabilities.camera.is_some(),
            hub,
        };
        /* Live video for the screen finds it by id (camera.rs). */
        if let Some(video) = &task.video {
            super::camera::register(&task.id, &video.snapshots);
        }
        tokio::spawn(task.run(commands_rx));
        DeviceHandle::new(commands)
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(probe(values))
    }
}

fn setup_error(failure: Failure) -> SetupError {
    let kind = match &failure {
        Failure::Unreachable(_) => ErrorKind::Unreachable,
        Failure::WrongPassword | Failure::Suspended(_) | Failure::OtherCamera => ErrorKind::Refused,
        Failure::Refused(_) => ErrorKind::Unsupported,
    };
    SetupError::new(kind, failure.to_string())
}

/* The test step: log in, read it; remembers its certificate and MAC. */
async fn probe(values: &SetupValues) -> Result<Probe, SetupError> {
    let host = values.plain.get("host").cloned().unwrap_or_default();
    let password = values.secret.get("password").map(|p| p.expose().to_string()).unwrap_or_default();
    /* A changed certificate (a firmware update) is accepted here too when
     * the camera passes the password proof -- see Task::conn. */
    let (mut conn, seen) = match Conn::login(&host, &password, values.plain.get("cert_sha256").map(String::as_str)).await {
        Err(Failure::OtherCamera) => Conn::login(&host, &password, None).await,
        other => other,
    }
    .map_err(setup_error)?;
    let state = read_state(&mut conn).await.map_err(setup_error)?;
    let mut probe_values = std::collections::BTreeMap::new();
    if let Some(fingerprint) = seen {
        probe_values.insert("cert_sha256".to_string(), fingerprint);
    }
    if !state.mac.is_empty() {
        probe_values.insert("mac".to_string(), state.mac.clone());
    }
    /* A Camera Account given: one picture, so a wrong one shows now. */
    let stream_password = values.secret.get("stream_password").map(|p| p.expose().to_string()).unwrap_or_default();
    let mut config = values.plain.clone();
    config.insert("host".into(), host.clone());
    let mut video_note = "";
    if let Some(video) = video(&config, &stream_password) {
        video.snapshots.get(false).await.map_err(|e| {
            SetupError::new(ErrorKind::Refused, format!("The camera works, but its video doesn't: {e}. Check the Camera Account's user name and password in the Tapo app"))
        })?;
        video_note = ", video OK";
    }
    let privacy = if state.privacy == Some(true) { ", privacy mode on" } else { "" };
    Ok(Probe {
        values: probe_values,
        name: None,
        summary: format!("{} (firmware {}){privacy}{video_note}", state.model, state.firmware),
    })
}

struct Task {
    id: String,
    host: String,
    fingerprint: Option<String>,
    password: String,
    has_options: bool,
    /* With a Camera Account: the stream URLs and the snapshots. */
    video: Option<Video>,
    has_camera: bool,
    hub: Hub,
}

/* A Tapo's RTSP streams (the Camera Account's user and password). */
struct Video {
    hd: String,
    sd: String,
    snapshots: std::sync::Arc<super::camera::Snapshots>,
}

fn video(config: &std::collections::BTreeMap<String, String>, password: &str) -> Option<Video> {
    let user = config.get("stream_user").filter(|u| !u.is_empty())?;
    let host = config.get("host")?;
    let base = format!("rtsp://{}{host}:554", super::camera::userinfo(user, password));
    let sd = format!("{base}/stream2");
    Some(Video {
        hd: format!("{base}/stream1"),
        snapshots: super::camera::Snapshots::new(super::camera::Source::Stream(sd.clone())),
        sd,
    })
}

impl Task {
    async fn run(mut self, mut commands: mpsc::Receiver<DeviceCmd>) {
        let mut conn: Option<Conn> = None;
        let mut wait = Duration::ZERO;
        let mut retry = POLL;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {
                    match self.poll(&mut conn).await {
                        Ok(()) => {
                            retry = POLL;
                            wait = POLL;
                        }
                        Err(e) => {
                            conn = None;
                            wait = match e {
                                /* Never retry a wrong password: each try
                                 * would count toward the lockout. */
                                Failure::WrongPassword | Failure::OtherCamera => Duration::from_secs(24 * 3600),
                                Failure::Suspended(s) => Duration::from_secs(s + 5),
                                _ => {
                                    retry = (retry * 2).min(RETRY_MAX);
                                    retry
                                }
                            };
                        }
                    }
                }
                cmd = commands.recv() => match cmd {
                    None => return,
                    Some(cmd) => {
                        self.handle(&mut conn, cmd).await;
                        wait = Duration::from_secs(1);
                    }
                },
            }
        }
    }

    /* A logged-in connection (logging in if there's none).
     *
     * A NEW CERTIFICATE (seen on the project's camera after a firmware
     * update) isn't a reason to give up: for Tapo the pin isn't what proves
     * who's there -- device_confirm is (only a camera that knows the
     * password's hash can make it, and every request is encrypted with
     * keys derived from it). So: logged in again without the pin; if that
     * passes the password proof, the new certificate is pinned. */
    async fn conn<'a>(&mut self, conn: &'a mut Option<Conn>) -> Result<&'a mut Conn, Failure> {
        if conn.is_none() {
            let fresh = match Conn::login(&self.host, &self.password, self.fingerprint.as_deref()).await {
                Err(Failure::OtherCamera) => {
                    let (fresh, seen) = Conn::login(&self.host, &self.password, None).await?;
                    if let Some(fingerprint) = seen {
                        println!("tapo: {}: new certificate (firmware update?), the camera passed the password proof: pinned", self.id);
                        self.hub.store_config(&self.id, "cert_sha256", &fingerprint).await;
                        self.fingerprint = Some(fingerprint);
                    }
                    fresh
                }
                other => other?.0,
            };
            *conn = Some(fresh);
        }
        Ok(conn.as_mut().unwrap())
    }

    async fn poll(&mut self, conn: &mut Option<Conn>) -> Result<(), Failure> {
        let result = match self.conn(conn).await {
            Ok(c) => read_state(c).await,
            Err(e) => Err(e),
        };
        /* A session the camera dropped: one fresh login. */
        let result = match result {
            Err(Failure::Refused(_)) => {
                *conn = None;
                match self.conn(conn).await {
                    Ok(c) => read_state(c).await,
                    Err(e) => Err(e),
                }
            }
            other => other,
        };
        match result {
            Ok(state) => {
                if let Some(privacy) = state.privacy {
                    let _ = self.hub.report(&self.id, "switch", json!({ "on": !privacy })).await;
                }
                if !state.options.is_empty() {
                    if !self.has_options {
                        let _ = self.hub.add_capabilities(&self.id, &["options"]).await;
                        self.has_options = true;
                    }
                    let _ = self.hub.report(&self.id, "options", json!({ "options": state.options })).await;
                }
                if self.video.is_some() && !self.has_camera {
                    let _ = self.hub.add_capabilities(&self.id, &["camera"]).await;
                    self.has_camera = true;
                }
                if self.video.is_some() {
                    let _ = self.hub.report(&self.id, "camera", json!({"snapshot": true, "stream": true})).await;
                }
                self.hub.set_online(&self.id, Health::Online).await;
                Ok(())
            }
            Err(e) => {
                println!("tapo: {}: {e}", self.id);
                let health = match e {
                    Failure::WrongPassword | Failure::OtherCamera | Failure::Suspended(_) => Health::Unauthorized,
                    _ => Health::Offline,
                };
                self.hub.set_online(&self.id, health).await;
                Err(e)
            }
        }
    }

    async fn handle(&mut self, conn: &mut Option<Conn>, cmd: DeviceCmd) {
        /* Pictures and stream URLs: no camera API call (ffmpeg reads the
         * stream; a snapshot can take seconds -- answered by its own
         * task). */
        let cmd = match cmd {
            DeviceCmd::Action { capability, name, args, reply } if capability == "camera" => {
                let Some(video) = &self.video else {
                    let _ = reply.send(Err("set the camera's stream account first (Change settings)".into()));
                    return;
                };
                match name.as_str() {
                    "stream" => {
                        let url = if args["quality"] == "sd" { &video.sd } else { &video.hd };
                        let _ = reply.send(Ok(json!({ "url": url })));
                    }
                    _ => {
                        let snapshots = video.snapshots.clone();
                        let live = args["live"].as_bool().unwrap_or(false);
                        tokio::spawn(async move {
                            let result = snapshots
                                .get(live)
                                .await
                                .map(|jpeg| json!({ "jpeg": base64::engine::general_purpose::STANDARD.encode(jpeg) }));
                            let _ = reply.send(result);
                        });
                    }
                }
                return;
            }
            other => other,
        };
        let (option, on, value, reply): (String, bool, String, super::Reply) = match cmd {
            /* The switch: on = privacy mode OFF. */
            DeviceCmd::Command { capability, value, reply } if capability == "switch" && value["on"].is_boolean() => {
                ("privacy".into(), !value["on"].as_bool().unwrap_or(true), String::new(), Box::new(move |r| { let _ = reply.send(r); }))
            }
            DeviceCmd::Action { capability, name, args, reply } if capability == "options" && name == "set" => (
                args["id"].as_str().unwrap_or_default().to_string(),
                args["on"].as_bool().unwrap_or(false),
                args["value"].as_str().unwrap_or_default().to_string(),
                Box::new(move |r: Result<(), String>| { let _ = reply.send(r.map(|_| json!({}))); }),
            ),
            other => {
                other.refuse("a Tapo camera switches privacy mode and sets its options");
                return;
            }
        };
        let result = match self.conn(conn).await {
            Ok(c) => write(c, &option, on, &value).await,
            Err(e) => Err(e),
        };
        if result.is_err() {
            *conn = None;
        }
        reply(result.map_err(|e| e.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* A device_confirm made like pytapo's _validateDeviceConfirm expects:
     * both hash flavours are recognised, a wrong password isn't. */
    #[test]
    fn device_confirm_tells_the_password_and_its_hash() {
        let (cnonce, nonce) = ("0123456789ABCDEF", "6BDCE6A56F06EE78");
        let [sha, md5] = password_hashes("secret");
        for hashed in [&sha, &md5] {
            let confirm_value = format!("{}{nonce}{cnonce}", sha256_upper(&[cnonce.as_bytes(), hashed.as_bytes(), nonce.as_bytes()]));
            assert_eq!(confirm("secret", cnonce, nonce, &confirm_value).as_ref(), Some(hashed));
            assert_eq!(confirm("wrong", cnonce, nonce, &confirm_value), None);
        }
    }

    /* The same value Python's hashlib gives for pytapo's formula
     * (tools/tapo_check.py), password "secret". */
    #[test]
    fn confirm_matches_python() {
        let [sha, _] = password_hashes("secret");
        let value = sha256_upper(&[b"0123456789ABCDEF", sha.as_bytes(), b"6BDCE6A56F06EE78"]);
        assert!(value.starts_with("A0B1124AB846B08C"), "{value}");
    }

    /* LIVE: the TLS handshake and the first login step (no password) with
     * a real camera: `TAPO_HOST=192.168.1.133 cargo test live_tapo_handshake
     * -- --ignored --nocapture`. Never a login attempt. */
    #[tokio::test]
    #[ignore]
    async fn live_tapo_handshake() {
        let host = std::env::var("TAPO_HOST").expect("TAPO_HOST");
        let body = json!({"method": "login", "params": {"cnonce": random_cnonce(), "encrypt_type": "3", "username": USER}});
        match post(&host, "/", &[], &body, None).await {
            Ok((reply, seen)) => println!("handshake OK, certificate {}, answer has nonce: {}", seen.unwrap_or_default(), reply["result"]["data"]["nonce"].is_string()),
            Err(e) => println!("FAILED: {e}"),
        }
    }

    #[test]
    fn requests_survive_the_session_cipher() {
        let key = token("lsk", "AA", "BB", "CC");
        let iv = token("ivb", "AA", "BB", "CC");
        assert_ne!(key, iv);
        let data = br#"{"method":"multipleRequest"}"#;
        assert_eq!(cbc_decrypt(&key, &iv, &cbc_encrypt(&key, &iv, data)).unwrap(), data);
    }

    #[test]
    fn settings_are_read_from_the_answers() {
        let answers = vec![
            Some(json!({"device_info": {"basic_info": {"device_model": "C200", "mac": "8C-86-DD-7F-DD-D8", "sw_version": "1.3.6"}}})),
            Some(json!({"lens_mask": {"lens_mask_info": {"enabled": "off"}}})),
            Some(json!({"motion_detection": {"motion_det": {"enabled": "on", "digital_sensitivity": "50"}}})),
            None, /* no siren on this model */
            Some(json!({"led": {"config": {"enabled": "on"}}})),
            Some(json!({"image": {"common": {"inf_type": "auto"}}})),
        ];
        let state = parse_state(&answers);
        assert_eq!((state.model.as_str(), state.mac.as_str(), state.privacy), ("C200", "8c:86:dd:7f:dd:d8", Some(false)));
        let ids: Vec<&str> = state.options.iter().filter_map(|o| o["id"].as_str()).collect();
        assert_eq!(ids, ["motion_detection", "status_led", "night_vision"]);
    }
}
