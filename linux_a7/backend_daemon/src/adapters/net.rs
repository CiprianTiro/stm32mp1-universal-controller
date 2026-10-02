/*
 * net.rs -- the adapters' toolkit (issue #40): the network building blocks
 * adapters share, so an adapter is mostly its protocol's logic.
 *
 *   http_json  one HTTP request with a JSON reply (WLED's /json/state,
 *              the generic HTTP adapter's Shelly, Tasmota, ...);
 *              http_request: the same, any reply
 *   ws_connect a WebSocket to a device (WLED's /ws push channel)
 *   ws_open    a WebSocket on any port, plain or TLS with a PINNED
 *              certificate (the LG TV: its certificate is self-signed, so
 *              the hub trusts the one it saw at pairing -- "trust on first
 *              use" -- and refuses any other afterwards)
 *   wake_on_lan  the "magic packet" that switches on a device whose
 *              network is asleep (the TV in standby)
 *   udp_request  one UDP request/answer, with retries (WiZ bulbs; issue
 *              #75 -- later LIFX, Yeelight's discovery, ...)
 *   TcpClient  a TCP connection exchanging lines or length-prefixed
 *              messages (Yeelight, old Kasa plugs, ...)
 *
 * Every call has a timeout: a device that stops answering mid-request
 * (unplugged, out of WiFi range) must never leave a task waiting forever.
 * Errors are plain sentences ("192.168.1.50 isn't answering"), because
 * they end up in front of a person (a failed command), plus a KIND the
 * wizard's test step turns into its own sentence (NetError).
 *
 * Addresses ("host") are what setup stored in the device's config: an IP
 * address or a name, optionally with a port -- "192.168.1.50",
 * "wled-kitchen.local", "127.0.0.1:8080" (the tests' simulated devices),
 * "[fe80::1]:80".
 */
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{DigitallySignedStruct, SignatureScheme};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::SetupError;
use crate::templates::ErrorKind;

/* A whole request -- connect, send, read the reply -- takes at most this.
 * Devices on the LAN answer in milliseconds; an ESP busy with a WiFi
 * reconnect can take a second or two. */
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/* Replies bigger than this are refused: a device's JSON is a few KB (a
 * full WLED /json about 20 KB); anything huge is a broken or hostile
 * device, and the hub has little RAM to spare. */
const MAX_REPLY: usize = 256 * 1024;

/* The same limit for one WebSocket message... */
const MAX_WS_MESSAGE: usize = 256 * 1024;
/* ...except where a device sends big lists by design: an LG TV sends its
 * whole channel list in ONE message -- 365 KB for ~400 channels on a real
 * TV, so several MB for a satellite TV's thousands. Going over the limit
 * doesn't just fail the one message: the WebSocket library then drops the
 * whole connection. */
pub const LARGE_WS_MESSAGE: usize = 16 * 1024 * 1024;

/* A failed request: what kind of failure, and the sentence. Converts to
 * a String (so `?` works in the adapters' Result<_, String> code) and to
 * a SetupError (the wizard). */
#[derive(Debug)]
pub struct NetError {
    pub kind: ErrorKind,
    pub message: String,
}

impl NetError {
    pub fn new(kind: ErrorKind, message: String) -> Self {
        NetError { kind, message }
    }
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<NetError> for String {
    fn from(e: NetError) -> String {
        e.message
    }
}

impl From<NetError> for SetupError {
    fn from(e: NetError) -> SetupError {
        SetupError::new(e.kind, e.message)
    }
}

/* A WebSocket connection to a device. `MaybeTlsStream`: ws:// today,
 * wss:// (the TV) later, same type. */
pub type WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/* "host" or "host:port" -> "host:port" to connect to. An IPv6 address
 * needs brackets when a port follows ("[fe80::1]:80"); a bare one gets
 * them added. */
pub fn with_port(host: &str, default_port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        return format!("[{host}]:{default_port}");
    }
    let has_port = match host.rsplit_once(':') {
        /* "[v6]:port" or "name:port" -- but not a bare "[v6]" */
        Some((_, port)) => port.parse::<u16>().is_ok(),
        None => false,
    };
    if has_port {
        host.to_string()
    } else {
        format!("{host}:{default_port}")
    }
}

