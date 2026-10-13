// panel.rs -- ui_layer as a WALL PANEL (issue #106): the same touchscreen
// UI on another machine (first: a Raspberry Pi, #107), controlling the hub
// over the home network instead of from the hub itself.
//
// Two modes, one program (UI_MODE in the systemd unit):
//
//   local  (default) the DK2's own screen: ws://127.0.0.1:8080/ws, trusted,
//          everything allowed -- unchanged.
//   panel  another machine: wss://<hub>:8443/ws, the door phones use
//          (backend ws.rs, issue #35). TLS with the hub's certificate
//          PINNED, and the panel's own key ("auth" after connecting).
//
// What a panel keeps (state_dir()/panel.json, readable only by the UI's
// user): where the hub is, its certificate's fingerprint, the panel's
// client id and key. Written once at pairing; the key never leaves it
// except to the pinned hub.
//
// PAIRING, as for a phone without a camera (Local-Security wiki page):
//   1. find the hub: mDNS (_uchub._tcp, hub-wifi's uchub.service) or a
//      typed address;
//   2. connect and READ its certificate's fingerprint -- the panel shows
//      it, the person compares it with the hub's pairing screen. Only then
//      is the code sent: a machine in between (pretending to be the hub)
//      would show another fingerprint;
//   3. connect again, pinned to exactly that fingerprint, and send the
//      code: the hub answers with the panel's key.
// From then on any other certificate is refused -- never trusted silently.
//
// Also here: the tiny mDNS query that finds hubs (one question, one second
// of answers; no daemon, no library) and the stream type ws_client.rs
// talks over (plain TCP for the local door, TLS for a panel).

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The hub's LAN door (backend ws.rs LAN_ADDR).
pub const HUB_PORT: u16 = 8443;
/// The mDNS service hubs announce (hub-wifi's uchub.service).
const SERVICE: &str = "_uchub._tcp.local";
/// Connecting and the TLS handshake: a hub on the LAN answers in well
/// under a second; this is for a wrong address.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether this UI is a wall panel (UI_MODE=panel in its systemd unit).
pub fn panel_mode() -> bool {
    std::env::var("UI_MODE").is_ok_and(|m| m == "panel")
}

// ---------------------------------------------------------------------
// What a paired panel keeps
// ---------------------------------------------------------------------

/// Where the panel's hub is and how it proves itself. Saved once paired.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PanelConfig {
    /// The hub's address as found or typed (an IP or a name).
    pub host: String,
    pub port: u16,
    /// What the person saw: the mDNS name ("stm32mp1"), or the address.
    pub hub_name: String,
    /// SHA-256 of the hub's certificate, hex (backend tls.rs).
    pub fingerprint: String,
    pub client_id: String,
    /// The panel's key. Secret: only in this file (0600), only sent to
    /// the pinned hub.
    pub token: String,
}

/// systemd's StateDirectory (ui-layer.service: /var/lib/ui-layer, owned
/// by the UI's user, 0700).
fn state_dir() -> PathBuf {
    std::env::var_os("STATE_DIRECTORY").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/lib/ui-layer"))
}

fn config_path() -> PathBuf {
    state_dir().join("panel.json")
}

/// The saved pairing, if this panel has one.
pub fn load() -> Option<PanelConfig> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    match serde_json::from_str(&text) {
        Ok(config) => Some(config),
        Err(e) => {
            println!("ui_layer: panel.json unreadable ({e}): pair again");
            None
        }
    }
}

/// Saves the pairing: written to a new file (0600) and renamed over the
/// old one, so a power cut leaves the old file or the new one, never half.
pub fn save(config: &PanelConfig) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = config_path();
    let tmp = path.with_extension("json.new");
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("can't save the pairing ({}): {e}", tmp.display()))?;
    file.write_all(text.as_bytes()).and_then(|_| file.sync_all()).map_err(|e| format!("can't save the pairing: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("can't save the pairing: {e}"))
}

/// Forgets the pairing ("Unpair this panel", or the hub removed it).
pub fn forget() {
    let _ = std::fs::remove_file(config_path());
}

/// The name the panel pairs under, as the hub's Paired devices list shows
/// it: "Wall panel (raspberrypi3)". At most 40 characters (backend auth.rs).
pub fn client_name() -> String {
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let host = host.trim();
    let name = if host.is_empty() { "Wall panel".to_string() } else { format!("Wall panel ({host})") };
    name.chars().take(40).collect()
}

// ---------------------------------------------------------------------
// The connection: plain TCP (local door) or TLS pinned to the hub
// ---------------------------------------------------------------------

/// What the WebSocket runs over: tungstenite takes any Read + Write.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Stream {
    /// Reads give up after this (ws_client's loop checks the GUI's
    /// requests in between).
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Stream::Plain(tcp) => tcp.set_read_timeout(timeout),
            Stream::Tls(tls) => tls.sock.set_read_timeout(timeout),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(tcp) => tcp.read(buf),
            Stream::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(tcp) => tcp.write(buf),
            Stream::Tls(tls) => tls.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(tcp) => tcp.flush(),
            Stream::Tls(tls) => tls.flush(),
        }
    }
}

