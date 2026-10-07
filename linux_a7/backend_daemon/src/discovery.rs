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
 *     asks (discover_now) -- the TV answers;
 *   - UDP broadcast (issue #75): a template's probe ("getPilot" for WiZ
 *     bulbs) sent to the broadcast address of every network the hub is
 *     on, with the SSDP rounds -- every device of that kind answers;
 *   - UDP multicast (issue #75): listening on a group devices announce
 *     themselves to (Yeelight), all the time;
 *   - UDP listen (issue #74): devices announcing themselves to the whole
 *     network in their vendor's packing (Roborock vacuums, UDP 58866,
 *     every few seconds) -- the hub just listens and decodes;
 *   - network scan and port probe (issue #73, netscan.rs): devices that
 *     announce nothing, found by their MAC address's maker and the ports
 *     they answer on -- ONLY when someone asks (discover_now: the
 *     wizard's search), never on the timer.
 * (WS-Discovery and the rest come with their first device.)
 *
 * A template's "match" picks ITS devices among what a method finds: every
 * Shelly answers mDNS "_shelly._tcp", the plug template only takes the
 * ones whose TXT says app=PlugSG3.
 *
 * WHAT IT'S FOR:
 *   - the "Found on your network" INBOX: devices matching a template that
 *     aren't added yet -- adding one is a tap, the address pre-filled;
 *   - IP AUTO-UPDATE: an added device seen at a NEW address (same
 *     identity: MAC, UUID) simply gets its address updated, and its
 *     adapter restarted -- no "device lost" after a router restart;
 *   - RE-FIND BY MAC (issue #73, refind below): an added device that went
 *     offline and announces nothing is looked for by its MAC address in
 *     the kernel's neighbour table, and, at most every REFIND_GAP, by a
 *     network sweep.
 *
 * Gentle on the network: mDNS and multicast are listening; SSDP and UDP
 * broadcast send one small packet per template every PASS (5 min). Only
 * netscan.rs sends to every address, bounded and paced as described
 * there, and only when asked or re-finding.
 *
 * FIREWALL: SSDP and UDP broadcast answers arrive as unicast to our
 * sending port, which the default-deny firewall (#37) only lets in
 * because this file always sends from SSDP_PORT / UDP_PORT, opened in
 * hub-firewall.nft for exactly that. (A broadcast's answer comes from the
 * DEVICE's address, not the broadcast address it was sent to, so the
 * firewall can't match it to the request as it does for normal replies.)
 * mDNS uses 5353, open already; a template's multicast port must be
 * opened there too.
 */
use serde::Serialize;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch, Notify};

use crate::control::Control;
use crate::device::Health;
use crate::netscan::{self, Host, Scanner};
use crate::network;
use crate::templates::{self, Template, Templates};

/* The port SSDP searches are sent FROM -- and answers come back TO.
 * Opened in hub-firewall.nft (keep them equal). */
pub const SSDP_PORT: u16 = 50190;
const SSDP_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);
/* The port UDP broadcast probes are sent FROM, answers come back TO.
 * Opened in hub-firewall.nft (keep them equal). */
pub const UDP_PORT: u16 = 50191;
/* How long answers are collected after a search. */
const SSDP_WAIT: Duration = Duration::from_secs(3);
/* A search round every PASS (and on demand). */
const PASS: Duration = Duration::from_secs(300);
/* Not seen for this long: dropped from the inbox (switched off, gone). */
const EXPIRE: Duration = Duration::from_secs(15 * 60);
/* Re-finding an offline device sweeps the network at most this often
 * (issue #73): a device that is simply switched off would otherwise make
 * the hub sweep every PASS, forever. 5 min: a sweep is only 253 small
 * packets, and 30 min left a camera that got a new address (DHCP)
 * unreachable for up to half an hour (2026-10-06). */
const REFIND_GAP: Duration = Duration::from_secs(5 * 60);

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
    /* Issue #73: found by a network scan or port probe only. The device's
     * own announcement (mDNS, SSDP, ...) says more -- its real name -- and
     * replaces it; the other way round, a probe only confirms the device
     * is still there. Without this the two took turns, each search. */
    probed: bool,
}

pub struct Discovery {
    /* Issue #72: devices announcing themselves (broker.rs: a device
     * knocking on the MQTT broker without a login). Taken by run(). */
    announcements: Mutex<Option<mpsc::Receiver<Vars>>>,
    announcer: mpsc::Sender<Vars>,
    /* Keyed by template + identity (or address, without an identity). */
    entries: Mutex<BTreeMap<String, Entry>>,
    /* Bumped whenever the list may have changed: ws.rs tells subscribed
     * clients (found_changed), who then list again. */
    changed: watch::Sender<u64>,
    /* discover_now wakes the search loop. */
    wake: Notify,
    /* Issue #73: network sweeps, rate-limited (netscan.rs). */
    scanner: Scanner,
    /* When re-finding last swept (REFIND_GAP). */
    last_refind_sweep: Mutex<Option<Instant>>,
}

impl Discovery {
    pub fn new() -> Self {
        let (announcer, announcements) = mpsc::channel(32);
        Discovery {
            announcements: Mutex::new(Some(announcements)),
            announcer,
            entries: Mutex::new(BTreeMap::new()),
            changed: watch::Sender::new(0),
            wake: Notify::new(),
            scanner: Scanner::new(),
            last_refind_sweep: Mutex::new(None),
        }
    }

    /* Where announcements go (see `announcements`): {address},
     * {client_id}, {client_kind}. */
    pub fn announcer(&self) -> mpsc::Sender<Vars> {
        self.announcer.clone()
    }

    /* A search round now (the wizard's discover step, "Search again"). */
    pub fn discover_now(&self) {
        self.wake.notify_one();
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /* Issue #74: a device seen on the network with this template and
     * identity -- added or not (the wizard: the address of a device picked
     * from a vendor account). */
    pub fn lookup(&self, template: &str, identity: &str) -> Option<Found> {
        if identity.is_empty() {
            return None;
        }
        self.entries
            .lock()
            .unwrap()
            .values()
            .find(|e| e.found.template == template && e.found.identity == identity)
            .map(|e| e.found.clone())
    }

    /* Issue #73: who is on the network, with makers, as the kernel knows
     * it right now (no packet sent; a search refreshes it). For a person
     * looking for a device by hand ("which one is the camera?"). */
    pub fn hosts(&self) -> Vec<Host> {
        let mut hosts = netscan::neighbours();
        hosts.sort_by_key(|h| h.address);
        hosts.dedup_by_key(|h| h.address);
        hosts
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
    #[cfg(test)]
    pub(crate) fn record(&self, found: Found) -> bool {
        self.record_from(found, false)
    }

    /* record, saying whether the sighting came from a scan or probe. */
    fn record_from(&self, found: Found, probed: bool) -> bool {
        let key = format!(
            "{}/{}",
            found.template,
            if found.identity.is_empty() { found.address.to_string() } else { found.identity.clone() }
        );
        let mut entries = self.entries.lock().unwrap();
        /* One device found two ways (issue #73): the TV by SSDP, with its
         * UUID, and by a port probe, without. Keep the one WITH an
         * identity -- it survives an address change. */
        let same_device = |e: &Entry| e.found.template == found.template && e.found.address == found.address;
        if found.identity.is_empty() {
            if entries.values().any(|e| same_device(e) && !e.found.identity.is_empty()) {
                return false;
            }
        } else {
            entries.retain(|_, e| !(same_device(e) && e.found.identity.is_empty()));
        }
        if let Some(entry) = entries.get_mut(&key) {
            if probed && !entry.probed {
                entry.last_seen = Instant::now();
                return false;
            }
        }
        let changed = entries.get(&key).is_none_or(|e| e.found != found);
        entries.insert(
            key,
            Entry {
                found,
                last_seen: Instant::now(),
                probed,
            },
        );
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
pub type Vars = BTreeMap<String, String>;

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
    /* The daemon runs on its own thread. The browses themselves are
     * (re)started by every search round, below. */
    let mdns = if mdns_types.is_empty() {
        None
    } else {
        match mdns_sd::ServiceDaemon::new() {
            Ok(daemon) => Some(daemon),
            Err(e) => {
                println!("discovery: mDNS unavailable: {e}");
                None
            }
        }
    };

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
    /* UDP broadcast probes: (port, what to send). */
    let udp_probes: Vec<(u16, String)> = templates
        .all()
        .flat_map(|t| t.discovery.iter())
        .filter_map(|d| match d {
            templates::Discovery::UdpBroadcast { port, probe, .. } => Some((*port, probe.clone())),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    /* UDP multicast: one listener per group and port. */
    let groups: std::collections::BTreeSet<(Ipv4Addr, u16)> = templates
        .all()
        .flat_map(|t| t.discovery.iter())
        .filter_map(|d| match d {
            templates::Discovery::UdpMulticast { group, port, .. } => Some((*group, *port)),
            _ => None,
        })
        .collect();
    for (group, port) in groups {
        spawn_multicast_listen(group, port, seen_tx.clone());
    }
    /* Issue #74: vendor broadcasts, one listener per port. */
    let listens: std::collections::BTreeSet<(u16, templates::UdpDecoder)> = templates
        .all()
        .flat_map(|t| t.discovery.iter())
        .filter_map(|d| match d {
            templates::Discovery::UdpListen { port, decode, .. } => Some((*port, *decode)),
            _ => None,
        })
        .collect();
    for (port, decoder) in listens {
        spawn_udp_listen(port, decoder, seen_tx.clone());
    }

    /* Issue #73: is any template looking for hosts by maker, and which
     * port probes are there? A probe's makers: all the templates' lists
     * for it together (any, if one of them takes any). */
    let network_scan = templates
        .all()
        .flat_map(|t| t.discovery.iter())
        .any(|d| matches!(d, templates::Discovery::NetworkScan { .. }));
    let mut port_probes: BTreeMap<(u16, Option<String>), Option<Vec<String>>> = BTreeMap::new();
    for method in templates.all().flat_map(|t| t.discovery.iter()) {
        if let templates::Discovery::PortProbe {
            port, get, manufacturers, ..
        } = method
        {
            let makers = port_probes.entry((*port, get.clone())).or_insert_with(|| Some(Vec::new()));
            match makers {
                Some(list) if !manufacturers.is_empty() => list.extend(manufacturers.iter().cloned()),
                _ => *makers = None,
            }
        }
    }

    {
        let discovery = discovery.clone();
        let seen_tx = seen_tx.clone();
        let control = control.clone();
        tokio::spawn(async move {
            let mut browsing = false;
            /* Woken by discover_now (someone is searching), not the timer. */
            let mut asked = false;
            loop {
                /* mDNS: browse again every round. A device announces itself
                 * once (when it starts, or answering the first browse); a
                 * single browse never reports it again, so 15 min later it
                 * expired from the inbox for good -- seen with two WLEDs
                 * after their devices were removed (#95). A new browse
                 * reports what the daemon has cached at once, and asks the
                 * network again. */
                if let Some(daemon) = &mdns {
                    for service in &mdns_types {
                        if browsing {
                            let _ = daemon.stop_browse(&format!("{service}.local."));
                        }
                        spawn_mdns_browse(daemon, service.clone(), seen_tx.clone());
                    }
                    browsing = true;
                }
                if !ssdp_targets.is_empty() {
                    if let Err(e) = ssdp_round(&ssdp_targets, &seen_tx).await {
                        println!("discovery: SSDP search failed: {e}");
                    }
                }
                if !udp_probes.is_empty() {
                    if let Err(e) = udp_round(&udp_probes, &broadcast_targets(), UDP_PORT, &seen_tx).await {
                        println!("discovery: UDP broadcast failed: {e}");
                    }
                }
                if asked && (network_scan || !port_probes.is_empty()) {
                    scan_round(&discovery.scanner, network_scan, &port_probes, &seen_tx).await;
                }
                refind(&discovery, &control).await;
                if discovery.expire() {
                    discovery.announce();
                }
                asked = tokio::select! {
                    _ = tokio::time::sleep(PASS) => false,
                    _ = discovery.wake.notified() => true,
                };
            }
        });
    }
    /* Devices announcing themselves (issue #72): passed in as "announce". */
    if let Some(mut announcements) = discovery.announcements.lock().unwrap().take() {
        let seen_tx = seen_tx.clone();
        tokio::spawn(async move {
            while let Some(vars) = announcements.recv().await {
                if seen_tx.send(("announce".to_string(), vars)).await.is_err() {
                    return;
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
                    templates::Discovery::UdpBroadcast { port, .. } => what == format!("udp:{port}"),
                    templates::Discovery::UdpMulticast { group, port, .. } => what == format!("mcast:{group}:{port}"),
                    templates::Discovery::DeviceAnnounce { .. } => what == "announce",
                    templates::Discovery::NetworkScan { .. } => what == "scan",
                    templates::Discovery::UdpListen { port, .. } => what == format!("listen:{port}"),
                    templates::Discovery::PortProbe { port, get, .. } => what == probe_key(*port, get),
                    _ => false,
                };
                if !matches || !matches_template(method, &vars) {
                    continue;
                }
                let Some(found) = to_found(template, method.fill(), &vars) else {
                    continue;
                };
                follow_moved_device(&found, &control).await;
                let probed = what == "scan" || what.starts_with("probe:");
                any_new |= discovery.record_from(found, probed);
            }
        }
        if any_new {
            discovery.announce();
        }
    }
}

/* The template's "match": every condition holds for this sighting -- and,
 * for a network scan or port probe, the maker is one it wants. */
fn matches_template(method: &templates::Discovery, vars: &Vars) -> bool {
    maker_wanted(method.manufacturers(), vars.get("manufacturer").map_or("", String::as_str))
        && method.matches().iter().all(|(name, wanted)| fill(&format!("{{{name}}}"), vars) == *wanted)
}

/* Is `maker` in `wanted` (no case)? An empty list wants any maker. */
fn maker_wanted(wanted: &[String], maker: &str) -> bool {
    wanted.is_empty() || wanted.iter().any(|w| w.eq_ignore_ascii_case(maker))
}

/* What a port probe's sightings are sent as. */
fn probe_key(port: u16, get: &Option<String>) -> String {
    format!("probe:{port}:{}", get.as_deref().unwrap_or(""))
}

/* Issue #73: a network scan (a sweep, at most every MIN_SWEEP_GAP), then
 * the port probes on the hosts it found. Sightings: "scan" per host, and
 * probe_key per host a probe found. */
async fn scan_round(
    scanner: &Scanner,
    network_scan: bool,
    port_probes: &BTreeMap<(u16, Option<String>), Option<Vec<String>>>,
    seen_tx: &mpsc::Sender<(String, Vars)>,
) {
    let hosts = scanner.scan().await;
    if network_scan {
        for host in &hosts {
            let _ = seen_tx.send(("scan".to_string(), host.vars())).await;
        }
    }
    for ((port, get), makers) in port_probes {
        let candidates: Vec<Host> = hosts
            .iter()
            .filter(|h| makers.as_ref().is_none_or(|m| maker_wanted(m, &h.manufacturer)))
            .cloned()
            .collect();
        let found = netscan::probe_all(candidates, |host| port_probe(host, *port, get.clone())).await;
        for vars in found {
            let _ = seen_tx.send((probe_key(*port, get), vars)).await;
        }
    }
}

/* One host, one port: open? With `get`: that page's JSON reply as
 * {json.<path>} values. None: not open, or no JSON there. */
async fn port_probe(host: Host, port: u16, get: Option<String>) -> Option<Vars> {
    if !netscan::port_open(host.address, port).await {
        return None;
    }
    let mut vars = host.vars();
    vars.insert("port".into(), port.to_string());
    if let Some(path) = get {
        let target = format!("{}:{port}", host.address);
        let request = crate::adapters::net::http_json(hyper::Method::GET, &target, &path, None);
        let reply = tokio::time::timeout(netscan::PROBE_TIMEOUT, request).await.ok()?.ok()?;
        flatten_json("json", &reply, &mut vars);
    }
    Some(vars)
}

/* Issue #73, RE-FIND BY MAC: devices that announce nothing (or not often)
 * keep working after their address changes. Every PASS:
 *   1. LEARN: an online device's MAC is read from the neighbour table (the
 *      hub just talked to it) and kept in its config as "mac", once;
 *   2. FOLLOW: an OFFLINE device whose MAC (config "mac", or an identity
 *      that is one) shows up at another address gets that address. If the
 *      table doesn't show it, the network is swept -- at most every
 *      REFIND_GAP -- and looked at again.
 * Guard: a WiFi repeater may answer ARP for everyone behind it with its
 * own MAC. A MAC seen at more than one address, or at an address another
 * added device has, is never learned or followed. */
async fn refind(discovery: &Discovery, control: &Control) {
    refind_with(discovery, control, netscan::neighbours).await
}

/* refind, reading the neighbour table with `read_table` (the tests' fake
 * network). */
async fn refind_with(discovery: &Discovery, control: &Control, read_table: impl Fn() -> Vec<Host>) {
    let Ok(devices) = control.list().await else { return };
    let with_host: Vec<(&crate::device::Device, Ipv4Addr)> = devices
        .iter()
        .filter_map(|d| Some((d, d.config.get("host")?.parse().ok()?)))
        .collect();
    if with_host.is_empty() {
        return;
    }
    let mut table = read_table();
    let unique = |table: &[Host], mac: &str| table.iter().filter(|h| h.mac == mac).count() == 1;

    for (device, address) in &with_host {
        if device.online != Some(Health::Online) || device.config.contains_key("mac") {
            continue;
        }
        if let Some(host) = table.iter().find(|h| h.address == *address) {
            if unique(&table, &host.mac) {
                println!("discovery: {} has MAC {}", device.id, host.mac);
                let _ = control
                    .store_config(&device.id, [("mac".to_string(), host.mac.clone())].into())
                    .await;
            }
        }
    }

    let lost: Vec<(&crate::device::Device, Ipv4Addr, String)> = with_host
        .iter()
        .filter(|(d, _)| d.online == Some(Health::Offline))
        .filter_map(|(d, a)| {
            let mac = d.config.get("mac").and_then(|m| netscan::normalize_mac(m)).or_else(|| netscan::normalize_mac(&d.identity))?;
            Some((*d, *a, mac))
        })
        .collect();
    if lost.is_empty() {
        return;
    }
    let moved = |table: &[Host], address: Ipv4Addr, mac: &str| -> Option<Ipv4Addr> {
        let host = table.iter().find(|h| h.mac == mac && h.address != address)?;
        let taken = with_host.iter().any(|(_, a)| *a == host.address);
        (unique(table, mac) && !taken).then_some(host.address)
    };
    let sweep_due = discovery
        .last_refind_sweep
        .lock()
        .unwrap()
        .is_none_or(|t| t.elapsed() >= REFIND_GAP);
    if sweep_due && lost.iter().any(|(_, a, mac)| moved(&table, *a, mac).is_none()) {
        *discovery.last_refind_sweep.lock().unwrap() = Some(Instant::now());
        discovery.scanner.scan().await;
        table = read_table();
    }
    for (device, address, mac) in lost {
        if let Some(new_address) = moved(&table, address, &mac) {
            println!("discovery: {} (MAC {mac}) moved from {address} to {new_address}, following it", device.id);
            let _ = control
                .set_config(&device.id, [("host".to_string(), new_address.to_string())].into())
                .await;
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

/* Where UDP broadcast probes go: the broadcast address of every network
 * the hub is on (192.168.1.255 for 192.168.1.x/24) -- 255.255.255.255
 * would only leave through ONE interface (the default route's), missing
 * the hotspot or a second network. Loopback is skipped. */
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut targets: Vec<Ipv4Addr> = network::ipv4_interfaces()
        .iter()
        .filter(|i| !i.address.is_loopback())
        .map(|i| i.broadcast())
        .collect();
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        targets.push(Ipv4Addr::BROADCAST);
    }
    targets
}

/* One UDP broadcast round: every probe to every target, answers
 * collected SSDP_WAIT. An answer belongs to the probe sent to the port it
 * comes FROM (a WiZ bulb answers from 38899). `from_port`: UDP_PORT (the
 * tests use a free one). */
async fn udp_round(
    probes: &[(u16, String)],
    targets: &[Ipv4Addr],
    from_port: u16,
    seen_tx: &mpsc::Sender<(String, Vars)>,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", from_port)).await?;
    socket.set_broadcast(true)?;
    for (port, probe) in probes {
        for target in targets {
            /* One unreachable network (an interface going down) mustn't
             * stop the others. */
            if let Err(e) = socket.send_to(probe.as_bytes(), (*target, *port)).await {
                println!("discovery: UDP probe to {target}:{port}: {e}");
            }
        }
    }
    let deadline = tokio::time::Instant::now() + SSDP_WAIT;
    let mut buf = vec![0u8; 4096];
    while let Ok(Ok((len, from))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
        if !probes.iter().any(|(port, _)| *port == from.port()) {
            continue;
        }
        let vars = parse_udp(&buf[..len], from);
        if seen_tx.send((format!("udp:{}", from.port()), vars)).await.is_err() {
            break;
        }
    }
    Ok(())
}

/* Listens to a multicast group for the daemon's lifetime. The group is
 * joined on every network the hub is on, again every PASS: the WiFi may
 * connect long after the daemon started. */
fn spawn_multicast_listen(group: Ipv4Addr, port: u16, seen_tx: mpsc::Sender<(String, Vars)>) {
    tokio::spawn(async move {
        let socket = match UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(socket) => socket,
            Err(e) => {
                println!("discovery: can't listen to {group}:{port}: {e}");
                return;
            }
        };
        let what = format!("mcast:{group}:{port}");
        let mut join = tokio::time::interval(PASS);
        let mut buf = vec![0u8; 4096];
        loop {
            tokio::select! {
                _ = join.tick() => {
                    for iface in network::ipv4_interfaces().iter().filter(|i| !i.address.is_loopback()) {
                        /* "Already a member" on the second round: fine. */
                        let _ = socket.join_multicast_v4(group, iface.address);
                    }
                }
                received = socket.recv_from(&mut buf) => {
                    let Ok((len, from)) = received else { continue };
                    if seen_tx.send((what.clone(), parse_udp(&buf[..len], from))).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/* Issue #74: listens for a vendor's broadcasts on `port`. Each is
 * decoded ("roborock": {duid}, and the address it says it has) and passed
 * on as "listen:<port>". */
fn spawn_udp_listen(port: u16, decoder: templates::UdpDecoder, seen_tx: mpsc::Sender<(String, Vars)>) {
    tokio::spawn(async move {
        let socket = match UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(socket) => socket,
            Err(e) => {
                println!("discovery: can't listen on UDP {port}: {e}");
                return;
            }
        };
        let what = format!("listen:{port}");
        let mut buf = vec![0u8; 2048];
        /* A vacuum says the same every few seconds: pass each one on at
         * most once a minute (the inbox only needs to know it's there). */
        let mut last: BTreeMap<String, Instant> = BTreeMap::new();
        loop {
            let Ok((len, from)) = socket.recv_from(&mut buf).await else { continue };
            let Some(vars) = decode_listen(decoder, &buf[..len], from.ip()) else { continue };
            let key = vars.values().cloned().collect::<Vec<_>>().join("/");
            if last.get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
                continue;
            }
            last.retain(|_, t| t.elapsed() < Duration::from_secs(600));
            last.insert(key, Instant::now());
            if seen_tx.send((what.clone(), vars)).await.is_err() {
                return;
            }
        }
    });
}

fn decode_listen(decoder: templates::UdpDecoder, packet: &[u8], from: IpAddr) -> Option<Vars> {
    match decoder {
        templates::UdpDecoder::Roborock => {
            let (duid, ip) = crate::adapters::roborock_proto::decode_broadcast(packet)?;
            /* The address it says it has must be the one it sent from: a
             * broadcast can't point the hub at another machine. */
            if ip.parse::<IpAddr>().ok()? != from {
                return None;
            }
            Some([("address".to_string(), ip), ("duid".to_string(), duid)].into())
        }
    }
}

/* A UDP answer or announcement: {address} and {port} it came from, then
 * either JSON (WiZ: {"method":"getPilot","result":{"mac":...}}) as
 * {json.result.mac}, or "Name: value" lines (Yeelight, SSDP-like) as
 * {header.Name}. */
fn parse_udp(packet: &[u8], from: SocketAddr) -> Vars {
    let mut vars = Vars::new();
    vars.insert("address".into(), from.ip().to_string());
    vars.insert("port".into(), from.port().to_string());
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(packet) {
        flatten_json("json", &json, &mut vars);
    } else if let Ok(text) = std::str::from_utf8(packet) {
        for line in text.split(['\r', '\n']) {
            if let Some((name, value)) = line.split_once(':') {
                if !name.trim().is_empty() && !name.contains(' ') {
                    vars.insert(format!("header.{}", name.trim()), value.trim().to_string());
                }
            }
        }
    }
    vars
}

/* {"result": {"mac": "a8bb50..", "rssi": -60}} under "json" ->
 * json.result.mac = "a8bb50..", json.result.rssi = "-60". List items by
 * index (json.list.0). Texts as they are, numbers and true/false as
 * written in JSON; null is left out. */
fn flatten_json(prefix: &str, value: &serde_json::Value, vars: &mut Vars) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                flatten_json(&format!("{prefix}.{key}"), value, vars);
            }
        }
        Value::Array(items) => {
            for (i, value) in items.iter().enumerate() {
                flatten_json(&format!("{prefix}.{i}"), value, vars);
            }
        }
        Value::String(text) => {
            vars.insert(prefix.to_string(), text.clone());
        }
        Value::Null => {}
        other => {
            vars.insert(prefix.to_string(), other.to_string());
        }
    }
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

    /* Against the REAL network: finds WLED devices and Shelly plugs
     * (mDNS), LG TVs (SSDP) and WiZ lights (UDP broadcast) on the LAN of
     * the machine running it. Not part of the normal test run (it needs
     * those devices and takes a few seconds):
     *     cargo test live_discovery -- --ignored --nocapture
     * (A PC's own firewall may drop the UDP broadcast's answers -- ufw
     * does: they don't look like replies. The hub's opens UDP_PORT.) */
    #[tokio::test]
    #[ignore]
    async fn live_discovery() {
        let (tx, mut rx) = mpsc::channel(64);
        let daemon = mdns_sd::ServiceDaemon::new().unwrap();
        spawn_mdns_browse(&daemon, "_wled._tcp".into(), tx.clone());
        spawn_mdns_browse(&daemon, "_shelly._tcp".into(), tx.clone());
        ssdp_round(&["urn:lge-com:service:webos-second-screen:1".into()], &tx).await.unwrap();
        let wiz = (38899, r#"{"method":"getPilot","params":{}}"#.to_string());
        udp_round(&[wiz], &broadcast_targets(), UDP_PORT, &tx).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while let Ok(Some((what, vars))) = tokio::time::timeout_at(deadline, rx.recv()).await {
            println!("{what}: {vars:?}");
        }
    }

    fn wiz_template() -> Template {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/wiz.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_wiz_answer_becomes_a_found_bulb() {
        let answer = br#"{"method":"getPilot","env":"pro","result":{"mac":"a8bb5012ab34","rssi":-58,"state":true,"dimming":80}}"#;
        let vars = parse_udp(answer, "192.168.1.77:38899".parse().unwrap());
        assert_eq!(vars["json.result.mac"], "a8bb5012ab34");
        assert_eq!(vars["json.result.rssi"], "-58");
        assert_eq!(vars["json.result.state"], "true");
        let t = wiz_template();
        assert!(matches_template(&t.discovery[0], &vars));
        let found = to_found(&t, t.discovery[0].fill(), &vars).unwrap();
        assert_eq!(found.values["host"], "192.168.1.77");
        assert_eq!(found.identity, "a8bb5012ab34");

        /* Something else answering on that port: not a bulb. */
        let other = parse_udp(br#"{"method":"error"}"#, "192.168.1.78:38899".parse().unwrap());
        assert!(!matches_template(&t.discovery[0], &other));
    }

    /* What a real Shelly Plug S Gen3 advertises (seen with
     * live_discovery, firmware 1.7.3): the plug template takes it, with
     * the same id its probe learns -- so IP auto-update works. */
    #[test]
    fn a_shelly_sighting_becomes_a_found_plug() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/shelly-plug-gen3.json");
        let t: Template = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut vars: Vars = [
            ("address", "192.168.1.131"),
            ("name", "shellyplugsg3-d885ac1fb048"),
            ("port", "80"),
            ("txt.app", "PlugSG3"),
            ("txt.gen", "3"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert!(matches_template(&t.discovery[0], &vars));
        let found = to_found(&t, t.discovery[0].fill(), &vars).unwrap();
        assert_eq!(found.identity, "shellyplugsg3-d885ac1fb048");
        assert_eq!(found.values["host"], "192.168.1.131");

        /* Another Shelly (a Plus 1 relay): not this template's. */
        vars.insert("txt.app".into(), "Plus1".into());
        assert!(!matches_template(&t.discovery[0], &vars));
    }

    #[test]
    fn text_announcements_become_headers() {
        let packet = b"NOTIFY * HTTP/1.1\r\nHost: 239.255.255.250:1982\r\nLocation: yeelight://192.168.1.9:55443\r\nid: 0x0000000002dfb19a\r\n\r\n";
        let vars = parse_udp(packet, "192.168.1.9:1982".parse().unwrap());
        assert_eq!(vars["header.Location"], "yeelight://192.168.1.9:55443");
        assert_eq!(vars["header.id"], "0x0000000002dfb19a");
        assert_eq!(vars["address"], "192.168.1.9");
        /* The request line has no "Name:" -- left out. */
        assert!(!vars.keys().any(|k| k.contains("NOTIFY")));
    }

    #[tokio::test]
    async fn a_udp_round_finds_simulated_bulbs() {
        /* Two "bulbs" on localhost answer the probe; a third device
         * answers from another port (not the probe's) and is ignored. */
        let bulb = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = bulb.local_addr().unwrap().port();
        let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            let (len, from) = bulb.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..len], b"hello?");
            bulb.send_to(br#"{"method":"getPilot","result":{"mac":"aa"}}"#, from).await.unwrap();
            stranger.send_to(b"{}", from).await.unwrap();
        });
        let (tx, mut rx) = mpsc::channel(8);
        /* A free port to send from (UDP_PORT may be taken on a dev PC). */
        let from_port = UdpSocket::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        udp_round(&[(port, "hello?".into())], &[Ipv4Addr::LOCALHOST], from_port, &tx).await.unwrap();
        drop(tx);
        let (what, vars) = rx.recv().await.unwrap();
        assert_eq!(what, format!("udp:{port}"));
        assert_eq!(vars["json.result.mac"], "aa");
        assert!(rx.recv().await.is_none(), "only the bulb's answer counts");
    }

    #[tokio::test]
    async fn multicast_listeners_hear_announcements() {
        /* Sent straight to the listening port (multicast routing on a
         * build machine is unpredictable); what's tested is the listener. */
        let port = UdpSocket::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::channel(8);
        spawn_multicast_listen(Ipv4Addr::new(239, 255, 255, 250), port, tx);
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut got = None;
        for _ in 0..20 {
            sender.send_to(b"id: 42\r\n", ("127.0.0.1", port)).await.unwrap();
            if let Ok(Some(seen)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                got = Some(seen);
                break;
            }
        }
        let (what, vars) = got.expect("no announcement heard");
        assert_eq!(what, format!("mcast:239.255.255.250:{port}"));
        assert_eq!(vars["header.id"], "42");
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

    fn wled_template() -> Template {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/wled.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /* Issue #73: a WLED that announces nothing is found by a port probe:
     * an Espressif MAC, port 80 open, /json/info says "WLED". */
    #[tokio::test]
    async fn a_port_probe_finds_a_quiet_wled() {
        let template = wled_template();
        let method = template
            .discovery
            .iter()
            .find(|d| matches!(d, templates::Discovery::PortProbe { .. }))
            .unwrap();
        let host = |manufacturer: &str| Host {
            address: Ipv4Addr::LOCALHOST,
            mac: "24:0a:c4:11:22:33".into(),
            manufacturer: manufacturer.into(),
            interface: "wlan0".into(),
        };
        let wled = crate::adapters::wled_sim::Sim::start(false).await;
        let port: u16 = wled.host().rsplit(':').next().unwrap().parse().unwrap();
        let vars = port_probe(host("Espressif"), port, Some("/json/info".into())).await.unwrap();
        assert!(matches_template(method, &vars));
        let found = to_found(&template, method.fill(), &vars).unwrap();
        assert_eq!(found.address, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(found.values["host"], "127.0.0.1");
        assert!(!found.identity.is_empty());
        /* Another maker: not this template's. */
        assert!(!matches_template(method, &host("TP-Link").vars()));
        /* Port open, but not a WLED. */
        let other = crate::adapters::wled_sim::Sim::start_not_wled().await;
        let port: u16 = other.host().rsplit(':').next().unwrap().parse().unwrap();
        let vars = port_probe(host("Espressif"), port, Some("/json/info".into())).await.unwrap();
        assert!(!matches_template(method, &vars));
        /* Nothing listening. */
        drop(other);
        assert!(port_probe(host("Espressif"), port, None).await.is_none());
    }

    /* Issue #73: one device found two ways is one inbox entry, the one
     * with an identity. */
    #[test]
    fn a_device_found_twice_is_one_entry() {
        let d = Discovery::new();
        let found = |identity: &str| Found {
            template: "lg-webos-tv".into(),
            name: "TV".into(),
            address: "192.168.1.20".parse().unwrap(),
            values: BTreeMap::new(),
            identity: identity.into(),
        };
        assert!(d.record(found("")));
        assert!(d.record(found("uuid-1")));
        assert_eq!(d.entries.lock().unwrap().len(), 1);
        assert!(!d.record(found("")));
        assert_eq!(d.entries.lock().unwrap().len(), 1);
        assert_eq!(d.entries.lock().unwrap().values().next().unwrap().found.identity, "uuid-1");
    }

    /* Issue #73: a WLED's own mDNS name stays when a probe finds it too
     * (seen on the DK2: the probe's "WLED" replaced "WLED-dk2wled"). */
    #[test]
    fn an_announcement_beats_a_probe() {
        let d = Discovery::new();
        let found = |name: &str| Found {
            template: "wled".into(),
            name: name.into(),
            address: "192.168.1.145".parse().unwrap(),
            values: BTreeMap::new(),
            identity: "7ce8b1b0aecc".into(),
        };
        assert!(d.record_from(found("WLED"), true));
        assert!(d.record_from(found("WLED-dk2wled"), false));
        assert!(!d.record_from(found("WLED"), true));
        assert_eq!(d.entries.lock().unwrap().values().next().unwrap().found.name, "WLED-dk2wled");
    }

    /* Issue #73, re-find by MAC, on a fake network: the strip's MAC is
     * learned while it's online; offline, it's followed to the address its
     * MAC turns up at -- unless that MAC is at two addresses (a repeater). */
    #[tokio::test]
    async fn an_offline_device_is_followed_by_its_mac() {
        use crate::adapters::test_hub::TestHub;
        let strip: crate::device::Device = serde_json::from_value(serde_json::json!({
            "id": "strip", "name": "Strip", "template": "wled", "source": "wled",
            "config": {"host": "192.0.2.10"},
            "capabilities": {"switch": {"on": false}}
        }))
        .unwrap();
        let hub = TestHub::start(strip, Box::new(crate::adapters::wled::Wled)).await;
        let d = Discovery::new();
        /* No sweeps here: the fake table is all there is. */
        *d.last_refind_sweep.lock().unwrap() = Some(Instant::now());
        let host = |address: [u8; 4], mac: &str| Host {
            address: address.into(),
            mac: mac.into(),
            manufacturer: "Espressif".into(),
            interface: "wlan0".into(),
        };
        let config = |hub: &TestHub| {
            let control = hub.control.clone();
            async move { control.get("strip").await.unwrap().unwrap().config }
        };

        /* 1. Online at .10: its MAC is learned. */
        hub.control.set_online("strip", Health::Online).await.unwrap();
        refind_with(&d, &hub.control, || vec![host([192, 0, 2, 10], "b8:d6:1a:6b:33:ac")]).await;
        assert_eq!(config(&hub).await["mac"], "b8:d6:1a:6b:33:ac");

        /* 2. Offline; its MAC is at two addresses: not followed. */
        hub.control.set_online("strip", Health::Offline).await.unwrap();
        refind_with(&d, &hub.control, || {
            vec![host([192, 0, 2, 20], "b8:d6:1a:6b:33:ac"), host([192, 0, 2, 21], "b8:d6:1a:6b:33:ac")]
        })
        .await;
        assert_eq!(config(&hub).await["host"], "192.0.2.10");

        /* 3. Offline; its MAC is at .20 alone: followed there. */
        hub.control.set_online("strip", Health::Offline).await.unwrap();
        refind_with(&d, &hub.control, || vec![host([192, 0, 2, 20], "b8:d6:1a:6b:33:ac")]).await;
        assert_eq!(config(&hub).await["host"], "192.0.2.20");
    }
}
