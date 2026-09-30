/*
 * ir_blaster.rs -- the adapter for the hub's own IR blaster add-on (issue
 * #42; template templates/ir-blaster.json; firmware and protocol:
 * firmware_ir_blaster/, PROTOCOL.md there).
 *
 * WHAT IT IS. An ESP32 with IR LEDs and an IR receiver, placed right next
 * to ONE device that has an IR remote (an LED strip, a projector, a fan).
 * In the hub it IS that device: "Astronaut", with a remote whose buttons
 * the person teaches by pressing them on the original remote.
 *
 * THE CONNECTION: mutual TLS, pinned both ways. The hub connects (port
 * 7443) with its own identity certificate (tls.rs) as the client
 * certificate; the blaster checks it against the hub it paired with, the
 * hub checks the blaster's against the fingerprint pinned at pairing
 * (config "fingerprint"). Inside: one JSON object per line.
 *
 * SETUP (the wizard):
 *   discover         mDNS _uc-irblaster._tcp -> "host", "blaster_id"
 *   code_from_device the pairing code (action "pair", below): both sides
 *                    prove they know it, without sending it, bound to
 *                    this very TLS connection's certificates -> pin
 *   test             connect pinned, "hello" (probe, below)
 *
 * THE DEVICE'S TASK:
 *   connect   pinned mutual TLS; report the taught buttons + online
 *   session   carry out actions (press -> "send", learn -> "learn"),
 *             ping every PING_EVERY, give up after SILENT_LIMIT of
 *             silence (an unplugged ESP32's connection never closes)
 *   lost      report offline (or "unauthorized" if the blaster refuses
 *             us / shows another certificate: pair again), wait RETRY,
 *             doubling up to RETRY_MAX, try again
 *
 * THE BUTTONS: the `remote` capability with learn: true (device.rs). The
 * codes themselves stay on the hub (ir_codes.rs), keyed by button name;
 * clients only ever see the names. forget / rename only change that store,
 * so they also work while the blaster is offline.
 */
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::client::TlsStream;

use super::net::{self, NetError};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Device, Health, Remote};
use crate::templates::ErrorKind;
use crate::tls;

/* The blaster's TCP port (PROTOCOL.md). */
const PORT: u16 = 7443;

/* The blaster closes a connection silent for 60 s: ping well before. */
const PING_EVERY: Duration = Duration::from_secs(20);
/* Nothing heard for this long (not even a ping reply) = gone. */
const SILENT_LIMIT: Duration = Duration::from_secs(45);
/* How long a request may take. A send is ~0.1 s, with 20 NEC repeats
 * ~2.3 s; a learn waits for the person (its own timeout, + this). */
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/* Waiting between connection attempts (see the header). */
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/* How long "learn" waits for a button press unless the client says. */
const LEARN_DEFAULT_S: u64 = 15;
/* The longest line accepted from the blaster (PROTOCOL.md: 16 KiB). */
const MAX_LINE: usize = 16 * 1024;

type Stream = TlsStream<TcpStream>;

pub struct IrBlaster;

impl Adapter for IrBlaster {
    fn id(&self) -> &'static str {
        "ir-blaster"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            host: device.config.get("host").cloned(),
            pin: device.config.get("fingerprint").cloned(),
            hub,
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
                other => Err(SetupError::new(ErrorKind::Unsupported, format!("ir-blaster has no action {other:?}"))),
            }
        })
    }
}

/* ------------------------------------------------------------------ */
/* Talking to a blaster, one request at a time (setup)                 */
/* ------------------------------------------------------------------ */

/* Opens a connection: pinned to `pin` (None = any certificate, for
 * pairing), always with the hub's own certificate. */
async fn open(host: &str, pin: Option<&str>) -> Result<(Stream, String), NetError> {
    let me = tls::client_identity(&tls::tls_dir()).map_err(|e| NetError::new(ErrorKind::Unsupported, e))?;
    net::tls_open(host, PORT, pin, Some(&me)).await
}