/* One HTTP/1.1 request to http://{host}{path}, with an optional JSON body;
 * returns the reply's JSON. Anything but 2xx is an error. A new
 * connection each time: devices like the ESP close it after every reply
 * anyway ("Connection: close"), and it's one request per command. */
pub async fn http_json(method: Method, host: &str, path: &str, body: Option<&Value>) -> Result<Value, NetError> {
    let bytes = http_request(method, host, path, body).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| NetError::new(ErrorKind::Unsupported, format!("{host} didn't answer with JSON: {e}")))
}

/* The same request, the reply's body as it is (issue #75: a command's
 * answer doesn't have to be JSON -- "OK", XML, nothing). */
pub async fn http_request(method: Method, host: &str, path: &str, body: Option<&Value>) -> Result<Bytes, NetError> {
    match tokio::time::timeout(HTTP_TIMEOUT, http_request_inner(method, host, path, body)).await {
        Ok(result) => result,
        Err(_) => Err(NetError::new(
            ErrorKind::Timeout,
            format!("{host} isn't answering (no reply within {} s)", HTTP_TIMEOUT.as_secs()),
        )),
    }
}

async fn http_request_inner(method: Method, host: &str, path: &str, body: Option<&Value>) -> Result<Bytes, NetError> {
    let unreachable = |e: String| NetError::new(ErrorKind::Unreachable, e);
    /* Answered, but not what an adapter of this kind expects. */
    let unsupported = |e: String| NetError::new(ErrorKind::Unsupported, e);
    let stream = TcpStream::connect(with_port(host, 80))
        .await
        .map_err(|e| unreachable(format!("can't reach {host}: {e}")))?;
    /* hyper's low-level client: a handshake gives a sender (to send
     * requests) and the connection itself, a future that must run for the
     * requests to move -- spawned; it ends when the exchange is done. */
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| unreachable(format!("{host}: {e}")))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let body = match body {
        Some(json) => Bytes::from(json.to_string()),
        None => Bytes::new(),
    };
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(HOST, host)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(body))
        .map_err(|e| unsupported(format!("invalid request for {host}{path}: {e}")))?;
    let reply = sender
        .send_request(request)
        .await
        .map_err(|e| unreachable(format!("{host} broke off the reply: {e}")))?;
    let status = reply.status();
    /* Limited: reading stops with an error past MAX_REPLY bytes. */
    let bytes = Limited::new(reply.into_body(), MAX_REPLY)
        .collect()
        .await
        .map_err(|e| unsupported(format!("{host}: bad reply: {e}")))?
        .to_bytes();
    if !status.is_success() {
        return Err(unsupported(format!("{host} answered {path} with HTTP {status}")));
    }
    Ok(bytes)
}

/* Opens ws://{host}{path}. Within HTTP_TIMEOUT, like a request. */
pub async fn ws_connect(host: &str, path: &str) -> Result<WebSocket, String> {
    let url = format!("ws://{}{path}", with_port(host, 80));
    let config = WebSocketConfig {
        max_message_size: Some(MAX_WS_MESSAGE),
        max_frame_size: Some(MAX_WS_MESSAGE),
        ..Default::default()
    };
    let connect = tokio_tungstenite::connect_async_with_config(url, Some(config), false);
    match tokio::time::timeout(HTTP_TIMEOUT, connect).await {
        Ok(Ok((socket, _response))) => Ok(socket),
        Ok(Err(e)) => Err(format!("{host}: WebSocket refused: {e}")),
        Err(_) => Err(format!("{host} isn't answering (WebSocket)")),
    }
}

/* ------------------------------------------------------------------ */
/* WebSockets on any port, with a pinned TLS certificate               */
/* ------------------------------------------------------------------ */

/* Any byte stream a WebSocket can run over: plain TCP or TLS. Boxed, so
 * ws:// and wss:// connections are one type (AnyWebSocket). */
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub type AnyWebSocket = WebSocketStream<Box<dyn Io>>;

/* How to connect. */
pub enum Transport<'a> {
    Plain,
    /* TLS, trusting only the certificate with this SHA-256 fingerprint
     * (hex); None = any certificate (the first contact: pairing), whose
     * fingerprint is then returned for pinning. */
    Pinned(Option<&'a str>),
}

/* Opens ws(s)://{host}:{port}{path}. With TLS, also returns the
 * certificate's fingerprint. A certificate that doesn't match the pinned
 * one fails with ErrorKind::Refused -- "not the device we paired with". */
