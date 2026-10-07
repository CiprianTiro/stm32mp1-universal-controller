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
 *   https_pinned  one HTTPS POST to a device with a PINNED certificate
 *              (issue #74: Tapo cameras; same trust-on-first-use)
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
    /* Issue #74 (https_pinned): also old devices' certificates -- X.509
     * version 1 and 1024-bit RSA keys (a Tapo camera after a firmware
     * update), which the usual checks refuse to read. See verify_raw. */
    legacy: bool,
}

impl PinnedCert {
    fn new(expected: Option<String>) -> Self {
        PinnedCert {
            expected,
            seen: Mutex::new(None),
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
            legacy: false,
        }
    }

    fn legacy(expected: Option<String>) -> Self {
        PinnedCert { legacy: true, ..PinnedCert::new(expected) }
    }
}

/* One DER element at the start of `data`: (tag, its content, the bytes
 * after it). */
fn der(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *data.first()?;
    let first = *data.get(1)? as usize;
    let (length, header) = if first < 0x80 {
        (first, 2)
    } else {
        let count = first & 0x7F;
        if count == 0 || count > 4 {
            return None;
        }
        let mut length = 0usize;
        for i in 0..count {
            length = (length << 8) | *data.get(2 + i)? as usize;
        }
        (length, 2 + count)
    };
    let content = data.get(header..header + length)?;
    Some((tag, content, &data[header + length..]))
}

/* A certificate's public key, read straight from its DER -- the same place
 * in every X.509 version: (is it RSA, the key). */
fn certificate_key(cert: &[u8]) -> Option<(bool, &[u8])> {
    let (_, certificate, _) = der(cert)?;
    let (_, tbs, _) = der(certificate)?;
    let mut rest = tbs;
    /* [0] version: absent in version 1. */
    if rest.first() == Some(&0xA0) {
        rest = der(rest)?.2;
    }
    /* serial, signature algorithm, issuer, validity, subject. */
    for _ in 0..5 {
        rest = der(rest)?.2;
    }
    let (_, spki, _) = der(rest)?;
    let (_, algorithm, after) = der(spki)?;
    let (_, oid, _) = der(algorithm)?;
    let (_, bits, _) = der(after)?;
    /* rsaEncryption 1.2.840.113549.1.1.1 */
    let rsa = oid == [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
    /* A BIT STRING starts with its count of unused bits (0). */
    Some((rsa, bits.get(1..)?))
}

/* The handshake signature, checked with the key read by certificate_key
 * (legacy devices only). 1024-bit RSA: ring's legacy PKCS#1 checks (no
 * PSS at that size -- such devices are only offered PKCS#1). */
fn verify_raw(cert: &[u8], message: &[u8], scheme: SignatureScheme, signature: &[u8]) -> bool {
    use ring::signature as sig;
    let Some((rsa, key)) = certificate_key(cert) else { return false };
    let algorithm: &dyn sig::VerificationAlgorithm = match (scheme, rsa) {
        (SignatureScheme::RSA_PKCS1_SHA256, true) => &sig::RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
        (SignatureScheme::RSA_PKCS1_SHA512, true) => &sig::RSA_PKCS1_1024_8192_SHA512_FOR_LEGACY_USE_ONLY,
        (SignatureScheme::RSA_PKCS1_SHA384, true) => &sig::RSA_PKCS1_2048_8192_SHA384,
        (SignatureScheme::ECDSA_NISTP256_SHA256, false) => &sig::ECDSA_P256_SHA256_ASN1,
        (SignatureScheme::ECDSA_NISTP384_SHA384, false) => &sig::ECDSA_P384_SHA384_ASN1,
        (SignatureScheme::ED25519, false) => &sig::ED25519,
        _ => return false,
    };
    sig::UnparsedPublicKey::new(algorithm, key).verify(message, signature).is_ok()
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
        match rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms) {
            Err(_) if self.legacy && verify_raw(cert.as_ref(), message, dss.scheme, dss.signature()) => {
                Ok(HandshakeSignatureValid::assertion())
            }
            other => other,
        }
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
        let schemes = self.algorithms.supported_schemes();
        if !self.legacy {
            return schemes;
        }
        /* Legacy: PKCS#1 only for RSA (a 1024-bit key can't be checked
         * with PSS here). */
        schemes
            .into_iter()
            .filter(|s| !matches!(s, SignatureScheme::RSA_PSS_SHA256 | SignatureScheme::RSA_PSS_SHA384 | SignatureScheme::RSA_PSS_SHA512))
            .collect()
    }
}

/* Issue #43: one plain HTTP POST of any body (ONVIF's SOAP XML); the
 * reply's body whatever its status -- SOAP errors come as HTTP 400/500
 * with the reason inside. */