/* A request/reply connection for the wizard's steps. */
struct SetupLink {
    stream: BufReader<Stream>,
    next_id: u64,
}

impl SetupLink {
    async fn request(&mut self, op: &str, fields: Value) -> Result<Value, NetError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({"id": id, "op": op});
        if let (Some(msg), Some(fields)) = (msg.as_object_mut(), fields.as_object()) {
            msg.extend(fields.clone());
        }
        let lost = |e: String| NetError::new(ErrorKind::Unreachable, e);
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
                    /* A paired blaster closes the connection of a hub it
                     * doesn't know right after the handshake. */
                    return Err(NetError::new(ErrorKind::Refused, "the blaster closed the connection".into()));
                }
                let reply: Value = serde_json::from_slice(&reply).map_err(|e| lost(format!("bad reply: {e}")))?;
                if reply["id"] == json!(id) {
                    return Ok(reply);
                }
                /* An event ("heard") meanwhile: not ours. */
            }
        };
        match tokio::time::timeout(REPLY_TIMEOUT, exchange).await {
            Ok(result) => result,
            Err(_) => Err(NetError::new(ErrorKind::Timeout, format!("no answer to {op}"))),
        }
    }
}

async fn setup_link(host: &str, pin: Option<&str>) -> Result<(SetupLink, String), NetError> {
    let (stream, fingerprint) = open(host, pin).await?;
    Ok((
        SetupLink {
            stream: BufReader::new(stream),
            next_id: 1,
        },
        fingerprint,
    ))
}

/* The wizard's test step: the pinned blaster answers "hello" and says
 * it's paired. */
async fn probe(values: &SetupValues) -> Result<Probe, SetupError> {
    let host = values
        .plain
        .get("host")
        .ok_or_else(|| SetupError::new(ErrorKind::Unreachable, "no address"))?;
    let pin = values
        .plain
        .get("fingerprint")
        .ok_or_else(|| SetupError::new(ErrorKind::Refused, "not paired yet"))?;
    let (mut link, _) = setup_link(host, Some(pin)).await?;
    let hello = link.request("hello", json!({})).await?;
    if hello["paired"] != json!(true) {
        return Err(SetupError::new(ErrorKind::Refused, "the blaster forgot the pairing"));
    }
    let blaster_id = hello["device"].as_str().unwrap_or_default().to_string();
    let fw = hello["fw"].as_str().unwrap_or("?");
    Ok(Probe {
        values: [("blaster_id".to_string(), blaster_id.clone())].into(),
        name: None,
        summary: format!("IR blaster {blaster_id}, firmware {fw}"),
    })
}

/* ------------------------------------------------------------------ */
/* Pairing (PROTOCOL.md, "Pairing")                                    */
/* ------------------------------------------------------------------ */

/* The code as the blaster keeps it: 16 characters of Crockford base32,
 * upper case, without the dashes people type. */
fn normalise_code(code: &str) -> Option<String> {
    let code: String = code
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid = code.len() == 16 && code.chars().all(|c| c.is_ascii_digit() || (c.is_ascii_uppercase() && !"ILOU".contains(c)));
    valid.then_some(code)
}

fn proof_message(role: &str, blaster_fp: &str, hub_fp: &str) -> String {
    format!("uc-irb-pair-v1 {role} {blaster_fp} {hub_fp}")
}

fn proof(code: &str, role: &str, blaster_fp: &str, hub_fp: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, code.as_bytes());
    let tag = ring::hmac::sign(&key, proof_message(role, blaster_fp, hub_fp).as_bytes());
    tag.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/* Checks the blaster's proof in constant time (ring::hmac::verify). */
fn proof_is_valid(code: &str, blaster_fp: &str, hub_fp: &str, hex_proof: &str) -> bool {
    let Some(bytes) = decode_hex(hex_proof) else { return false };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, code.as_bytes());
    ring::hmac::verify(&key, proof_message("blaster", blaster_fp, hub_fp).as_bytes(), &bytes).is_ok()
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

/* The wizard's code_from_device step. Returns the blaster's fingerprint
 * (pinned from now on) and its id. */
