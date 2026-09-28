/*
 * discovery.rs -- finding devices on the LAN (issue #40).
 *
 * WHAT TO LOOK FOR comes from the templates: each one lists how devices of
 * its type announce themselves ("discovery" in templates.rs). This file
 * runs those searches for ALL templates, in the background, and keeps what
 * it found:
 *   - mDNS (DNS-SD): a browse per service type ("_wled._tcp"), running
 *     all the time -- devices answer and announce themselves;
 *   - SSDP: an M-SEARCH per search target every PASS, or when a client
 *     asks (discover_now) -- the TV answers.
 * (UDP broadcast, WS-Discovery and the rest come with their first device.)
 *
 * WHAT IT'S FOR:
 *   - the "Found on your network" INBOX: devices matching a template that
 *     aren't added yet -- adding one is a tap, the address pre-filled;
 *   - IP AUTO-UPDATE: an added device seen at a NEW address (same
 *     identity: MAC, UUID) simply gets its address updated, and its
 *     adapter restarted -- no "device lost" after a router restart.
 *
 * Gentle on the network: mDNS is mostly listening; SSDP sends one small
 * packet per template every PASS (5 min). Nothing here scans addresses.
 *
 * FIREWALL: SSDP answers arrive as unicast to our sending port, which the
 * default-deny firewall (#37) only lets in because this file always sends
 * from SSDP_PORT, opened in hub-firewall.nft for exactly that. mDNS uses
 * 5353, open already.
 */
use serde::Serialize;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch, Notify};

use crate::control::Control;
use crate::templates::{self, Template, Templates};

/* The port SSDP searches are sent FROM -- and answers come back TO.
 * Opened in hub-firewall.nft (keep them equal). */
pub const SSDP_PORT: u16 = 50190;
const SSDP_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);
/* How long answers are collected after a search. */
const SSDP_WAIT: Duration = Duration::from_secs(3);
/* A search round every PASS (and on demand). */
const PASS: Duration = Duration::from_secs(300);
/* Not seen for this long: dropped from the inbox (switched off, gone). */
const EXPIRE: Duration = Duration::from_secs(15 * 60);

/* One device found, as the inbox shows it. */
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Found {
    /* The template it matches. */
    pub template: String,
    /* What it calls itself ("WLED-Kitchen"), or the template's name. */
    pub name: String,
    pub address: IpAddr,
    /* The template's "fill": pre-filled inputs ("host") and extra values
     * ("mac", "uuid"). */
    pub values: BTreeMap<String, String>,
    /* From the template's "identity"; empty if it has none. */
    pub identity: String,
}

struct Entry {
    found: Found,
    last_seen: Instant,
}

pub struct Discovery {
    /* Keyed by template + identity (or address, without an identity). */
    entries: Mutex<BTreeMap<String, Entry>>,
    /* Bumped whenever the list may have changed: ws.rs tells subscribed
     * clients (found_changed), who then list again. */
    changed: watch::Sender<u64>,
    /* discover_now wakes the search loop. */
    wake: Notify,
}

impl Discovery {
    pub fn new() -> Self {
        Discovery {
            entries: Mutex::new(BTreeMap::new()),
            changed: watch::Sender::new(0),
            wake: Notify::new(),
        }
    }