pub async fn ws_open(host: &str, port: u16, path: &str, transport: Transport<'_>) -> Result<(AnyWebSocket, Option<String>), NetError> {
    ws_open_sized(host, port, path, transport, MAX_WS_MESSAGE).await
}

/* ws_open with another message size limit (LARGE_WS_MESSAGE). */
pub async fn ws_open_sized(
    host: &str,
    port: u16,
    path: &str,
    transport: Transport<'_>,
    max_message: usize,
) -> Result<(AnyWebSocket, Option<String>), NetError> {
    match tokio::time::timeout(HTTP_TIMEOUT, ws_open_inner(host, port, path, transport, max_message)).await {
        Ok(result) => result,
        Err(_) => Err(NetError::new(ErrorKind::Timeout, format!("{host}:{port} isn't answering"))),
    }
}

async fn ws_open_inner(
    host: &str,
    port: u16,
    path: &str,
    transport: Transport<'_>,
    max_message: usize,
) -> Result<(AnyWebSocket, Option<String>), NetError> {
    let unreachable = |e: String| NetError::new(ErrorKind::Unreachable, e);
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| unreachable(format!("can't reach {host}:{port}: {e}")))?;
    let (stream, scheme, fingerprint): (Box<dyn Io>, &str, Option<String>) = match transport {
        Transport::Plain => (Box::new(tcp), "ws", None),
        Transport::Pinned(expected) => {
            let verifier = Arc::new(PinnedCert::new(expected.map(str::to_string)));
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| unreachable(format!("TLS setup: {e}")))?
                .dangerous()
                .with_custom_certificate_verifier(verifier.clone())
                .with_no_client_auth();
            let name = ServerName::try_from(host.to_string()).map_err(|e| unreachable(format!("{host}: {e}")))?;
            let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await;
            let seen = verifier.seen.lock().unwrap().clone();
            match tls {
                Ok(tls) => (Box::new(tls), "wss", seen),
                Err(_) if expected.is_some() && seen.is_some() && seen.as_deref() != expected => {
                    return Err(NetError::new(
                        ErrorKind::Refused,
                        format!("{host} presented a different certificate than at pairing"),
                    ));
                }
                Err(e) => return Err(unreachable(format!("{host}:{port}: TLS failed: {e}"))),
            }
        }
    };
    let url = format!("{scheme}://{}{path}", with_port(host, port));
    let config = WebSocketConfig {
        max_message_size: Some(max_message),
        max_frame_size: Some(max_message),
        ..Default::default()
    };
    let (socket, _response) = tokio_tungstenite::client_async_with_config(url, stream, Some(config))
        .await
        .map_err(|e| NetError::new(ErrorKind::Unsupported, format!("{host}:{port}: WebSocket refused: {e}")))?;
    Ok((socket, fingerprint))
}

/* A plain TLS connection (no WebSocket on top) to host:port, e.g. an IR
 * blaster's line protocol (issue #42). The server's certificate must have
 * the fingerprint `expected` (None = any: the first contact, pairing);
 * with `client`, the hub also shows its own certificate (mutual TLS).
 * Returns the stream and the server certificate's fingerprint. A
 * different certificate than the pinned one fails with
 * ErrorKind::Refused, like ws_open. */