async fn pair(values: &SetupValues) -> Result<SetupValues, SetupError> {
    let host = values
        .plain
        .get("host")
        .ok_or_else(|| SetupError::new(ErrorKind::Unreachable, "no address"))?;
    let code = values
        .secret
        .get("pairing_code")
        .and_then(|c| normalise_code(c.expose()))
        .ok_or_else(|| SetupError::new(ErrorKind::Refused, "the pairing code has 16 letters and digits (XXXX-XXXX-XXXX-XXXX)"))?;
    let hub_fp = tls::client_identity(&tls::tls_dir())
        .map_err(|e| SetupError::new(ErrorKind::Unsupported, e))?
        .fingerprint;

    let not_ours = || {
        SetupError::new(
            ErrorKind::Refused,
            "the blaster belongs to another hub: type `unpair` in its console (or reset it), then try again",
        )
    };

    let (mut link, blaster_fp) = setup_link(host, None).await?;
    let hello = link.request("hello", json!({})).await.map_err(|e| match e.kind {
        ErrorKind::Refused => not_ours(),
        _ => e.into(),
    })?;
    if hello["paired"] == json!(true) {
        /* It let us in, so it's paired with THIS hub already (a device
         * removed and added again). Unpair and pair afresh: that also
         * proves, with the code, that it's the blaster the person means. */
        link.request("unpair", json!({})).await?;
        drop(link);
        let (again, fp) = setup_link(host, None).await?;
        if fp != blaster_fp {
            return Err(SetupError::new(ErrorKind::Refused, "the blaster's certificate changed during pairing"));
        }
        link = again;
    }

    let reply = link.request("pair", json!({"proof": proof(&code, "hub", &blaster_fp, &hub_fp)})).await?;
    if reply["ok"] != json!(true) {
        let error = reply["error"].as_str().unwrap_or("failed");
        return Err(match error {
            "bad_proof" => SetupError::new(ErrorKind::Refused, "wrong pairing code"),
            "locked" => SetupError::new(ErrorKind::Refused, "too many wrong codes: restart the blaster, then try again"),
            _ => SetupError::new(ErrorKind::Unsupported, format!("pairing failed: {error}")),
        });
    }
    /* The blaster proves it knows the code too -- and saw the same two
     * certificates, so nobody sits in between. */
    if !proof_is_valid(&code, &blaster_fp, &hub_fp, reply["proof"].as_str().unwrap_or_default()) {
        return Err(SetupError::new(
            ErrorKind::Refused,
            "the blaster's answer doesn't match the code: another device may be pretending to be it",
        ));
    }

    let mut result = SetupValues::default();
    result.plain.insert("fingerprint".into(), blaster_fp);
    if let Some(id) = hello["device"].as_str() {
        result.plain.insert("blaster_id".into(), id.to_string());
    }
    Ok(result)
}

/* ------------------------------------------------------------------ */
/* The device's task                                                   */
/* ------------------------------------------------------------------ */

struct Task {
    id: String,
    host: Option<String>,
    /* The blaster's pinned certificate fingerprint (from pairing). */
    pin: Option<String>,
    hub: Hub,
}

/* A request sent, waiting for its reply. */
struct Pending {
    kind: PendingKind,
    deadline: Instant,
}