pub async fn http_request_raw(host: &str, path: &str, content_type: &str, body: Vec<u8>) -> Result<Bytes, NetError> {
    let unreachable = |e: String| NetError::new(ErrorKind::Unreachable, e);
    let work = async {
        let stream = TcpStream::connect(with_port(host, 80))
            .await
            .map_err(|e| unreachable(format!("can't reach {host}: {e}")))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| unreachable(format!("{host}: {e}")))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(HOST, host)
            .header(CONTENT_TYPE, content_type)
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| NetError::new(ErrorKind::Unsupported, format!("{host}: {e}")))?;
        let reply = sender
            .send_request(request)
            .await
            .map_err(|e| unreachable(format!("{host} broke off the reply: {e}")))?;
        Limited::new(reply.into_body(), MAX_REPLY)
            .collect()
            .await
            .map(|b| b.to_bytes())
            .map_err(|e| NetError::new(ErrorKind::Unsupported, format!("{host}: bad reply: {e}")))
    };
    /* A PullMessages waits up to 20 s by design. */
    match tokio::time::timeout(Duration::from_secs(30), work).await {
        Ok(result) => result,
        Err(_) => Err(NetError::new(ErrorKind::Timeout, format!("{host} isn't answering"))),
    }
}

/* Issue #74: one HTTPS POST (a JSON body) to a device whose certificate is
 * self-signed: pinned like ws_open's -- `expected` None at setup (any
 * certificate, its fingerprint returned to be saved), Some afterwards
 * (only that one). Returns the reply's body, its HTTP status and the
 * fingerprint seen. `headers`: extra ones (a Tapo camera's Seq, Tapo_tag). */
pub async fn https_pinned(
    host: &str,
    port: u16,
    path: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
    expected: Option<&str>,
) -> Result<(Bytes, u16, Option<String>), NetError> {
    match tokio::time::timeout(HTTP_TIMEOUT, https_pinned_inner(host, port, path, headers, body, expected)).await {
        Ok(result) => result,
        Err(_) => Err(NetError::new(
            ErrorKind::Timeout,
            format!("{host} isn't answering (no reply within {} s)", HTTP_TIMEOUT.as_secs()),
        )),
    }
}

async fn https_pinned_inner(
    host: &str,
    port: u16,
    path: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
    expected: Option<&str>,
) -> Result<(Bytes, u16, Option<String>), NetError> {
    let unreachable = |e: String| NetError::new(ErrorKind::Unreachable, e);
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| unreachable(format!("can't reach {host}:{port}: {e}")))?;
    let verifier = Arc::new(PinnedCert::legacy(expected.map(str::to_string)));
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
    let tls = match tls {
        Ok(tls) => tls,
        Err(_) if expected.is_some() && seen.is_some() && seen.as_deref() != expected => {
            return Err(NetError::new(
                ErrorKind::Refused,
                format!("{host} presented a different certificate than at setup"),
            ));
        }
        Err(e) => return Err(unreachable(format!("{host}:{port}: TLS failed: {e}"))),
    };
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|e| unreachable(format!("{host}: {e}")))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(HOST, format!("{host}:{port}"))
        .header(CONTENT_TYPE, "application/json; charset=UTF-8");
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| NetError::new(ErrorKind::Unsupported, format!("{host}: {e}")))?;
    let reply = sender
        .send_request(request)
        .await
        .map_err(|e| unreachable(format!("{host} broke off the reply: {e}")))?;
    let status = reply.status().as_u16();
    let bytes = Limited::new(reply.into_body(), MAX_REPLY)
        .collect()
        .await
        .map_err(|e| NetError::new(ErrorKind::Unsupported, format!("{host}: bad reply: {e}")))?
        .to_bytes();
    Ok((bytes, status, seen))
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