/// SHA-256, hex: how the hub names its certificate (backend tls.rs).
pub fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The first 16 hex digits in groups of four -- exactly what the hub's
/// pairing screen shows (backend tls.rs short_fingerprint).
pub fn short_fingerprint(fingerprint: &str) -> String {
    fingerprint.as_bytes().chunks(4).take(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

/// Accepts exactly one certificate (`expected`), or -- while pairing,
/// before anything secret is sent -- any, remembering which (`seen`).
/// The hub's certificate is self-signed (backend tls.rs), so there's no
/// authority to check it against: its fingerprint IS its identity. The
/// handshake's signatures are still checked: whoever answers must hold
/// that certificate's private key.
#[derive(Debug)]
struct Pin {
    expected: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = sha256_hex(end_entity.as_ref());
        *self.seen.lock().unwrap() = Some(fingerprint.clone());
        match &self.expected {
            Some(expected) if *expected != fingerprint => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Opens TLS to the hub. `expected`: the pinned fingerprint (None only
/// while pairing, to read it). Returns the stream, handshake done, and
/// the fingerprint the hub showed.
pub fn connect_tls(host: &str, port: u16, expected: Option<&str>) -> Result<(Stream, String), String> {
    let addr = resolve(host, port)?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| format!("can't reach {host}: {e}"))?;
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| e.to_string())?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let pin = Arc::new(Pin { expected: expected.map(str::to_string), seen: Mutex::new(None), provider: provider.clone() });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(pin.clone())
        .with_no_client_auth();
    // The name only goes into the handshake (SNI); the pin decides.
    let name = ServerName::try_from(host.to_string()).map_err(|_| format!("{host:?} isn't an address"))?;
    let connection = ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
    let mut tls = StreamOwned::new(connection, tcp);
    // The handshake now, so a wrong certificate is known here, by name.
    while tls.conn.is_handshaking() {
        if let Err(e) = tls.conn.complete_io(&mut tls.sock) {
            let seen = pin.seen.lock().unwrap().clone();
            return Err(match (expected, seen) {
                (Some(expected), Some(seen)) if expected != seen => format!(
                    "the hub's certificate changed (now {}, paired with {}): not trusted. If the hub was reset, pair this panel again.",
                    short_fingerprint(&seen),
                    short_fingerprint(expected)
                ),
                _ => format!("secure connection to {host} failed: {e}"),
            });
        }
    }
    let seen = pin.seen.lock().unwrap().clone().ok_or("the hub sent no certificate")?;
    Ok((Stream::Tls(Box::new(tls)), seen))
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("can't find {host}: {e}"))?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| format!("can't find {host}"))
}

/// The WebSocket URL for a hub (tungstenite only reads its host and path
/// here: the TLS is already done by connect_tls).
pub fn hub_url(host: &str, port: u16) -> String {
    format!("wss://{host}:{port}/ws")
}

/// One request and its reply, on a connection that isn't serving the GUI
/// yet (pairing, logging in). Waits CONNECT_TIMEOUT at most.
pub fn ask(socket: &mut tungstenite::WebSocket<Stream>, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    socket.send(tungstenite::Message::Text(request.to_string())).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + CONNECT_TIMEOUT * 2;
    loop {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                return serde_json::from_str(&text).map_err(|e| format!("the hub's answer: {e}"));
            }
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
        if Instant::now() > deadline {
            return Err("the hub didn't answer".into());
        }
    }
}

/// Pairing, step 2 (see the top): the hub's certificate, to compare.
pub fn read_fingerprint(host: &str, port: u16) -> Result<String, String> {
    connect_tls(host, port, None).map(|(_, fingerprint)| fingerprint)
}