enum PendingKind {
    Ping,
    Send(oneshot::Sender<Result<Value, String>>),
    Learn {
        button: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

/* One live connection: the write half here, lines read by a separate
 * task (read_lines) and handed over through `incoming`. */
struct Conn {
    writer: WriteHalf<Stream>,
    incoming: mpsc::Receiver<Value>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
    last_heard: Instant,
    last_ping: Instant,
}

enum End {
    /* The device was removed (its command channel closed). */
    Stop,
    /* The connection broke. */
    Lost(String),
}

/* Reads lines from the blaster, forever, as their own task: a line may
 * arrive in pieces, and reading it inside the task's select! would lose
 * the pieces whenever another branch wins. Ends when the connection does
 * (then `incoming` closes, which the session sees). */
async fn read_lines(reader: ReadHalf<Stream>, tx: mpsc::Sender<Value>) {
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

impl Task {
    async fn run(self, mut commands: mpsc::Receiver<DeviceCmd>) {
        /* The taught buttons are known even before the blaster answers. */
        self.report_buttons(self.hub.ir_codes().get(&self.id).into_iter().map(|b| b.button).collect())
            .await;
        let (Some(host), Some(pin)) = (self.host.clone(), self.pin.clone()) else {
            println!("ir-blaster: {} has no address or pairing: pair it again", self.id);
            self.hub.set_online(&self.id, Health::Unauthorized).await;
            while let Some(cmd) = commands.recv().await {
                self.offline_command(cmd);
            }
            return;
        };

        let mut retry = RETRY_MIN;
        loop {
            match open(&host, Some(&pin)).await {
                Ok((stream, _)) => {
                    retry = RETRY_MIN;
                    let (reader, writer) = tokio::io::split(stream);
                    let (tx, incoming) = mpsc::channel(8);
                    tokio::spawn(read_lines(reader, tx));
                    let conn = Conn {
                        writer,
                        incoming,
                        pending: HashMap::new(),
                        next_id: 1,
                        last_heard: Instant::now(),
                        last_ping: Instant::now(),
                    };
                    self.hub.set_online(&self.id, Health::Online).await;
                    match self.session(conn, &mut commands).await {
                        End::Stop => return,
                        End::Lost(why) => println!("ir-blaster: {}: connection lost: {why}", self.id),
                    }
                    self.hub.set_online(&self.id, Health::Offline).await;
                }
                Err(e) => {
                    /* Refused: the blaster shows another certificate, or
                     * closes on us (it was reset / paired elsewhere). */
                    let health = if e.kind == ErrorKind::Refused { Health::Unauthorized } else { Health::Offline };
                    self.hub.set_online(&self.id, health).await;
                }
            }

            /* Wait before the next attempt, answering commands meanwhile.
             * A press or learn tries again at once (the person wants it
             * now, and the blaster may be back). */
            let until = tokio::time::Instant::now() + retry;
            loop {
                tokio::select! {
                    cmd = commands.recv() => match cmd {
                        None => return,
                        Some(cmd) => {
                            if self.offline_command(cmd) {
                                break;
                            }
                        }
                    },
                    _ = tokio::time::sleep_until(until) => break,
                }
            }
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    /* A command while there's no connection. Returns true if it needed
     * the blaster (so: try to reconnect now). */
    fn offline_command(&self, cmd: DeviceCmd) -> bool {
        match cmd {
            DeviceCmd::Action { capability, name, args, reply } if capability == "remote" && matches!(name.as_str(), "forget" | "rename") => {
                let result = self.edit_buttons(&name, &args);
                /* report_buttons is async; the store is the truth, so a
                 * spawned report is fine here. */
                if let Ok(names) = &result {
                    let hub = self.hub.clone();
                    let id = self.id.clone();
                    let names = names.clone();
                    tokio::spawn(async move { report_buttons(&hub, &id, names).await });
                }
                let _ = reply.send(result.map(|_| json!({})));
                false
            }
            cmd => {
                cmd.refuse("The IR blaster isn't reachable right now. Is it plugged in and on the WiFi?");
                true
            }
        }
    }

    /* forget / rename: only the hub's store changes. */
    fn edit_buttons(&self, name: &str, args: &Value) -> Result<Vec<String>, String> {
        let button = args["button"].as_str().unwrap_or_default();
        match name {
            "forget" => self.hub.ir_codes().forget(&self.id, button),
            _ => self.hub.ir_codes().rename(&self.id, button, args["to"].as_str().unwrap_or_default()),
        }
    }

    async fn report_buttons(&self, names: Vec<String>) {
        report_buttons(&self.hub, &self.id, names).await;
    }

    async fn session(&self, mut conn: Conn, commands: &mut mpsc::Receiver<DeviceCmd>) -> End {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let end = loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    None => break End::Stop,
                    Some(cmd) => {
                        if let Err(why) = self.command(&mut conn, cmd).await {
                            break End::Lost(why);
                        }
                    }
                },
                msg = conn.incoming.recv() => match msg {
                    None => break End::Lost("the blaster closed the connection".into()),
                    Some(msg) => {
                        conn.last_heard = Instant::now();
                        self.message(&mut conn, msg).await;
                    }
                },
                _ = tick.tick() => {
                    if conn.last_heard.elapsed() > SILENT_LIMIT {
                        break End::Lost(format!("silent for {} s", SILENT_LIMIT.as_secs()));
                    }
                    if conn.last_ping.elapsed() >= PING_EVERY {
                        conn.last_ping = Instant::now();
                        if let Err(why) = send_request(&mut conn, "ping", json!({}), PendingKind::Ping, REPLY_TIMEOUT).await {
                            break End::Lost(why);
                        }
                    }
                    expire(&mut conn);
                }
            }
        };
        /* Whoever still waits for an answer gets one. */
        for (_, pending) in conn.pending.drain() {
            fail(pending.kind, "The connection to the IR blaster was lost. Try again.");
        }
        end
    }

    /* A command or action from control.rs (already checked by device.rs).
     * Err only if the connection broke while sending. */
    async fn command(&self, conn: &mut Conn, cmd: DeviceCmd) -> Result<(), String> {
        let (capability, name, args, reply) = match cmd {
            DeviceCmd::Action { capability, name, args, reply } => (capability, name, args, reply),
            cmd => {
                cmd.refuse("an IR blaster has no state to set: press its buttons");
                return Ok(());
            }
        };
        if capability != "remote" {
            let _ = reply.send(Err(format!("ir-blaster has no {capability} actions")));
            return Ok(());
        }
        match name.as_str() {
            "press" => {
                let button = args["button"].as_str().unwrap_or_default();
                let Some(code) = self.hub.ir_codes().code(&self.id, button) else {
                    let _ = reply.send(Err(format!("{button:?} isn't taught yet")));
                    return Ok(());
                };
                send_request(conn, "send", json!({"code": code}), PendingKind::Send(reply), REPLY_TIMEOUT).await
            }
            "learn" => {
                let seconds = args["timeout_s"].as_u64().unwrap_or(LEARN_DEFAULT_S);
                let kind = PendingKind::Learn {
                    button: args["button"].as_str().unwrap_or_default().to_string(),
                    reply,
                };
                /* The blaster answers "timeout" itself; our own limit is
                 * only for a blaster that never answers. */
                let limit = Duration::from_secs(seconds) + REPLY_TIMEOUT;
                send_request(conn, "learn", json!({"timeout_ms": seconds * 1000}), kind, limit).await
            }
            "forget" | "rename" => {
                let result = self.edit_buttons(&name, &args);
                if let Ok(names) = &result {
                    self.report_buttons(names.clone()).await;
                }
                let _ = reply.send(result.map(|_| json!({})));
                Ok(())
            }
            other => {
                let _ = reply.send(Err(format!("ir-blaster can't {other}")));
                Ok(())
            }
        }
    }

    /* A line from the blaster: a reply to one of our requests, or an
     * event. */
    async fn message(&self, conn: &mut Conn, msg: Value) {
        let Some(id) = msg["id"].as_u64() else {
            /* "heard": the original remote was used. Later (#42 DoD) this
             * updates the device's state; for now it's only logged. */
            if msg["event"] == json!("heard") {
                println!("ir-blaster: {}: remote pressed: {}", self.id, msg["code"]);
            }
            return;
        };
        let Some(pending) = conn.pending.remove(&id) else { return };
        if msg["ok"] != json!(true) {
            let why = match msg["error"].as_str() {
                Some("timeout") => "No button was pressed on the remote in time. Try again.".to_string(),
                Some(error) => format!("The IR blaster said: {error} ({})", msg["detail"].as_str().unwrap_or("")),
                None => "The IR blaster sent a strange answer".to_string(),
            };
            fail(pending.kind, &why);
            return;
        }
        match pending.kind {
            PendingKind::Ping => {}
            PendingKind::Send(reply) => {
                let _ = reply.send(Ok(json!({})));
            }
            PendingKind::Learn { button, reply } => {
                let code = msg["code"].clone();
                if !code.is_object() {
                    let _ = reply.send(Err("The IR blaster sent no code".into()));
                    return;
                }
                let names = self.hub.ir_codes().learn(&self.id, &button, code.clone());
                self.report_buttons(names).await;
                println!("ir-blaster: {}: learned {button:?}: {code}", self.id);
                let _ = reply.send(Ok(json!({"code": code})));
            }
        }
    }
}

async fn report_buttons(hub: &Hub, id: &str, buttons: Vec<String>) {
    let remote = Remote {
        buttons,
        keyboard: false,
        learn: true,
    };
    if let Err(e) = hub.report(id, "remote", json!(remote)).await {
        println!("ir-blaster: {id}: {e}");
    }
}

/* Sends a request and remembers who waits for the reply. Err if the
 * connection broke. */
async fn send_request(conn: &mut Conn, op: &str, fields: Value, kind: PendingKind, limit: Duration) -> Result<(), String> {
    let id = conn.next_id;
    conn.next_id += 1;
    let mut msg = json!({"id": id, "op": op});
    if let (Some(msg), Some(fields)) = (msg.as_object_mut(), fields.as_object()) {
        msg.extend(fields.clone());
    }
    let mut line = msg.to_string();
    line.push('\n');
    if let Err(e) = conn.writer.write_all(line.as_bytes()).await {
        fail(kind, "The connection to the IR blaster was lost. Try again.");
        return Err(e.to_string());
    }
    conn.pending.insert(
        id,
        Pending {
            kind,
            deadline: Instant::now() + limit,
        },
    );
    Ok(())
}

/* Requests whose reply is overdue get an error. */
fn expire(conn: &mut Conn) {
    let now = Instant::now();
    let overdue: Vec<u64> = conn
        .pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(id, _)| *id)
        .collect();
    for id in overdue {
        if let Some(pending) = conn.pending.remove(&id) {
            fail(pending.kind, "The IR blaster didn't answer in time.");
        }
    }
}

fn fail(kind: PendingKind, why: &str) {
    match kind {
        PendingKind::Ping => {}
        PendingKind::Send(reply) | PendingKind::Learn { reply, .. } => {
            let _ = reply.send(Err(why.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::Registry;
    use crate::control::Control;
    use crate::device::{Capabilities, Source};
    use crate::secrets::{Secret, Secrets};
    use crate::state::{self, Event, Outputs};
    use std::sync::Arc;
    use tokio::sync::{broadcast, watch};

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
     * firmware agree on PROTOCOL.md's formula. */
    #[test]
    fn proofs_match_the_protocol() {
        let code = "5MCNWM1904CHQ2GT";
        let fb = "4c3f7fa94ec5c219b549dc4baf53926057c0c05930d8d8a4b15b938048f401ac";
        let fh = "0".repeat(64);
        let hub = proof(code, "hub", fb, &fh);
        assert_eq!(hub, "280d2b06ae47cf86d87090f87f1fe8f125eb7149a89f0f0b4753f9ad1de956dd");
        let blaster = proof(code, "blaster", fb, &fh);
        assert_eq!(blaster, "796c229f53d5546ff701214199e60d5c1ab108e04952d18347eac2ca186410e1");
        assert!(proof_is_valid(code, fb, &fh, &blaster));
        assert!(!proof_is_valid(code, fb, &fh, &hub));
        assert!(!proof_is_valid("AAAAAAAAAAAAAAAA", fb, &fh, &blaster));
        assert!(!proof_is_valid(code, fb, &fh, "zz"));
    }

    /* ---- against a real blaster on the bench ----
     *
     *   HUB_TLS_DIR=/tmp/irb-test-hub IRB_HOST=192.168.1.143 \
     *   IRB_CODE=XXXX-XXXX-XXXX-XXXX cargo test ir_blaster::tests::live -- --ignored --nocapture
     *
     * The blaster must be unpaired first. Pairs through the wizard's
     * action, runs the test step, then a device task: learn (press a
     * button on a remote within 60 s when asked), press it back, rename,
     * forget -- and unpairs at the end, leaving the blaster free. */
    #[tokio::test]
    #[ignore]
    async fn live() {
        let host = std::env::var("IRB_HOST").expect("set IRB_HOST");
        let code = std::env::var("IRB_CODE").expect("set IRB_CODE");

        let mut values = SetupValues::default();
        values.plain.insert("host".into(), host.clone());
        values.secret.insert("pairing_code".into(), Secret::new(code));
        let paired = pair(&values).await.expect("pairing");
        println!("paired: {:?}", paired.plain);
        values.plain.extend(paired.plain.clone());
        let probe = probe(&values).await.expect("test step");
        println!("test step: {}", probe.summary);

        /* The hub's parts a device task talks to. */
        let device = Device {
            id: "astro".into(),
            name: "Astronaut".into(),
            room: String::new(),
            template: "ir-blaster".into(),
            source: Source::new("ir-blaster"),
            config: values.plain.clone(),
            identity: String::new(),
            online: None,
            capabilities: Capabilities::with_defaults(&["remote".into()]).unwrap(),
        };
        let (state_tx, state_rx) = mpsc::channel(8);
        let (events_tx, mut events) = broadcast::channel(64);
        let outputs = Outputs {
            changed_tx: watch::channel(()).0,
            events_tx,
            save_tx: watch::channel(Vec::new()).0,
        };
        tokio::spawn(state::run(state_rx, [(device.id.clone(), device)].into(), outputs));
        let registry = Arc::new(Registry::new(vec![Box::new(IrBlaster)]));
        let secrets = Arc::new(Secrets::new(Default::default(), watch::channel(Vec::new()).0));
        let control = Control::new(state_tx, registry.clone(), secrets);
        registry.start_all(&control).await;

        /* Online within a few seconds. */
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if control.get("astro").await.unwrap().unwrap().online == Some(Health::Online) {
                break;
            }
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(Event::Changed(_))) | Ok(Ok(_)) => {}
                Ok(Err(_)) => {}
                Err(_) => panic!("never came online"),
            }
        }
        println!("online");

        println!(">>> PRESS A BUTTON ON THE REMOTE NOW (60 s) <<<");
        let learned = control
            .action("astro", "remote", "learn", json!({"button": "Power", "timeout_s": 60}))
            .await
            .expect("learn");
        println!("learned: {learned}");
        let d = control.get("astro").await.unwrap().unwrap();
        assert_eq!(d.capabilities.remote.as_ref().unwrap().buttons, vec!["Power"]);

        tokio::time::sleep(Duration::from_secs(2)).await;
        let started = Instant::now();
        control.action("astro", "remote", "press", json!({"button": "Power"})).await.expect("press");
        println!("pressed Power: {} ms", started.elapsed().as_millis());

        control
            .action("astro", "remote", "rename", json!({"button": "Power", "to": "On/Off"}))
            .await
            .expect("rename");
        /* A pause a person can see: IR receivers (LED strips especially)
         * ignore the same code repeated within a fraction of a second. */
        tokio::time::sleep(Duration::from_secs(3)).await;
        control.action("astro", "remote", "press", json!({"button": "On/Off"})).await.expect("press renamed");
        println!("pressed On/Off (renamed) -- the device should be back as it was");
        control.action("astro", "remote", "forget", json!({"button": "On/Off"})).await.expect("forget");
        let d = control.get("astro").await.unwrap().unwrap();
        assert!(d.capabilities.remote.as_ref().unwrap().buttons.is_empty());

        /* Leave the blaster unpaired. */
        control.remove("astro").await.unwrap();
        let (mut link, _) = setup_link(&host, Some(&values.plain["fingerprint"])).await.unwrap();
        println!("unpair: {}", link.request("unpair", json!({})).await.unwrap());
    }
}