#[cfg(test)]
mod legacy_cert_tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    /* The project's Tapo camera's certificate after its firmware update:
     * X.509 version 1, RSA 1024 -- its key is found. */
    #[test]
    fn a_version_1_certificate_key_is_read() {
        let cam = hex("308201e730820150021330306431313637303739303239646464316400300d06092a864886f70d01010b050030323114301206035504030c0b545052492d444556494345310d300b060355040a0c0454505249310b30090603550406130255533020170d3031303130313030303030305a180f32303730313233313233353935395a30323114301206035504030c0b545052492d444556494345310d300b060355040a0c0454505249310b300906035504061302555330819f300d06092a864886f70d010101050003818d0030818902818100b824a5905d28da6b78f62636cce7cb6628e375c91d981e5160242fa653b829ed03941b8525b238dbc6d7bae791bf228c7b51fcc357de1ff8946614653d22c5d7fa097142a464eee70072baa64da6b87f7de20e91663414de80847638b19db94ee5baba02a17cff82690e32d37a5ffb20036502118c31d34fec4ad47585a81f2f0203010001300d06092a864886f70d01010b050003818100770cc80200ed30eec62e32ad638e9e41cd79c7a098b211c32a47cfaa7e93176765e3ce41a43e04ef2bfd236df1f7d725f46345865bd8d3290d63cf9fe879142fd2b585f6dbbe1bd941ad9c6e7fcff80ecf2ee698e9d981026c00e11ff1b4e814612f325fd936ec4428148f96ebc265c981f2cb112b06cdfe105f4b472d13cf14");
        let (rsa, key) = certificate_key(&cam).unwrap();
        assert!(rsa);
        /* RSAPublicKey: SEQUENCE { modulus, exponent 65537 }. */
        assert_eq!(key[0], 0x30);
        assert!(key.ends_with(&[0x02, 0x03, 0x01, 0x00, 0x01]));
    }

    /* Signatures made with OpenSSL (1024-bit RSA, PKCS#1 SHA-256; EC P-256)
     * are accepted; a changed message isn't. */
    #[test]
    fn legacy_signatures_are_checked() {
        let message = b"tls handshake message";
        let (rsa_cert, rsa_sig) = (hex("308201d63082013fa00302010202141b2e1d51cb5333f3107f3eca845b287f52547427300d06092a864886f70d01010b050030163114301206035504030c0b545052492d444556494345301e170d3236313030333233303732355a170d3336303933303233303732355a30163114301206035504030c0b545052492d44455649434530819f300d06092a864886f70d010101050003818d0030818902818100abc68c1d4c61c00e65ce2923929afd2c7139c7211eb3fa5376927165dde525e514b171ec8e6906110311af1ae74310361983f46df928416de1d34f594abfb1f7d74098408e81de3139596deefd822abf72f44c2ad60f5160b0840ac3de758aad888dfb7ad16ee8e1dda0d1e1d68ce1742efcd0e9af572f750389fc5d814c44610203010001a321301f301d0603551d0e0416041424d6c23db100e80a556c35ac3b15908553189b6b300d06092a864886f70d01010b0500038181006184eeeb0bf7ff4b2f187e25ae5a52fd0de38709033090370517b54a38bb6aeee52783757ea188ef5d77610607a5cc49b675396380d2d1ef63ca898f7205f33126a2d84177ef208ecd6d2a3bd5177dd5aafe2140101d2a2e334c6654cd42b6a1cfa9ac871c3f40c93c53b5c9d332a8e6208e353ff087a6d7118499c61a736497"), hex("9bfeba4eff6cdb2058f5fcf397cca23e40f4e7ec2a8842d5f8e0226df5e34797cab691e3fd804abbbd46c60ef6c9384b0d10054a7a85911224c2b96a8b0c07955131a1bb2cc0da1ef4b59287c0286e2421b53fc4ccfe738c9a3b5c3447c726904dff60a4068d2eb32df4b4c8835280369e74c4d40f0bdba1a3b5aed2c8fd9362"));
        assert!(verify_raw(&rsa_cert, message, SignatureScheme::RSA_PKCS1_SHA256, &rsa_sig));
        assert!(!verify_raw(&rsa_cert, b"tls handshake massage", SignatureScheme::RSA_PKCS1_SHA256, &rsa_sig));
        assert!(!verify_raw(&rsa_cert, message, SignatureScheme::RSA_PSS_SHA256, &rsa_sig));
        let (ec_cert, ec_sig) = (hex("308201633082010aa003020102021419c49aec7b878805b8e095c9bb6e05a23e4c0901300a06082a8648ce3d04030230323114301206035504030c0b545052492d444556494345310d300b060355040a0c0454505249310b3009060355040613025553301e170d3236313030333233303635365a170d3336303933303233303635365a30323114301206035504030c0b545052492d444556494345310d300b060355040a0c0454505249310b30090603550406130255533059301306072a8648ce3d020106082a8648ce3d03010703420004956457ce3b2adbf2219a4af5ee404aafec345911a1427424c669263c398aed240bc03ddad9bffe844b6c7940d7bfde8f46a59d6ba5e8fbbeb0320b75e93073fc300a06082a8648ce3d040302034700304402202ef852b6948162c52cdb3662a8e459111260e2efb3fdb40c672adcd1069c234802201c27018d729acefe6278f7f04a4c9b7a6deaeea6e9315d0055567085006c3f91"), hex("3046022100a8e2f982298448d9b6b5662a4ef72584430d5b818dfe7148b475597129077d10022100cd1e1e318785c636eb928926ead5feb54150f959d89b03ec9f7c854fbd58c3e6"));
        assert!(verify_raw(&ec_cert, message, SignatureScheme::ECDSA_NISTP256_SHA256, &ec_sig));
        assert!(!verify_raw(&ec_cert, message, SignatureScheme::RSA_PKCS1_SHA256, &ec_sig));
    }
}