pub async fn tls_open(
    host: &str,
    port: u16,
    expected: Option<&str>,
    client: Option<&crate::tls::ClientIdentity>,
) -> Result<(tokio_rustls::client::TlsStream<TcpStream>, String), NetError> {
    let open = async {
        let unreachable = |e: String| NetError::new(ErrorKind::Unreachable, e);
        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|e| unreachable(format!("can't reach {host}:{port}: {e}")))?;
        let verifier = Arc::new(PinnedCert::new(expected.map(str::to_string)));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| unreachable(format!("TLS setup: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone());
        let config = match client {
            Some(me) => builder
                .with_client_auth_cert(vec![me.cert.clone()], me.key.clone_key())
                .map_err(|e| unreachable(format!("TLS client certificate: {e}")))?,
            None => builder.with_no_client_auth(),
        };
        let name = ServerName::try_from(host.to_string()).map_err(|e| unreachable(format!("{host}: {e}")))?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await;
        let seen = verifier.seen.lock().unwrap().clone();
        match (tls, seen) {
            (Ok(tls), Some(seen)) => Ok((tls, seen)),
            (Err(_), Some(seen)) if expected.is_some_and(|e| e != seen) => Err(NetError::new(
                ErrorKind::Refused,
                format!("{host} presented a different certificate than at pairing"),
            )),
            (Err(e), _) => Err(unreachable(format!("{host}:{port}: TLS failed: {e}"))),
            (Ok(_), None) => Err(unreachable(format!("{host}:{port}: no certificate seen"))),
        }
    };
    match tokio::time::timeout(HTTP_TIMEOUT, open).await {
        Ok(result) => result,
        Err(_) => Err(NetError::new(ErrorKind::Timeout, format!("{host}:{port} isn't answering"))),
    }
}

/* rustls's check of the server's certificate, replaced by pinning: the
 * usual check (signed by a known authority, for this name) can't work for
 * a self-signed certificate. What still IS checked, as normal: that the
 * server owns the certificate's key (the handshake signatures below) --
 * so a fingerprint match really means "the same device". */
#[derive(Debug)]
struct PinnedCert {
    expected: Option<String>,
    /* The fingerprint seen in the handshake, for the caller. */
    seen: Mutex<Option<String>>,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PinnedCert {
    fn new(expected: Option<String>) -> Self {
        PinnedCert {
            expected,
            seen: Mutex::new(None),
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PinnedCert {
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
            Some(expected) if *expected != fingerprint => Err(rustls::Error::General("certificate changed".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/* ------------------------------------------------------------------ */
/* Raw UDP and TCP (issue #75)                                         */
/* ------------------------------------------------------------------ */

/* (TcpClient has no adapter using it yet -- Yeelight and old Kasa plugs
 * will; it's tested below. The allow goes when the first one comes.) */

/* How long one UDP try waits for its answer. LAN devices answer within
 * milliseconds; a bulb busy with its WiFi can take a few hundred. */
pub const UDP_TRY_TIMEOUT: Duration = Duration::from_millis(1000);
/* Biggest datagram accepted (and the biggest UDP can carry). */
const MAX_DATAGRAM: usize = 65_535;

/* One UDP request: sends `payload` to host (default port `port`) and
 * returns the first answer `accept` takes. UDP can lose a packet without
 * anyone noticing, so it's sent up to `tries` times, each waiting
 * UDP_TRY_TIMEOUT.
 *
 * `accept` skips answers to something else: a WiZ bulb may still be
 * answering an earlier try, or send a status of its own. Only the
 * device's own address is listened to: the socket is CONNECTED to it, so
 * the kernel drops datagrams from anyone else -- and the firewall (#37)
 * lets the answer in as part of an exchange the hub started.
 *
 * Only for commands that are fine to repeat (set the light to 50 %, ask
 * for the state): a retried "toggle" could toggle twice. */
pub async fn udp_request(
    host: &str,
    port: u16,
    payload: &[u8],
    tries: u32,
    accept: impl Fn(&[u8]) -> bool,
) -> Result<Vec<u8>, NetError> {
    let unreachable = |e: String| NetError::new(ErrorKind::Unreachable, e);
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| unreachable(format!("UDP: {e}")))?;
    /* connect() also resolves a name ("bulb.local") and checks there's a
     * route; nothing is sent yet. */
    socket
        .connect(with_port(host, port))
        .await
        .map_err(|e| unreachable(format!("can't reach {host}: {e}")))?;
    let mut buf = vec![0u8; MAX_DATAGRAM];
    for _ in 0..tries.max(1) {
        socket
            .send(payload)
            .await
            .map_err(|e| unreachable(format!("can't reach {host}: {e}")))?;
        let deadline = tokio::time::Instant::now() + UDP_TRY_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
                Ok(Ok(len)) if accept(&buf[..len]) => return Ok(buf[..len].to_vec()),
                /* Something else: keep listening until this try's time is up. */
                Ok(Ok(_)) => continue,
                /* "Connection refused": the host said nothing listens on
                 * that port (an ICMP message) -- trying again won't help. */
                Ok(Err(e)) => return Err(unreachable(format!("can't reach {host}: {e}"))),
                Err(_) => break,
            }
        }
    }
    Err(NetError::new(
        ErrorKind::Timeout,
        format!("{host} isn't answering (UDP, {} tries)", tries.max(1)),
    ))
}

/* How messages are cut out of a TCP byte stream. */
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub enum Framing {
    /* Text lines ending with this ("\r\n" for Yeelight, "\n" for most). */
    Lines(&'static str),
    /* A 4-byte big-endian length, then that many bytes (old TP-Link
     * Kasa, and many binary protocols). */
    Length32,
}

/* A TCP connection to a device that exchanges whole messages ("frames").
 * Every read and write has a timeout; a frame bigger than MAX_REPLY is
 * refused (a broken or hostile device must not fill the hub's RAM). */
#[allow(dead_code)]
pub struct TcpClient {
    stream: tokio::io::BufReader<TcpStream>,
    framing: Framing,
    host: String,
}

#[allow(dead_code)]
impl TcpClient {
    /* Connects within HTTP_TIMEOUT. */
    pub async fn connect(host: &str, port: u16, framing: Framing) -> Result<TcpClient, NetError> {
        let connect = TcpStream::connect(with_port(host, port));
        let stream = match tokio::time::timeout(HTTP_TIMEOUT, connect).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => return Err(NetError::new(ErrorKind::Unreachable, format!("can't reach {host}: {e}"))),
            Err(_) => return Err(NetError::new(ErrorKind::Timeout, format!("{host} isn't answering (TCP)"))),
        };
        /* Small messages, sent at once (no Nagle delay). */
        let _ = stream.set_nodelay(true);
        Ok(TcpClient {
            stream: tokio::io::BufReader::new(stream),
            framing,
            host: host.to_string(),
        })
    }

    /* Sends one message (the line ending / length prefix is added here). */
    pub async fn send(&mut self, message: &[u8]) -> Result<(), NetError> {
        use tokio::io::AsyncWriteExt;
        let mut frame = Vec::with_capacity(message.len() + 4);
        match self.framing {
            Framing::Lines(end) => {
                frame.extend_from_slice(message);
                frame.extend_from_slice(end.as_bytes());
            }
            Framing::Length32 => {
                let len = u32::try_from(message.len())
                    .map_err(|_| NetError::new(ErrorKind::Unsupported, "message too long".into()))?;
                frame.extend_from_slice(&len.to_be_bytes());
                frame.extend_from_slice(message);
            }
        }
        let write = self.stream.get_mut().write_all(&frame);
        match tokio::time::timeout(HTTP_TIMEOUT, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(NetError::new(ErrorKind::Unreachable, format!("{}: connection lost: {e}", self.host))),
            Err(_) => Err(NetError::new(ErrorKind::Timeout, format!("{} isn't reading (TCP)", self.host))),
        }
    }

    /* The next message (without its line ending / length), waiting at
     * most `wait`. Ok(None): nothing arrived in time -- normal for
     * devices that only speak when something changes. A closed
     * connection is an error. */
    pub async fn receive(&mut self, wait: Duration) -> Result<Option<Vec<u8>>, NetError> {
        let host = self.host.clone();
        let lost = |e: String| NetError::new(ErrorKind::Unreachable, format!("{host}: {e}"));
        let read = async {
            match self.framing {
                Framing::Lines(end) => read_line(&mut self.stream, end.as_bytes()).await,
                Framing::Length32 => read_length32(&mut self.stream).await,
            }
        };
        match tokio::time::timeout(wait, read).await {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(e)) => Err(lost(e)),
            Err(_) => Ok(None),
        }
    }

    /* Sends a message and returns the first answer `accept` takes, within
     * HTTP_TIMEOUT (other messages arriving meanwhile are skipped). */
    pub async fn request(&mut self, message: &[u8], accept: impl Fn(&[u8]) -> bool) -> Result<Vec<u8>, NetError> {
        self.send(message).await?;
        let deadline = tokio::time::Instant::now() + HTTP_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.receive(left).await? {
                Some(frame) if accept(&frame) => return Ok(frame),
                Some(_) if !left.is_zero() => continue,
                _ => {
                    return Err(NetError::new(
                        ErrorKind::Timeout,
                        format!("{} isn't answering (no reply within {} s)", self.host, HTTP_TIMEOUT.as_secs()),
                    ))
                }
            }
        }
    }
}

#[allow(dead_code)]
/* Bytes up to `end` (not included). Byte by byte from a BufReader, which
 * is cheap: the reader asks the socket for big chunks. */
async fn read_line(stream: &mut tokio::io::BufReader<TcpStream>, end: &[u8]) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    loop {
        let byte = stream.read_u8().await.map_err(|e| format!("connection lost: {e}"))?;
        line.push(byte);
        if line.ends_with(end) {
            line.truncate(line.len() - end.len());
            return Ok(line);
        }
        if line.len() > MAX_REPLY {
            return Err(format!("a line longer than {MAX_REPLY} bytes"));
        }
    }
}

#[allow(dead_code)]
async fn read_length32(stream: &mut tokio::io::BufReader<TcpStream>) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let len = stream.read_u32().await.map_err(|e| format!("connection lost: {e}"))? as usize;
    if len > MAX_REPLY {
        return Err(format!("a message of {len} bytes (at most {MAX_REPLY} accepted)"));
    }
    let mut frame = vec![0u8; len];
    stream.read_exact(&mut frame).await.map_err(|e| format!("connection lost: {e}"))?;
    Ok(frame)
}

/* ------------------------------------------------------------------ */
/* Wake-on-LAN                                                         */
/* ------------------------------------------------------------------ */

/* Sends the "magic packet" for `mac` (aa:bb:cc:dd:ee:ff): 6 x 0xFF, then
 * the MAC 16 times, to UDP port 9. A sleeping device's network chip
 * watches for exactly that and wakes the device. Sent three ways -- to the
 * device's last address, its network's broadcast (assuming the usual /24
 * home network) and the general broadcast -- three times each, since UDP
 * can get lost and nothing answers. */
pub async fn wake_on_lan(mac: &str, last_address: Option<&str>) -> Result<(), String> {
    let bytes = parse_mac(mac).ok_or_else(|| format!("invalid MAC address {mac:?}"))?;
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }
    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| format!("wake-on-LAN: {e}"))?;
    socket.set_broadcast(true).map_err(|e| format!("wake-on-LAN: {e}"))?;
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if let Some(ip) = last_address.and_then(|a| a.parse::<Ipv4Addr>().ok()) {
        let [a, b, c, _] = ip.octets();
        targets.push(ip);
        targets.push(Ipv4Addr::new(a, b, c, 255));
    }
    let mut sent = false;
    for _ in 0..3 {
        for target in &targets {
            sent |= socket.send_to(&packet, (*target, 9)).await.is_ok();
        }
    }
    if sent {
        Ok(())
    } else {
        Err("wake-on-LAN: the packet couldn't be sent (no network?)".into())
    }
}