    /* A search round now (the wizard's discover step, "Search again"). */
    pub fn discover_now(&self) {
        self.wake.notify_one();
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /* The inbox: found devices not added yet. A found device counts as
     * added if a device of the same template has its identity -- or, for
     * templates without one, its address. */
    pub async fn inbox(&self, control: &Control) -> Vec<Found> {
        let devices = control.list().await.unwrap_or_default();
        let found: Vec<Found> = self.entries.lock().unwrap().values().map(|e| e.found.clone()).collect();
        found
            .into_iter()
            .filter(|f| {
                !devices.iter().any(|d| {
                    d.template == f.template
                        && if f.identity.is_empty() {
                            d.config.get("host").is_some_and(|h| *h == f.address.to_string())
                        } else {
                            d.identity == f.identity
                        }
                })
            })
            .collect()
    }

    /* Remembers a found device; true if it's new or changed. (Crate-wide
     * for the wizard's tests, which play "found on the network".) */
    pub(crate) fn record(&self, found: Found) -> bool {
        let key = format!(
            "{}/{}",
            found.template,
            if found.identity.is_empty() { found.address.to_string() } else { found.identity.clone() }
        );
        let mut entries = self.entries.lock().unwrap();
        let changed = entries.get(&key).is_none_or(|e| e.found != found);
        entries.insert(key, Entry { found, last_seen: Instant::now() });
        changed
    }

    fn expire(&self) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|_, e| e.last_seen.elapsed() < EXPIRE);
        entries.len() != before
    }

    fn announce(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/* What a discovery method saw, before a template turns it into a Found:
 * {address}, {port}, {name}, {uuid}, {txt.<key>}, {header.<Name>}. */
type Vars = BTreeMap<String, String>;

/* Fills a template text's {placeholders} from `vars`. An unknown one
 * becomes "" (a device that doesn't send some value). Also the wizard's,
 * for the template's identity. */
pub fn fill(text: &str, vars: &Vars) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        let value = vars.get(name).or_else(|| {
            /* HTTP header names aren't case-sensitive (SSDP answers are
             * HTTP-like). */
            name.strip_prefix("header.").and_then(|h| {
                vars.iter()
                    .find(|(k, _)| k.strip_prefix("header.").is_some_and(|k| k.eq_ignore_ascii_case(h)))
                    .map(|(_, v)| v)
            })
        });
        out.push_str(value.map_or("", String::as_str));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/* A template's view of what was seen: its fill, identity and name. None
 * if there's no usable address. */
fn to_found(template: &Template, fill_map: &BTreeMap<String, String>, vars: &Vars) -> Option<Found> {
    let address: IpAddr = vars.get("address")?.parse().ok()?;
    let values: BTreeMap<String, String> = fill_map
        .iter()
        .map(|(target, source)| (target.clone(), fill(source, vars)))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    let identity = fill(&template.identity, &values);
    let name = values
        .get("name")
        .cloned()
        .unwrap_or_else(|| template.name.clone());
    Some(Found {
        template: template.id.clone(),
        name,
        address,
        values,
        identity,
    })
}

/* Runs for the daemon's lifetime (spawned from main). */
pub async fn run(discovery: Arc<Discovery>, templates: Arc<Templates>, control: Control) {
    /* Every method's sightings arrive here, from the mDNS readers and the
     * SSDP rounds, and are handled in one place. */
    let (seen_tx, mut seen_rx) = mpsc::channel::<(String, Vars)>(64);

    /* mDNS: one browse per service type any template asks for. */
    let mdns_types: Vec<String> = templates
        .all()
        .flat_map(|t| t.discovery.iter())
        .filter_map(|d| match d {
            templates::Discovery::Mdns { service, .. } => Some(service.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !mdns_types.is_empty() {
        match mdns_sd::ServiceDaemon::new() {
            Ok(daemon) => {
                for service in mdns_types {
                    spawn_mdns_browse(&daemon, service, seen_tx.clone());
                }
                /* The daemon runs on its own thread; keep it alive. */
                std::mem::forget(daemon);
            }
            Err(e) => println!("discovery: mDNS unavailable: {e}"),
        }
    }

    /* SSDP search targets, and the rounds. */
    let ssdp_targets: Vec<String> = templates
        .all()
        .flat_map(|t| t.discovery.iter())
        .filter_map(|d| match d {
            templates::Discovery::Ssdp { search, .. } => Some(search.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    {
        let discovery = discovery.clone();
        let seen_tx = seen_tx.clone();
        tokio::spawn(async move {
            loop {
                if !ssdp_targets.is_empty() {
                    if let Err(e) = ssdp_round(&ssdp_targets, &seen_tx).await {
                        println!("discovery: SSDP search failed: {e}");
                    }
                }
                if discovery.expire() {
                    discovery.announce();
                }
                tokio::select! {
                    _ = tokio::time::sleep(PASS) => {}
                    _ = discovery.wake.notified() => {}
                }
            }
        });
    }
    drop(seen_tx);

    /* Sightings -> Found, per matching template. */
    while let Some((what, vars)) = seen_rx.recv().await {
        let mut any_new = false;
        for template in templates.all() {
            for method in &template.discovery {
                let matches = match method {
                    templates::Discovery::Mdns { service, .. } => what == format!("mdns:{service}"),
                    templates::Discovery::Ssdp { search, .. } => what == format!("ssdp:{search}"),
                    _ => false,
                };
                if !matches {
                    continue;
                }
                let Some(found) = to_found(template, method.fill(), &vars) else {
                    continue;
                };
                follow_moved_device(&found, &control).await;
                any_new |= discovery.record(found);
            }
        }
        if any_new {
            discovery.announce();
        }
    }
}

/* IP auto-update: an added device with this identity at another address. */
async fn follow_moved_device(found: &Found, control: &Control) {
    if found.identity.is_empty() {
        return;
    }
    let Some(host) = found.values.get("host") else { return };
    let Ok(devices) = control.list().await else { return };
    for device in devices {
        if device.template == found.template && device.identity == found.identity && device.config.get("host") != Some(host) {
            println!(
                "discovery: {} moved from {} to {host}, following it",
                device.id,
                device.config.get("host").map_or("?", String::as_str)
            );
            let _ = control.set_config(&device.id, [("host".to_string(), host.clone())].into()).await;
        }
    }
}

/* One mDNS browse, read by its own task. `what` = "mdns:<service>". */
fn spawn_mdns_browse(daemon: &mdns_sd::ServiceDaemon, service: String, seen_tx: mpsc::Sender<(String, Vars)>) {
    let receiver = match daemon.browse(&format!("{service}.local.")) {
        Ok(r) => r,
        Err(e) => {
            println!("discovery: can't browse {service}: {e}");
            return;
        }
    };
    tokio::spawn(async move {
        while let Ok(event) = receiver.recv_async().await {
            let mdns_sd::ServiceEvent::ServiceResolved(info) = event else { continue };
            let Some(address) = info.get_addresses_v4().into_iter().next() else { continue };
            let mut vars = Vars::new();
            vars.insert("address".into(), address.to_string());
            vars.insert("port".into(), info.get_port().to_string());
            /* "WLED-Kitchen._wled._tcp.local." -> "WLED-Kitchen". */
            let name = info.get_fullname().split(&format!(".{service}")).next().unwrap_or_default();
            vars.insert("name".into(), name.to_string());
            for property in info.get_properties().iter() {
                vars.insert(format!("txt.{}", property.key()), property.val_str().to_string());
            }
            if seen_tx.send((format!("mdns:{service}"), vars)).await.is_err() {
                return;
            }
        }
    });
}

/* One SSDP round: an M-SEARCH per target, answers collected SSDP_WAIT. */
async fn ssdp_round(targets: &[String], seen_tx: &mpsc::Sender<(String, Vars)>) -> std::io::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", SSDP_PORT)).await?;
    for target in targets {
        let search = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {target}\r\n\r\n"
        );
        socket.send_to(search.as_bytes(), SSDP_ADDR).await?;
    }
    let deadline = tokio::time::Instant::now() + SSDP_WAIT;
    let mut buf = [0u8; 2048];
    while let Ok(Ok((len, from))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
        let Some(vars) = parse_ssdp(&buf[..len], from.ip()) else { continue };
        /* Which search it answers: its ST header. */
        let Some(st) = vars.get("header.ST").cloned() else { continue };
        if targets.contains(&st) && seen_tx.send((format!("ssdp:{st}"), vars)).await.is_err() {
            break;
        }
    }
    Ok(())
}

/* An SSDP answer: "HTTP/1.1 200 OK", then "Name: value" lines. Header
 * values are %-decoded (LG sends its name as "%5BLG%5D%20webOS%20TV").
 * {uuid} comes from USN ("uuid:<uuid>::urn:..."). */
fn parse_ssdp(packet: &[u8], from: IpAddr) -> Option<Vars> {
    let text = std::str::from_utf8(packet).ok()?;
    let mut lines = text.split("\r\n");
    if !lines.next()?.starts_with("HTTP/1.1 200") {
        return None;
    }
    let mut vars = Vars::new();
    vars.insert("address".into(), from.to_string());
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim();
        /* Normalised to upper case for the headers every answer has; the
         * others keep their spelling (fill() compares them ignoring case). */
        let key = match name.to_ascii_uppercase().as_str() {
            n @ ("ST" | "USN" | "LOCATION" | "SERVER") => n.to_string(),
            _ => name.to_string(),
        };
        vars.insert(format!("header.{key}"), percent_decode(value.trim()));
    }
    if let Some(uuid) = vars
        .get("header.USN")
        .and_then(|usn| usn.strip_prefix("uuid:"))
        .map(|u| u.split("::").next().unwrap_or(u).to_string())
    {
        vars.insert("uuid".into(), uuid);
    }
    Some(vars)
}

fn percent_decode(text: &str) -> String {
    /* Byte by byte: "%" + two hex digits -> that byte. Anything else,
     * including a "%" not followed by two hex digits, stays as it is.
     * (Working on bytes, not &str slices: slicing a string in the middle
     * of a multi-byte character would panic.) */
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LG_ANSWER: &str = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nDATE: Fri, 25 Sep 2026 12:00:00 GMT\r\nEXT:\r\nLOCATION: http://192.168.1.40:1981/\r\nSERVER: WebOS/4.1.0 UPnP/1.0\r\nST: urn:lge-com:service:webos-second-screen:1\r\nUSN: uuid:0e7c3b1a-11aa-4d44-b2f3-7f0e1c2d3e4f::urn:lge-com:service:webos-second-screen:1\r\nDLNADeviceName.lge.com: %5BLG%5D%20webOS%20TV\r\n\r\n";

    fn tv_template() -> Template {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/lg-webos-tv.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn an_lg_answer_becomes_a_found_tv() {
        let vars = parse_ssdp(LG_ANSWER.as_bytes(), "192.168.1.40".parse().unwrap()).unwrap();
        assert_eq!(vars["uuid"], "0e7c3b1a-11aa-4d44-b2f3-7f0e1c2d3e4f");
        assert_eq!(vars["header.ST"], "urn:lge-com:service:webos-second-screen:1");
        let t = tv_template();
        let found = to_found(&t, t.discovery[0].fill(), &vars).unwrap();
        assert_eq!(found.name, "[LG] webOS TV");
        assert_eq!(found.values["host"], "192.168.1.40");
        assert_eq!(found.identity, "0e7c3b1a-11aa-4d44-b2f3-7f0e1c2d3e4f");
    }

    #[test]
    fn fill_handles_missing_and_case() {
        let vars: Vars = [("address".into(), "10.0.0.2".into()), ("header.Server".into(), "X".into())].into();
        assert_eq!(fill("{address}:{port}", &vars), "10.0.0.2:");
        assert_eq!(fill("{header.SERVER}", &vars), "X");
        assert_eq!(fill("no placeholders", &vars), "no placeholders");
    }

    #[test]
    fn not_an_answer_is_ignored() {
        assert!(parse_ssdp(b"NOTIFY * HTTP/1.1\r\n\r\n", "10.0.0.2".parse().unwrap()).is_none());
        assert!(parse_ssdp(&[0xff, 0xfe], "10.0.0.2".parse().unwrap()).is_none());
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("%5BLG%5D%20TV"), "[LG] TV");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%\u{e9}t\u{e9}"), "%\u{e9}t\u{e9}");
        assert_eq!(percent_decode("ends with %4"), "ends with %4");
    }

    /* Against the REAL network: finds WLED devices (mDNS) and LG TVs
     * (SSDP) on the LAN of the machine running it. Not part of the normal
     * test run (it needs those devices and takes a few seconds):
     *     cargo test live_discovery -- --ignored --nocapture */
    #[tokio::test]
    #[ignore]
    async fn live_discovery() {
        let (tx, mut rx) = mpsc::channel(64);
        let daemon = mdns_sd::ServiceDaemon::new().unwrap();
        spawn_mdns_browse(&daemon, "_wled._tcp".into(), tx.clone());
        ssdp_round(&["urn:lge-com:service:webos-second-screen:1".into()], &tx).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while let Ok(Some((what, vars))) = tokio::time::timeout_at(deadline, rx.recv()).await {
            println!("{what}: {vars:?}");
        }
    }

    #[test]
    fn record_reports_only_changes_and_expires() {
        let d = Discovery::new();
        let found = Found {
            template: "wled".into(),
            name: "Strip".into(),
            address: "10.0.0.5".parse().unwrap(),
            values: BTreeMap::new(),
            identity: "aabbcc".into(),
        };
        assert!(d.record(found.clone()));
        assert!(!d.record(found.clone()));
        let moved = Found { address: "10.0.0.6".parse().unwrap(), ..found };
        assert!(d.record(moved));
        /* Same identity: still one entry. */
        assert_eq!(d.entries.lock().unwrap().len(), 1);
        assert!(!d.expire());
    }
}
