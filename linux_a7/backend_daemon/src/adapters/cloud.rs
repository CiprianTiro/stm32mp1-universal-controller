/*
 * cloud.rs -- talking to a vendor's cloud over HTTPS (issue #74).
 *
 * Some devices need their maker's account: the cloud gives the key the
 * hub then uses locally (Roborock, "P4"), or it's the only way to the
 * device at all (EZVIZ plugs, "P5"). This is the one HTTPS client those
 * adapters share:
 *
 *   https_json   one request, a JSON reply; form fields or a query string
 *                and extra headers as the vendor wants them
 *
 * TRUST. Unlike the devices on the LAN (whose self-signed certificates
 * the hub pins at pairing, net.rs), a vendor's cloud has a certificate
 * from a public certificate authority, and is checked like a browser
 * checks a website: against the system's list of trusted authorities
 * (the image's ca-certificates package, CA_BUNDLE), and for the right
 * name. Never "accept anything": a vendor login sends a password.
 *
 * Every request has a timeout and a size limit, like net.rs: a cloud that
 * hangs or answers something huge must never block or exhaust the hub.
 */
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::header::{CONTENT_TYPE, HOST, USER_AGENT};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

use super::net::NetError;
use crate::templates::ErrorKind;

/* The trusted authorities (Debian/Yocto layout; overridable for tests and
 * the PC with HUB_CA_BUNDLE). */
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
/* A cloud answers in well under a second; a slow mobile uplink (#55) and
 * a busy vendor can take several. */
pub const CLOUD_TIMEOUT: Duration = Duration::from_secs(20);
/* A Roborock "home" with a few devices is ~20 KB. */
const MAX_REPLY: usize = 2 * 1024 * 1024;

/* The TLS settings, built once: reading and parsing ~140 certificates
 * takes a moment on the A7. None if the bundle is missing. */
pub fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let path = std::env::var("HUB_CA_BUNDLE").unwrap_or_else(|_| CA_BUNDLE.into());
            let pem = std::fs::read(&path).map_err(|e| format!("no trusted certificate list ({path}: {e})"))?;
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(&pem).flatten() {
                /* One odd certificate in the system list isn't a reason to
                 * trust nothing. */
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err(format!("{path} holds no certificates"));
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("TLS setup: {e}"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
}

/* What to send. `query` goes into the URL, `form` into the body
 * (application/x-www-form-urlencoded), `json` into the body as JSON. */
#[derive(Default)]
pub struct CloudRequest<'a> {
    pub query: &'a [(&'a str, String)],
    pub form: Option<&'a [(&'a str, String)]>,
    pub json: Option<&'a Value>,
    pub headers: &'a [(&'a str, String)],
}

/* One HTTPS request to `url` ("https://euiot.roborock.com/api/v1/..."),
 * its reply as JSON. Any HTTP status is returned as JSON if it is JSON
 * (vendors put their error codes there); a reply that isn't JSON is an
 * error. */
pub async fn https_json(method: Method, url: &str, request: CloudRequest<'_>) -> Result<Value, NetError> {
    match tokio::time::timeout(CLOUD_TIMEOUT, https_inner(method, url, request)).await {
        Ok(result) => result,
        Err(_) => Err(NetError::new(
            ErrorKind::Timeout,
            format!("{} isn't answering (no reply within {} s)", host_of(url), CLOUD_TIMEOUT.as_secs()),
        )),
    }
}

fn host_of(url: &str) -> &str {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

async fn https_inner(method: Method, url: &str, request: CloudRequest<'_>) -> Result<Value, NetError> {
    let unreachable = |e: String| NetError::new(ErrorKind::Unreachable, e);
    let unsupported = |e: String| NetError::new(ErrorKind::Unsupported, e);
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| unsupported(format!("{url}: only https:// is allowed for a cloud")))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut path = path.to_string();
    if !request.query.is_empty() {
        path.push(if path.contains('?') { '&' } else { '?' });
        path.push_str(&form_encode(request.query));
    }

    let config = tls_config().map_err(unreachable)?;
    let tcp = TcpStream::connect((host, 443))
        .await
        .map_err(|e| unreachable(format!("can't reach {host}: {e} (is the hub online?)")))?;
    let name = ServerName::try_from(host.to_string()).map_err(|e| unsupported(format!("{host}: {e}")))?;
    let tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, tcp)
        .await
        .map_err(|e| unreachable(format!("{host}: secure connection failed: {e}")))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|e| unreachable(format!("{host}: {e}")))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let mut builder = Request::builder()
        .method(method)
        .uri(&path)
        .header(HOST, host)
        .header(USER_AGENT, "universal-controller-hub");
    let body = if let Some(form) = request.form {
        builder = builder.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
        Bytes::from(form_encode(form))
    } else if let Some(json) = request.json {
        builder = builder.header(CONTENT_TYPE, "application/json");
        Bytes::from(json.to_string())
    } else {
        Bytes::new()
    };
    for (name, value) in request.headers {
        builder = builder.header(*name, value);
    }
    let http = builder
        .body(Full::new(body))
        .map_err(|e| unsupported(format!("invalid request for {host}: {e}")))?;
    let reply = sender
        .send_request(http)
        .await
        .map_err(|e| unreachable(format!("{host} broke off the reply: {e}")))?;
    let status = reply.status();
    let bytes = Limited::new(reply.into_body(), MAX_REPLY)
        .collect()
        .await
        .map_err(|e| unsupported(format!("{host}: bad reply: {e}")))?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|_| unsupported(format!("{host} answered HTTP {status}, not JSON")))
}

/* name=value&name=value, each part percent-encoded (RFC 3986 unreserved
 * characters stay as they are). */
pub fn form_encode(fields: &[(&str, String)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", percent(k), percent(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_are_encoded() {
        let fields = [("email", "a.b+c@x.com".to_string()), ("type", "login".to_string())];
        assert_eq!(form_encode(&fields), "email=a.b%2Bc%40x.com&type=login");
    }

    #[test]
    fn hosts_are_taken_from_urls() {
        assert_eq!(host_of("https://euiot.roborock.com/api/v1/x?y=1"), "euiot.roborock.com");
        assert_eq!(host_of("https://api.example.com"), "api.example.com");
    }
}