/// Pairing, step 3: the code, to the hub with exactly `fingerprint`.
/// On success the pairing is saved and returned.
pub fn pair(host: &str, port: u16, hub_name: &str, fingerprint: &str, code: &str) -> Result<PanelConfig, String> {
    let (stream, _) = connect_tls(host, port, Some(fingerprint))?;
    let (mut socket, _) = tungstenite::client(hub_url(host, port), stream).map_err(|e| format!("the hub refused the connection: {e}"))?;
    let reply = ask(
        &mut socket,
        &serde_json::json!({"action": "pair", "code": code.trim(), "client_name": client_name(), "kind": "panel"}),
    )?;
    let _ = socket.close(None);
    match reply["type"].as_str() {
        Some("paired") => {}
        Some("error") => return Err(reply["message"].as_str().unwrap_or("refused").to_string()),
        _ => return Err("unexpected answer from the hub".into()),
    }
    // The hub names the certificate it wants pinned: it must be the one
    // the person compared.
    if reply["hub_fingerprint"].as_str() != Some(fingerprint) {
        return Err("the hub named another certificate than the one shown: not paired".into());
    }
    let text = |key: &str| reply[key].as_str().unwrap_or_default().to_string();
    let config = PanelConfig {
        host: host.to_string(),
        port,
        hub_name: hub_name.to_string(),
        fingerprint: fingerprint.to_string(),
        client_id: text("client_id"),
        token: text("token"),
    };
    if config.client_id.is_empty() || config.token.is_empty() {
        return Err("the hub's answer had no key".into());
    }
    save(&config)?;
    Ok(config)
}

// ---------------------------------------------------------------------
// How warm the panel is (issue #107)
// ---------------------------------------------------------------------

/// The SoC's temperature and, on a Raspberry Pi, whether its firmware is
/// slowing it down -- for Settings > This panel. E.g. "52.1 °C" or
/// "61.3 °C, slowed down (heat)". Empty when the machine doesn't say.
pub fn health_text() -> String {
    let temp = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        .ok()
        .and_then(|t| t.trim().parse::<f32>().ok())
        .map(|milli| format!("{:.1} \u{b0}C", milli / 1000.0));
    // The firmware's flags (the kernel's raspberrypi-hwmon / firmware
    // driver): bit 0 undervoltage now, 1 clock capped, 2 throttled now,
    // 3 soft temperature limit now; 16-19 the same "since boot".
    let throttled = std::fs::read_to_string("/sys/devices/platform/soc/soc:firmware/get_throttled")
        .ok()
        .and_then(|t| u32::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok());
    let mut text = temp.unwrap_or_default();
    if let Some(flags) = throttled {
        let mut why = Vec::new();
        if flags & 0x1 != 0 {
            why.push("power supply too weak");
        }
        if flags & 0xc != 0 {
            why.push("slowed down (heat)");
        }
        if why.is_empty() && flags & 0x50000 != 0 {
            why.push("was slowed down since start");
        }
        if !why.is_empty() {
            text = format!("{text}, {}", why.join(", "));
        }
    }
    text
}

// ---------------------------------------------------------------------
// Finding hubs: one mDNS question
// ---------------------------------------------------------------------

/// A hub on the network.
#[derive(Clone, Debug, PartialEq)]
pub struct FoundHub {
    /// Its mDNS instance name ("stm32mp1").
    pub name: String,
    pub host: String,
    pub port: u16,
}

