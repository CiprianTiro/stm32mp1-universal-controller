/*
 * tls.rs -- the hub's own TLS identity (issue #35): the key and certificate
 * the LAN WebSocket (wss://<hub>:8443) is encrypted with.
 *
 * WHY A SELF-MADE CERTIFICATE. Websites prove who they are with a
 * certificate from a public authority (Let's Encrypt, ...), which checks
 * that you own the domain. A hub on a home network has no domain, and must
 * work without internet. So the hub makes its own certificate on first
 * start, and clients check it differently: by its FINGERPRINT (the SHA-256
 * of the certificate), which they learn once, at pairing -- from the QR
 * code on the hub's own screen, i.e. from a source an attacker on the
 * network can't fake. From then on they accept this exact certificate and
 * no other ("certificate pinning"). This is also why the validity dates
 * don't matter here: rcgen's defaults (1975-4096) are kept, because the
 * certificate is never checked against a clock or an authority, only
 * against its pinned fingerprint.
 *
 * The files live on the userfs partition next to the MQTT identity, so a
 * reflash (#56) keeps them -- a new certificate would make every paired
 * client refuse the hub (its fingerprint changed), and they'd all have to
 * pair again:
 *
 *   /usr/local/etc/universal-controller/tls/   (mode 700)
 *   ├── hub.key   the private key (mode 600: whoever has it can pose as the hub)
 *   └── hub.crt   the certificate (public)
 */
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/* Where the key and certificate live unless overridden with HUB_TLS_DIR
 * (e.g. when running the daemon on the PC). */
const DEFAULT_TLS_DIR: &str = "/usr/local/etc/universal-controller/tls";

pub fn tls_dir() -> PathBuf {
    std::env::var_os("HUB_TLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TLS_DIR))
}

/* Everything the WebSocket server needs. */
pub struct Identity {
    /* Ready-to-use TLS settings for tokio-rustls's acceptor. */
    pub config: Arc<rustls::ServerConfig>,
    /* SHA-256 of the certificate, 64 lowercase hex digits: what clients
     * pin (and what the QR code carries). */
    pub fingerprint: String,
}

/* Loads the identity from `dir`, creating it first if it isn't there. */
pub fn load_or_create(dir: &Path) -> Result<Identity, String> {
    let key_path = dir.join("hub.key");
    let cert_path = dir.join("hub.crt");
    if !key_path.exists() || !cert_path.exists() {
        create(dir, &key_path, &cert_path)?;
    }
    let key_pem = fs::read(&key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
    let cert_pem = fs::read(&cert_path).map_err(|e| format!("{}: {e}", cert_path.display()))?;

    /* PEM ("-----BEGIN ...-----" text) to the binary DER form rustls
     * uses, with rustls's own reader (pki_types::pem). */
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| format!("{}: {e}", key_path.display()))?;
    let cert = CertificateDer::from_pem_slice(&cert_pem).map_err(|e| format!("{}: {e}", cert_path.display()))?;

    let fingerprint = hex(ring::digest::digest(&ring::digest::SHA256, cert.as_ref()).as_ref());

    /* No client certificates: clients prove who they are with their token,
     * inside the encrypted connection (auth.rs). with_single_cert also
     * checks that the key belongs to the certificate. */
    /* ring as the crypto provider, named explicitly (rather than relying
     * on it being the only one compiled in). */
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS settings: {e}"))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| format!("TLS key/certificate unusable: {e}"))?;
    /* WebSocket starts as HTTP/1.1 (the "upgrade" handshake). */
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Identity {
        config: Arc::new(config),
        fingerprint,
    })
}

/* Makes a new key pair and a self-signed certificate for it. The key is
 * ECDSA P-256: small, fast on the Cortex-A7, and supported by every TLS
 * client (phones, Python, browsers). */
fn create(dir: &Path, key_path: &Path, cert_path: &Path) -> Result<(), String> {
    let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("could not create a TLS key: {e}"))?;
    /* The names the certificate is "for". Clients pin the fingerprint and
     * don't rely on them, but they make the certificate recognisable
     * (e.g. in a browser's certificate viewer). */
    let host = fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "universal-controller".into());
    let mut params = rcgen::CertificateParams::new(vec![format!("{host}.local"), "localhost".into()])
        .map_err(|e| format!("could not describe the certificate: {e}"))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, format!("Universal Controller hub {host}"));
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("could not create the certificate: {e}"))?;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    write_file(key_path, key_pair.serialize_pem().as_bytes(), 0o600)?;
    write_file(cert_path, cert.pem().as_bytes(), 0o644)?;
    println!("tls: created a new hub certificate in {}", dir.display());
    Ok(())
}

/* Written to a .tmp file, fsynced, then renamed into place: a power cut
 * never leaves a half-written key (same idea as store.rs). */
fn write_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(path.parent().unwrap_or(Path::new("/")))?.sync_all()
    };
    write().map_err(|e| format!("{}: {e}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/* The first 16 hex digits in groups of 4 ("3f2a 91c0 7be4 d012"), for
 * showing on the screen next to the QR code, so a user CAN compare it by
 * eye with what their app shows. */
pub fn short_fingerprint(fingerprint: &str) -> String {
    fingerprint
        .as_bytes()
        .chunks(4)
        .take(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uc-tls-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn created_once_then_reused() {
        let dir = test_dir("reuse");
        let first = load_or_create(&dir).unwrap();
        assert_eq!(first.fingerprint.len(), 64);
        /* The key is root-only; the directory too. */
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode(&dir.join("hub.key")), 0o600);
        assert_eq!(mode(&dir), 0o700);
        /* A second start loads the same identity: same fingerprint. */
        let second = load_or_create(&dir).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn short_fingerprint_groups() {
        assert_eq!(short_fingerprint("3f2a91c07be4d012ffff"), "3f2a 91c0 7be4 d012");
    }
}