/* The MAC of `ip` from the kernel's neighbour (ARP) table -- known once
 * the hub has talked to it. For devices that don't say their MAC
 * themselves; Wake-on-LAN needs it. Lines of /proc/net/arp:
 *   IP address  HW type  Flags  HW address         Mask  Device
 *   192.168.1.40 0x1     0x2    64:cb:e9:8c:b0:94  *     eth0
 * (flags 0x2 = complete; an incomplete entry has 00:00:00:00:00:00). */
pub fn mac_from_arp(ip: &str) -> Option<String> {
    let table = std::fs::read_to_string("/proc/net/arp").ok()?;
    mac_in_arp_table(&table, ip)
}

fn mac_in_arp_table(table: &str, ip: &str) -> Option<String> {
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (address, flags, mac) = (fields.first()?, fields.get(2)?, fields.get(3)?);
        let complete = u32::from_str_radix(flags.trim_start_matches("0x"), 16).is_ok_and(|f| f & 0x2 != 0);
        (*address == ip && complete && *mac != "00:00:00:00:00:00" && parse_mac(mac).is_some())
            .then(|| mac.to_lowercase())
    })
}

/* "aa:bb:cc:dd:ee:ff" / "aa-bb-..." / "aabbccddeeff" -> 6 bytes. */
fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let digits: String = mac.chars().filter(|c| !matches!(c, ':' | '-')).collect();
    if digits.len() != 12 {
        return None;
    }
    let mut bytes = [0u8; 6];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(digits.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_added_only_when_missing() {
        assert_eq!(with_port("192.168.1.50", 80), "192.168.1.50:80");
        assert_eq!(with_port("127.0.0.1:8080", 80), "127.0.0.1:8080");
        assert_eq!(with_port("wled.local", 80), "wled.local:80");
        assert_eq!(with_port("fe80::1", 80), "[fe80::1]:80");
        assert_eq!(with_port("[fe80::1]:81", 80), "[fe80::1]:81");
    }

    #[test]
    fn arp_table_lookup() {
        let table = "IP address       HW type     Flags       HW address            Mask     Device\n\
                     192.168.1.140    0x1         0x2         64:CB:E9:8C:B0:94     *        eth0\n\
                     192.168.1.9      0x1         0x0         00:00:00:00:00:00     *        eth0\n";
        assert_eq!(mac_in_arp_table(table, "192.168.1.140").as_deref(), Some("64:cb:e9:8c:b0:94"));
        assert_eq!(mac_in_arp_table(table, "192.168.1.9"), None);
        assert_eq!(mac_in_arp_table(table, "192.168.1.1"), None);
    }

    #[test]
    fn macs_parse() {
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:0f"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x0f]));
        assert_eq!(parse_mac("AABBCCDDEE0F"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x0f]));
        assert_eq!(parse_mac("aa:bb"), None);
        assert_eq!(parse_mac("zz:bb:cc:dd:ee:ff"), None);
    }

    #[tokio::test]
    async fn udp_requests_retry_and_skip_other_answers() {
        /* A "device" that ignores the first packet (lost), then answers
         * with something unrelated before the real answer. */
        let device = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = device.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let (_, _) = device.recv_from(&mut buf).await.unwrap();
            let (len, from) = device.recv_from(&mut buf).await.unwrap();
            device.send_to(b"noise", from).await.unwrap();
            let mut reply = b"re:".to_vec();
            reply.extend_from_slice(&buf[..len]);
            device.send_to(&reply, from).await.unwrap();
        });
        let reply = udp_request("127.0.0.1", addr.port(), b"ping", 3, |r| r.starts_with(b"re:")).await.unwrap();
        assert_eq!(reply, b"re:ping");
    }

    #[tokio::test]
    async fn udp_requests_time_out() {
        /* Bound but silent: every try times out. */
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = silent.local_addr().unwrap().port();
        let err = udp_request("127.0.0.1", port, b"x", 2, |_| true).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(err.message.contains("2 tries"), "{err}");
    }

    #[tokio::test]
    async fn tcp_lines_and_length_frames() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            /* 1st connection: lines. Pushes a status first, then echoes. */
            let (mut s, _) = listener.accept().await.unwrap();
            s.write_all(b"status\r\n").await.unwrap();
            let mut buf = [0u8; 64];
            let len = s.read(&mut buf).await.unwrap();
            s.write_all(b"ok:").await.unwrap();
            s.write_all(&buf[..len]).await.unwrap();
            /* 2nd: length-prefixed. */
            let (mut s, _) = listener.accept().await.unwrap();
            let len = s.read_u32().await.unwrap() as usize;
            let mut msg = vec![0u8; len];
            s.read_exact(&mut msg).await.unwrap();
            msg.reverse();
            s.write_u32(msg.len() as u32).await.unwrap();
            s.write_all(&msg).await.unwrap();
            /* then a huge length: refused, not allocated. */
            s.write_u32(u32::MAX).await.unwrap();
        });
        let mut c = TcpClient::connect(&addr, 0, Framing::Lines("\r\n")).await.unwrap();
        let reply = c.request(b"hello", |r| r.starts_with(b"ok:")).await.unwrap();
        /* The pushed "status" was skipped; the line ending is removed. */
        assert_eq!(reply, b"ok:hello");

        let mut c = TcpClient::connect(&addr, 0, Framing::Length32).await.unwrap();
        assert_eq!(c.request(b"abc", |_| true).await.unwrap(), b"cba");
        /* The 4 GB "message" is refused before anything is allocated. */
        assert!(c.receive(Duration::from_secs(1)).await.is_err());
    }

    #[tokio::test]
    async fn unreachable_hosts_fail_with_a_readable_error() {
        /* Port 9 on localhost: nothing listens there, refused at once. */
        let err = http_json(Method::GET, "127.0.0.1:9", "/json", None).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unreachable);
        assert!(err.message.starts_with("can't reach 127.0.0.1:9"), "{err}");
    }
}