/// Asks the network "which hubs are there?" and collects the answers for
/// `wait`. A "legacy unicast" question (from an ordinary port, not 5353):
/// responders answer straight to it, with the address records included --
/// no mDNS daemon needed on the panel.
pub fn discover(wait: Duration) -> Vec<FoundHub> {
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else { return vec![] };
    let query = mdns_query(SERVICE);
    let mdns = (Ipv4Addr::new(224, 0, 0, 251), 5353);
    let mut found: Vec<FoundHub> = Vec::new();
    let deadline = Instant::now() + wait;
    // Asked twice: UDP may lose one.
    for _ in 0..2 {
        let _ = socket.send_to(&query, mdns);
        let round = Instant::now() + wait / 2;
        let mut buf = [0u8; 4096];
        while let Some(left) = round.min(deadline).checked_duration_since(Instant::now()) {
            if left.is_zero() || socket.set_read_timeout(Some(left)).is_err() {
                break;
            }
            let Ok((len, from)) = socket.recv_from(&mut buf) else { break };
            for hub in parse_answer(&buf[..len], from) {
                if !found.iter().any(|f| f.host == hub.host && f.port == hub.port) {
                    found.push(hub);
                }
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// A DNS question for `name`'s PTR records (the instances of a service).
fn mdns_query(name: &str) -> Vec<u8> {
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]; // id 0, no flags, 1 question
    q.extend(encode_name(name));
    q.extend([0, 12, 0, 1]); // PTR, IN
    q
}

fn encode_name(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name.split('.').filter(|l| !l.is_empty()) {
        out.push(label.len() as u8);
        out.extend(label.as_bytes());
    }
    out.push(0);
    out
}

/// A name at `pos` (labels, or a pointer to an earlier name: DNS
/// compression). Returns it ("stm32mp1._uchub._tcp.local") and where the
/// next field starts.
fn read_name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    // A pointer loop would never end: at most this many jumps.
    for _ in 0..32 {
        let len = *msg.get(pos)? as usize;
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(pos + 1)));
        }
        if len & 0xc0 == 0xc0 {
            let target = ((len & 0x3f) << 8) | *msg.get(pos + 1)? as usize;
            end.get_or_insert(pos + 2);
            pos = target;
            continue;
        }
        labels.push(String::from_utf8_lossy(msg.get(pos + 1..pos + 1 + len)?).into_owned());
        pos += 1 + len;
    }
    None
}

/// The hubs in one mDNS answer: PTR (service -> instance), SRV (instance
/// -> host name, port), A (host name -> address). Without an A record the
/// address is the one the answer came from.
fn parse_answer(msg: &[u8], from: SocketAddr) -> Vec<FoundHub> {
    let count = |at: usize| msg.get(at..at + 2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]) as usize);
    if msg.len() < 12 || msg[2] & 0x80 == 0 {
        return vec![]; // not an answer
    }
    let (questions, records) = (count(4), count(6) + count(8) + count(10));
    let mut pos = 12;
    for _ in 0..questions {
        let Some((_, next)) = read_name(msg, pos) else { return vec![] };
        pos = next + 4;
    }
    let mut instances: Vec<String> = Vec::new();
    let mut srv: HashMap<String, (String, u16)> = HashMap::new();
    let mut a: HashMap<String, Ipv4Addr> = HashMap::new();
    for _ in 0..records {
        let Some((owner, next)) = read_name(msg, pos) else { break };
        let Some(header) = msg.get(next..next + 10) else { break };
        let kind = u16::from_be_bytes([header[0], header[1]]);
        let len = u16::from_be_bytes([header[8], header[9]]) as usize;
        let data = next + 10;
        let Some(rdata) = msg.get(data..data + len) else { break };
        match kind {
            12 if owner.eq_ignore_ascii_case(SERVICE) => {
                if let Some((instance, _)) = read_name(msg, data) {
                    instances.push(instance);
                }
            }
            33 if len >= 7 => {
                let port = u16::from_be_bytes([rdata[4], rdata[5]]);
                if let Some((target, _)) = read_name(msg, data + 6) {
                    srv.insert(owner.to_ascii_lowercase(), (target.to_ascii_lowercase(), port));
                }
            }
            1 if len == 4 => {
                a.insert(owner.to_ascii_lowercase(), Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]));
            }
            _ => {}
        }
        pos = data + len;
    }
    instances
        .into_iter()
        .map(|instance| {
            let (target, port) = srv.get(&instance.to_ascii_lowercase()).cloned().unwrap_or_default();
            let host = a.get(&target).copied().map_or_else(|| from.ip().to_string(), |ip| ip.to_string());
            let name = instance.strip_suffix(&format!(".{SERVICE}")).unwrap_or(&instance).to_string();
            FoundHub { name, host, port: if port == 0 { HUB_PORT } else { port } }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /* An answer as avahi sends it to a legacy unicast question: the
     * question echoed, the PTR, then SRV and A as extra records -- names
     * compressed (pointers to earlier ones). */
    fn avahi_answer() -> Vec<u8> {
        let mut m = vec![0, 0, 0x84, 0, 0, 1, 0, 1, 0, 0, 0, 2];
        // question at 12: _uchub._tcp.local PTR IN
        m.extend(encode_name("_uchub._tcp.local"));
        m.extend([0, 12, 0, 1]);
        // PTR: owner -> pointer to 12; data: "stm32mp1" + pointer to 12
        m.extend([0xc0, 12, 0, 12, 0, 1, 0, 0, 0, 10, 0, 11]);
        let instance_at = m.len();
        m.push(8);
        m.extend(b"stm32mp1");
        m.extend([0xc0, 12]);
        // SRV: owner -> the instance; target "stm32mp1.local"
        m.extend([0xc0, instance_at as u8, 0, 33, 0x80, 1, 0, 0, 0, 120]);
        let target = encode_name("stm32mp1.local");
        m.extend(((6 + target.len()) as u16).to_be_bytes());
        m.extend([0, 0, 0, 0, 0x20, 0xfb]); // priority, weight, port 8443
        let target_at = m.len();
        m.extend(&target);
        // A: stm32mp1.local -> 192.168.1.140
        m.extend([0xc0, target_at as u8, 0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4, 192, 168, 1, 140]);
        m
    }

    #[test]
    fn a_hub_is_found_in_an_avahi_answer() {
        let from: SocketAddr = "192.168.1.7:5353".parse().unwrap();
        assert_eq!(
            parse_answer(&avahi_answer(), from),
            vec![FoundHub { name: "stm32mp1".into(), host: "192.168.1.140".into(), port: 8443 }]
        );
    }

    #[test]
    fn without_an_address_record_the_sender_is_the_hub() {
        let mut m = avahi_answer();
        m[11] = 1; // only the SRV of the extra records
        let from: SocketAddr = "192.168.1.141:5353".parse().unwrap();
        assert_eq!(parse_answer(&m, from)[0].host, "192.168.1.141");
    }

    #[test]
    fn questions_and_garbage_are_not_hubs() {
        let from: SocketAddr = "10.0.0.1:5353".parse().unwrap();
        assert!(parse_answer(&mdns_query(SERVICE), from).is_empty(), "a question");
        assert!(parse_answer(&[0, 0, 0x84, 0, 0, 0, 0, 1], from).is_empty(), "too short");
        let mut cut = avahi_answer();
        cut.truncate(40);
        assert!(parse_answer(&cut, from).is_empty(), "cut off");
        // A pointer to itself: no endless loop.
        let mut looped = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        looped.extend([0xc0, 12, 0, 12, 0, 1, 0, 0, 0, 0, 0, 2, 0xc0, 12]);
        assert!(parse_answer(&looped, from).is_empty());
    }

    #[test]
    fn fingerprints_read_like_the_hubs_screen() {
        assert_eq!(short_fingerprint("3f2a91c07be4d012ffff"), "3f2a 91c0 7be4 d012");
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /* Against a real hub on the network (not in the normal run):
     *   cargo test --manifest-path tools/ui_preview/Cargo.toml real_hub -- --ignored --nocapture
     * Finds it by mDNS, reads its certificate, then connects pinned to
     * it -- and is refused when pinned to another. Pairs with nothing. */
    #[test]
    #[ignore]
    fn real_hub() {
        let hubs = discover(Duration::from_secs(2));
        println!("found: {hubs:?}");
        let hub = hubs.first().expect("no hub answered on this network");
        let fingerprint = read_fingerprint(&hub.host, hub.port).unwrap();
        println!("certificate {}", short_fingerprint(&fingerprint));
        let (stream, seen) = connect_tls(&hub.host, hub.port, Some(&fingerprint)).unwrap();
        assert_eq!(seen, fingerprint);
        let (mut socket, _) = tungstenite::client(hub_url(&hub.host, hub.port), stream).unwrap();
        let hello = ask(&mut socket, &serde_json::json!({"action": "hello"})).unwrap();
        println!("hello: {hello}");
        assert_eq!(hello["authenticated"], false, "the LAN door asks for a key first");
        let wrong = "00".repeat(32);
        let refused = connect_tls(&hub.host, hub.port, Some(&wrong)).err().unwrap();
        println!("pinned to another certificate: {refused}");
        assert!(refused.contains("certificate changed"));
    }

    #[test]
    fn the_pairing_is_saved_privately_and_read_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ui-panel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: tests in this module don't read the environment
        // concurrently with this one (it's the only one setting it).
        unsafe { std::env::set_var("STATE_DIRECTORY", &dir) };
        let config = PanelConfig {
            host: "192.168.1.140".into(),
            port: 8443,
            hub_name: "stm32mp1".into(),
            fingerprint: "ab".repeat(32),
            client_id: "c-1".into(),
            token: "secret".into(),
        };
        save(&config).unwrap();
        assert_eq!(load(), Some(config));
        let mode = std::fs::metadata(dir.join("panel.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only the UI's user may read the key");
        forget();
        assert_eq!(load(), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
