/*
 * network.rs -- the hub's network connections (issue #61): Ethernet and
 * WiFi status, scanning for WiFi networks, joining one, forgetting it, and
 * the WiFi country.
 *
 * WHO DOES WHAT. This module decides nothing about routing and runs no
 * shell commands:
 *   - wpa_supplicant (a system service, see the hub-wifi recipe) joins WiFi
 *     networks and keeps them joined (reconnecting after an outage).
 *   - systemd-networkd gets an IP address over DHCP on whichever link is up.
 *   - the kernel's routing picks the link: Ethernet has route metric 10,
 *     WiFi 20, lower wins -- so the cable is used while it's plugged in
 *     and WiFi takes over the moment it's pulled.
 * This module only TALKS to wpa_supplicant (to scan/connect/forget) and
 * READS the kernel's view (/sys, /proc) to report the status.
 *
 * TALKING TO WPA_SUPPLICANT. It has a control socket,
 * /run/wpa_supplicant/wlan0: a Unix *datagram* socket (like UDP, but a
 * file path instead of an IP address, and only on this machine). We send
 * a text command ("SCAN", "STATUS", ...) and get a text reply. This is
 * exactly what the `wpa_cli` tool does. A client must have its own socket
 * path for the reply to come back to (see Ctrl::open). A client can also
 * ATTACH, after which wpa_supplicant additionally sends it EVENTS such as
 * "<3>CTRL-EVENT-CONNECTED ..." -- used while scanning and connecting to
 * know when something happened, instead of guessing with sleeps.
 *
 * SECURITY. WiFi passwords pass through here but are never logged, and
 * never sent back to clients. Only requests from the hub itself may change
 * the WiFi (see ws.rs) until LAN clients are authenticated (#35).
 *
 * THE COUNTRY. WiFi channels are regulated per country. Out of the box the
 * chip runs in "world" mode, legal everywhere (channels 1-11). Routers
 * announce their country in their beacons, so the first scan adopts the
 * most common one (unless a country was already set) -- that's only a
 * hint (the author's Romanian ISP router announces "DE"), so the WiFi
 * screen shows it and lets the user change it. Stored in wpa_supplicant's
 * own config file on /usr/local, like the saved network.
 *
 * Everything that interprets wpa_supplicant's text is a plain function
 * below (parse_*, unescape_ssid, ...) with unit tests; the actor itself
 * only needs the real board.
 */
use serde::Serialize;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::UnixDatagram;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Instant};

/* Network interface names on the DK2. "end0" is the Ethernet port (the
 * kernel's predictable name for the on-board MAC). */
const ETHERNET_IF: &str = "end0";
const WIFI_IF: &str = "wlan0";

/* wpa_supplicant's control socket for wlan0 (ctrl_interface in its config). */
const WPA_CTRL: &str = "/run/wpa_supplicant/wlan0";
/* Its config file -- only needed here to force it onto the flash after a
 * save (see sync_config). */
const WPA_CONF: &str = "/usr/local/etc/universal-controller/wifi/wpa_supplicant.conf";

/* How long one command may take to be answered. */
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/* A scan of the 2.4 GHz channels takes ~3-4 s on this chip. */
const SCAN_TIMEOUT: Duration = Duration::from_secs(12);
/* Joining a network: association + WPA handshake normally take 1-5 s; a
 * network that isn't found is only given up on after a few scans. */
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);

/* ------------------------------------------------------------------ */
/* What clients see (sent as JSON by ws.rs)                            */
/* ------------------------------------------------------------------ */

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Status {
    /* Which link carries traffic right now: "ethernet", "wifi", or null
     * (offline). Read from the kernel's routing table, i.e. the truth, not
     * a guess from "is it plugged in". */
    pub uplink: Option<&'static str>,
    pub ethernet: EthernetStatus,
    pub wifi: WifiStatus,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct EthernetStatus {
    /* Cable plugged in and the link up (the "carrier"). */
    pub connected: bool,
    pub ip: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct WifiStatus {
    /* wpa_supplicant is running and answering. false = WiFi unusable
     * (no chip, service stopped) -- the UI then hides the WiFi screen. */
    pub available: bool,
    /* Joined to a network right now. */
    pub connected: bool,
    /* The network we're joined to, else the saved one we're trying to join. */
    pub ssid: Option<String>,
    pub signal_dbm: Option<i32>,
    /* 0-4, for a signal icon (see bars()). */
    pub bars: u8,
    pub ip: Option<String>,
    /* The saved network, if any (at most one, see connect()). */
    pub saved_ssid: Option<String>,
    /* Two-letter country code, or None = world mode. */
    pub country: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Network {
    pub ssid: String,
    pub signal_dbm: i32,
    pub bars: u8,
    /* Needs a password (WPA/WPA2). false = open network. */
    pub secure: bool,
    /* The saved network. */
    pub saved: bool,
}

/* ------------------------------------------------------------------ */
/* The actor                                                           */
/* ------------------------------------------------------------------ */

/* What other tasks can ask this module. Each carries its oneshot reply
 * envelope, same pattern as state.rs / rpmsg.rs. */
pub enum Cmd {
    GetStatus {
        reply: oneshot::Sender<Status>,
    },
    Scan {
        reply: oneshot::Sender<Result<Vec<Network>, String>>,
    },
    Connect {
        ssid: String,
        /* None or empty: an open network. */
        password: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Forget {
        reply: oneshot::Sender<Result<(), String>>,
    },
    SetCountry {
        country: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/* One command at a time, in order: wpa_supplicant's network list is shared
 * state, and a scan in the middle of a connect (or two connects at once)
 * would confuse both. A connect can take up to CONNECT_TIMEOUT; requests
 * arriving meanwhile simply wait their turn. */
pub async fn run(mut rx: mpsc::Receiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::GetStatus { reply } => {
                let _ = reply.send(status().await);
            }
            Cmd::Scan { reply } => {
                let _ = reply.send(scan().await);
            }
            Cmd::Connect { ssid, password, reply } => {
                let result = connect(&ssid, password.as_deref().unwrap_or("")).await;
                /* The password is deliberately NOT in this log line. */
                match &result {
                    Ok(()) => println!("network: joined WiFi \"{ssid}\""),
                    Err(e) => println!("network: could not join WiFi \"{ssid}\": {e}"),
                }
                let _ = reply.send(result);
            }
            Cmd::Forget { reply } => {
                let result = forget().await;
                if result.is_ok() {
                    println!("network: saved WiFi network forgotten");
                }
                let _ = reply.send(result);
            }
            Cmd::SetCountry { country, reply } => {
                let result = set_country(&country).await;
                if result.is_ok() {
                    println!("network: WiFi country set to {country}");
                }
                let _ = reply.send(result);
            }
        }
    }
}

/* How often watch_uplink checks which link carries traffic. */
const UPLINK_POLL: Duration = Duration::from_secs(1);

/* Keeps `tx` up to date with the interface the default route uses right
 * now ("end0", "wlan0", or None = offline). mqtt.rs watches it: when the
 * cable is pulled, its TCP connection to AWS is dead, but MQTT itself would
 * only notice after its keep-alive fails -- measured 45 s on the DK2. With
 * this, it reconnects over the new link within a second or two.
 *
 * Polling a small /proc file once a second costs nothing measurable; the
 * alternative (a netlink socket for route change events) is far more code
 * for the same result. send_if_modified: receivers are only woken when the
 * value actually changes. */
pub async fn watch_uplink(tx: tokio::sync::watch::Sender<Option<String>>) {
    let mut tick = tokio::time::interval(UPLINK_POLL);
    loop {
        tick.tick().await;
        let table = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
        let now = default_route_interface(&table);
        tx.send_if_modified(|current| {
            if *current == now {
                return false;
            }
            println!("network: traffic now goes over {}", now.as_deref().unwrap_or("nothing (offline)"));
            *current = now;
            true
        });
    }
}

/* ------------------------------------------------------------------ */
/* Operations                                                          */
/* ------------------------------------------------------------------ */

async fn status() -> Status {
    let addresses = ipv4_addresses();
    let route = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    let uplink = match default_route_interface(&route).as_deref() {
        Some(ETHERNET_IF) => Some("ethernet"),
        Some(WIFI_IF) => Some("wifi"),
        _ => None,
    };
    let ethernet = EthernetStatus {
        connected: read_trimmed(&format!("/sys/class/net/{ETHERNET_IF}/carrier")).as_deref() == Some("1"),
        ip: addresses.get(ETHERNET_IF).map(|ip| ip.to_string()),
    };

    let mut wifi = WifiStatus::default();
    if let Ok(ctrl) = Ctrl::open(false).await {
        if let Ok(text) = ctrl.request("STATUS").await {
            wifi.available = true;
            let fields = parse_key_values(&text);
            wifi.connected = fields.get("wpa_state").map(String::as_str) == Some("COMPLETED");
            wifi.ssid = fields.get("ssid").map(|s| unescape_ssid(s));
        }
        if wifi.connected {
            if let Ok(text) = ctrl.request("SIGNAL_POLL").await {
                wifi.signal_dbm = parse_key_values(&text).get("RSSI").and_then(|v| v.parse().ok());
                wifi.bars = wifi.signal_dbm.map_or(0, bars);
            }
        }
        if let Ok(text) = ctrl.request("LIST_NETWORKS").await {
            wifi.saved_ssid = parse_saved_networks(&text).into_iter().next().map(|(_, ssid)| ssid);
        }
        if wifi.ssid.is_none() {
            wifi.ssid = wifi.saved_ssid.clone();
        }
        wifi.country = ctrl.request("GET country").await.ok().and_then(|c| valid_country(&c));
    }
    wifi.ip = addresses.get(WIFI_IF).map(|ip| ip.to_string());

    Status { uplink, ethernet, wifi }
}

async fn scan() -> Result<Vec<Network>, String> {
    let ctrl = Ctrl::open(true).await?;
    /* "FAIL-BUSY" = a scan is already running (wpa_supplicant scans by
     * itself while looking for the saved network): just wait for its
     * results like for our own. */
    let answer = ctrl.request("SCAN").await?;
    if answer != "OK" && answer != "FAIL-BUSY" {
        return Err(format!("scan refused: {answer}"));
    }
    ctrl.wait_for_event(SCAN_TIMEOUT, |event| event.contains("CTRL-EVENT-SCAN-RESULTS"))
        .await
        .ok_or("no scan results (timeout)")?;

    let saved = parse_saved_networks(&ctrl.request("LIST_NETWORKS").await?);
    let saved_ssid = saved.first().map(|(_, ssid)| ssid.clone());
    let results = ctrl.request("SCAN_RESULTS").await?;
    let networks = parse_scan_results(&results, saved_ssid.as_deref());

    /* The country: only if none is set yet (a user's choice is never
     * overridden). Asks for each router's details ("BSS <bssid>"), whose
     * "ie=" line holds its beacon's raw data. */
    if ctrl.request("GET country").await.ok().and_then(|c| valid_country(&c)).is_none() {
        let mut countries = Vec::new();
        for bssid in scan_bssids(&results) {
            if let Ok(details) = ctrl.request(&format!("BSS {bssid}")).await {
                if let Some(ie) = parse_key_values(&details).get("ie") {
                    countries.extend(country_from_ie(ie));
                }
            }
        }
        if let Some(country) = most_common(&countries) {
            match set_country_with(&ctrl, &country).await {
                Ok(()) => println!("network: WiFi country {country} adopted from nearby routers"),
                Err(e) => println!("network: could not set WiFi country {country}: {e}"),
            }
        }
    }
    Ok(networks)
}

/* Joins a network. Only ONE network is ever saved: a hub stays where it
 * is, and "the" WiFi is simpler to show and to reason about than a list.
 * The new network is only saved once it actually connected -- a wrong
 * password leaves the previous setting untouched. */
async fn connect(ssid: &str, password: &str) -> Result<(), String> {
    validate_ssid(ssid)?;
    let key = wifi_key(password)?;
    let ctrl = Ctrl::open(true).await?;
    let old: Vec<String> = parse_saved_networks(&ctrl.request("LIST_NETWORKS").await?)
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    let id = ctrl.request("ADD_NETWORK").await?;
    if id.parse::<u32>().is_err() {
        return Err(format!("could not add network: {id}"));
    }
    let result = try_join(&ctrl, &id, ssid, &key).await;
    match result {
        Ok(()) => {
            /* Joined: drop the previous network, save, and force the file
             * onto the flash. */
            for old_id in &old {
                let _ = ctrl.request(&format!("REMOVE_NETWORK {old_id}")).await;
            }
            save_config(&ctrl).await
        }
        Err(e) => {
            /* Undo: remove the new entry and re-enable the old one
             * (SELECT_NETWORK had disabled it for the attempt). */
            let _ = ctrl.request(&format!("REMOVE_NETWORK {id}")).await;
            let _ = ctrl.request("ENABLE_NETWORK all").await;
            Err(e)
        }
    }
}

/* The attempt itself: configure the new entry, switch to it, and wait for
 * wpa_supplicant to report the outcome. */
async fn try_join(ctrl: &Ctrl, id: &str, ssid: &str, key: &WifiKey) -> Result<(), String> {
    /* The SSID in hex: a network name can contain any bytes, including
     * quotes and spaces, and hex needs no escaping at all. */
    set_network(ctrl, id, "ssid", &hex(ssid.as_bytes())).await?;
    /* Also find networks that don't broadcast their name. */
    set_network(ctrl, id, "scan_ssid", "1").await?;
    match key {
        WifiKey::Open => set_network(ctrl, id, "key_mgmt", "NONE").await?,
        /* A passphrase goes in double quotes. wpa_supplicant takes
         * everything up to the LAST quote, so quotes inside the password
         * are fine; control characters were already refused. */
        WifiKey::Passphrase(p) => set_network(ctrl, id, "psk", &format!("\"{p}\"")).await?,
        /* 64 hex digits: the raw key itself, written without quotes. */
        WifiKey::RawPsk(p) => set_network(ctrl, id, "psk", p).await?,
    }
    expect_ok(ctrl.request(&format!("SELECT_NETWORK {id}")).await?, "select network")?;

    /* Wait for the outcome. Not found is reported after every scan that
     * didn't see the network; give it three scans before giving up. */
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut not_found = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(event) = ctrl.next_event(left).await else {
            return Err("timed out (network out of range, or a wrong password?)".into());
        };
        match classify_connect_event(&event) {
            ConnectEvent::Connected => return Ok(()),
            ConnectEvent::WrongPassword => return Err("wrong password".into()),
            ConnectEvent::NotFound => {
                not_found += 1;
                if not_found >= 3 {
                    return Err("network not found".into());
                }
            }
            ConnectEvent::Other => {}
        }
    }
}

async fn forget() -> Result<(), String> {
    let ctrl = Ctrl::open(false).await?;
    expect_ok(ctrl.request("REMOVE_NETWORK all").await?, "remove networks")?;
    save_config(&ctrl).await
}

async fn set_country(country: &str) -> Result<(), String> {
    let country = valid_country(country).ok_or("country must be two letters, e.g. RO or US")?;
    let ctrl = Ctrl::open(false).await?;
    set_country_with(&ctrl, &country).await
}

async fn set_country_with(ctrl: &Ctrl, country: &str) -> Result<(), String> {
    /* wpa_supplicant passes the country on to the kernel's regulatory
     * code right away (channels 12/13 on or off) and keeps it in the file. */
    expect_ok(ctrl.request(&format!("SET country {country}")).await?, "set country")?;
    save_config(ctrl).await
}

async fn set_network(ctrl: &Ctrl, id: &str, name: &str, value: &str) -> Result<(), String> {
    expect_ok(ctrl.request(&format!("SET_NETWORK {id} {name} {value}")).await?, name)
}

/* SAVE_CONFIG writes the config to a .tmp file and renames it into place
 * (wpa_supplicant's config_file.c), but never fsyncs. Doing that here makes
 * the new setting survive a power cut right after. */
async fn save_config(ctrl: &Ctrl) -> Result<(), String> {
    expect_ok(ctrl.request("SAVE_CONFIG").await?, "save configuration")?;
    tokio::task::spawn_blocking(|| sync_config(Path::new(WPA_CONF)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not sync the WiFi configuration: {e}"))
}

fn sync_config(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    std::fs::File::open(path.parent().unwrap_or(Path::new("/")))?.sync_all()
}

fn expect_ok(answer: String, what: &str) -> Result<(), String> {
    if answer == "OK" {
        Ok(())
    } else {
        Err(format!("{what} failed: {answer}"))
    }
}

/* ------------------------------------------------------------------ */
/* The control socket client                                           */
/* ------------------------------------------------------------------ */

/* One conversation with wpa_supplicant. Created per operation and dropped
 * at its end -- cheap (a socket and a file name), and nothing lingers. */
struct Ctrl {
    socket: UnixDatagram,
    /* Our own end's file name, deleted again in Drop. */
    local: PathBuf,
    attached: bool,
}

/* Makes every client socket name unique within this process. */
static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);

impl Ctrl {
    /* `attach`: also receive events (see the header comment). */
    async fn open(attach: bool) -> Result<Self, String> {
        let n = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        let local = PathBuf::from(format!("/run/backend-daemon-wpa-{}-{n}", std::process::id()));
        /* A leftover from a crashed run would make bind() fail. */
        let _ = std::fs::remove_file(&local);
        let socket = UnixDatagram::bind(&local).map_err(|e| format!("WiFi control socket: {e}"))?;
        /* `attached` is set before ATTACH is even sent: its "OK" reply
         * doesn't start with "<", so request() still returns it. */
        let ctrl = Ctrl { socket, local, attached: attach };
        ctrl.socket
            .connect(WPA_CTRL)
            .map_err(|e| format!("WiFi service not reachable ({WPA_CTRL}): {e}"))?;
        if attach {
            expect_ok(ctrl.request("ATTACH").await?, "attach")?;
        }
        Ok(ctrl)
    }

    /* Sends one command and returns its reply (trailing newline removed).
     * On an attached socket, events can arrive before the reply; they
     * start with "<" (e.g. "<3>CTRL-EVENT-...") and are skipped here. */
    async fn request(&self, command: &str) -> Result<String, String> {
        self.socket.send(command.as_bytes()).await.map_err(|e| e.to_string())?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let text = self.receive(left).await.ok_or_else(|| {
                /* Don't echo SET_NETWORK commands: they may hold a password. */
                let name = command.split(' ').next().unwrap_or(command);
                format!("no answer from the WiFi service to {name}")
            })?;
            if !(self.attached && text.starts_with('<')) {
                return Ok(text);
            }
        }
    }

    /* The next event, or None when `wait` runs out. */
    async fn next_event(&self, wait: Duration) -> Option<String> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let text = self.receive(left).await?;
            if text.starts_with('<') {
                return Some(text);
            }
        }
    }

    /* Waits for the first event `wanted` accepts. */
    async fn wait_for_event(&self, wait: Duration, wanted: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + wait;
        loop {
            let event = self.next_event(deadline.saturating_duration_since(Instant::now())).await?;
            if wanted(&event) {
                return Some(event);
            }
        }
    }

    async fn receive(&self, wait: Duration) -> Option<String> {
        /* Replies can be long (SCAN_RESULTS with many networks, BSS with
         * all of a router's data): 16 KiB is what wpa_cli uses too. */
        let mut buf = vec![0u8; 16384];
        let len = timeout(wait, self.socket.recv(&mut buf)).await.ok()?.ok()?;
        Some(String::from_utf8_lossy(&buf[..len]).trim_end().to_string())
    }
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        /* An attached client is detached by wpa_supplicant itself once its
         * socket is gone; only our file needs removing. */
        let _ = std::fs::remove_file(&self.local);
    }
}

/* ------------------------------------------------------------------ */
/* Pure helpers (unit-tested below)                                    */
/* ------------------------------------------------------------------ */

/* "key=value" lines (STATUS, SIGNAL_POLL, BSS) -> map. */
fn parse_key_values(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/* wpa_supplicant prints SSIDs "printf-escaped": \\ for a backslash, \"
 * for a quote, \xNN for any other byte that isn't plain printable ASCII
 * (e.g. each byte of a UTF-8 emoji). This turns that back into the name. */
fn unescape_ssid(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'x' if i + 3 < bytes.len() => {
                    let digits = std::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or("");
                    if let Ok(b) = u8::from_str_radix(digits, 16) {
                        out.push(b);
                        i += 4;
                        continue;
                    }
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                c @ (b'\\' | b'"' | b'e' | b't' | b'r') => {
                    out.push(match c {
                        b'e' => 0x1b,
                        b't' => b'\t',
                        b'r' => b'\r',
                        other => other,
                    });
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/* LIST_NETWORKS -> [(id, ssid)]. Format: a header line, then
 * "id<TAB>ssid<TAB>bssid<TAB>flags" per saved network. */
fn parse_saved_networks(text: &str) -> Vec<(String, String)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let id = cols.next()?.to_string();
            let ssid = unescape_ssid(cols.next()?);
            id.parse::<u32>().ok().map(|_| (id, ssid))
        })
        .collect()
}

/* SCAN_RESULTS -> the networks to show. Format: a header line, then
 * "bssid<TAB>frequency<TAB>signal<TAB>flags<TAB>ssid" per ROUTER. One
 * network is often several routers (a mesh, or 2.4 GHz and 5 GHz of the
 * same box), so they're merged by name, keeping the strongest signal.
 * Hidden networks (no name) are left out; they can't be picked from a list. */
fn parse_scan_results(text: &str, saved_ssid: Option<&str>) -> Vec<Network> {
    let mut by_name: HashMap<String, Network> = HashMap::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        let [_bssid, _freq, signal, flags, ssid] = cols.as_slice() else {
            continue;
        };
        let ssid = unescape_ssid(ssid);
        let Ok(signal_dbm) = signal.parse::<i32>() else {
            continue;
        };
        if ssid.is_empty() || ssid.bytes().all(|b| b == 0) {
            continue;
        }
        let network = Network {
            saved: saved_ssid == Some(ssid.as_str()),
            secure: flags.contains("WPA") || flags.contains("WEP") || flags.contains("SAE"),
            signal_dbm,
            bars: bars(signal_dbm),
            ssid: ssid.clone(),
        };
        let better = by_name.get(&ssid).map_or(true, |known| signal_dbm > known.signal_dbm);
        if better {
            by_name.insert(ssid, network);
        }
    }
    let mut networks: Vec<Network> = by_name.into_values().collect();
    /* Strongest first; equal signals by name, so the order is stable. */
    networks.sort_by(|a, b| b.signal_dbm.cmp(&a.signal_dbm).then_with(|| a.ssid.cmp(&b.ssid)));
    networks
}

/* The routers' addresses from SCAN_RESULTS (first column). */
fn scan_bssids(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| line.split('\t').next())
        .filter(|b| b.len() == 17)
        .map(str::to_string)
        .collect()
}

/* A router's beacon data, as the hex string on BSS's "ie=" line, is a
 * list of elements: [id][length][length bytes of data]. Element 7 is the
 * country; its first two bytes are the code in ASCII ("DE"). */
fn country_from_ie(ie_hex: &str) -> Option<String> {
    let bytes: Vec<u8> = (0..ie_hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(ie_hex.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let (id, len) = (bytes[i], bytes[i + 1] as usize);
        let data = bytes.get(i + 2..i + 2 + len)?;
        if id == 7 && data.len() >= 2 {
            return valid_country(std::str::from_utf8(&data[..2]).ok()?);
        }
        i += 2 + len;
    }
    None
}

/* "RO" / "us" -> Some("RO"); anything else (including world mode "00",
 * which is the absence of a country) -> None. */
fn valid_country(text: &str) -> Option<String> {
    let code = text.trim().to_ascii_uppercase();
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())).then_some(code)
}

/* The most frequent entry; ties go to the alphabetically first one, so the
 * result doesn't depend on scan order. */
fn most_common(items: &[String]) -> Option<String> {
    let mut counts: HashMap<&String, usize> = HashMap::new();
    for item in items {
        *counts.entry(item).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|(a, ca), (b, cb)| ca.cmp(cb).then_with(|| b.cmp(a)))
        .map(|(item, _)| item.clone())
}

/* Signal strength in dBm (always negative; closer to 0 = stronger) ->
 * 0-4 bars, roughly as phones show it. */
fn bars(dbm: i32) -> u8 {
    match dbm {
        d if d >= -55 => 4,
        d if d >= -65 => 3,
        d if d >= -75 => 2,
        d if d >= -85 => 1,
        _ => 0,
    }
}

fn validate_ssid(ssid: &str) -> Result<(), String> {
    if ssid.is_empty() || ssid.len() > 32 {
        return Err("network name must be 1-32 bytes".into());
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum WifiKey {
    Open,
    Passphrase(String),
    RawPsk(String),
}

/* The rules of WPA/WPA2-Personal: a passphrase is 8-63 printable ASCII
 * characters; alternatively the key itself as 64 hex digits. Empty = an
 * open network. Checked here so the user gets a clear message instead of
 * a bare "FAIL" from wpa_supplicant. */
fn wifi_key(password: &str) -> Result<WifiKey, String> {
    if password.is_empty() {
        return Ok(WifiKey::Open);
    }
    if password.len() == 64 && password.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(WifiKey::RawPsk(password.to_string()));
    }
    if !(8..=63).contains(&password.len()) {
        return Err("WiFi passwords are 8 to 63 characters long".into());
    }
    if !password.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err("the password contains characters WiFi passwords can't have".into());
    }
    Ok(WifiKey::Passphrase(password.to_string()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, PartialEq)]
enum ConnectEvent {
    Connected,
    WrongPassword,
    NotFound,
    Other,
}

/* The events that decide a connection attempt. A wrong password shows up
 * as the network being temporarily disabled with reason WRONG_KEY. */
fn classify_connect_event(event: &str) -> ConnectEvent {
    if event.contains("CTRL-EVENT-CONNECTED") {
        ConnectEvent::Connected
    } else if event.contains("CTRL-EVENT-SSID-TEMP-DISABLED") && event.contains("reason=WRONG_KEY") {
        ConnectEvent::WrongPassword
    } else if event.contains("CTRL-EVENT-NETWORK-NOT-FOUND") {
        ConnectEvent::NotFound
    } else {
        ConnectEvent::Other
    }
}

/* /proc/net/route lists the kernel's IPv4 routes, one per line:
 * "Iface Destination Gateway Flags RefCnt Use Metric ..." with numbers in
 * hex. The default route is the one to destination 00000000; with both
 * links up there are two, and the one with the LOWER metric is the one
 * actually used. Returns that route's interface. */
fn default_route_interface(table: &str) -> Option<String> {
    const RTF_UP: u32 = 0x1;
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let (iface, dest, flags, metric) = (cols.first()?, cols.get(1)?, cols.get(3)?, cols.get(6)?);
            let flags = u32::from_str_radix(flags, 16).ok()?;
            (*dest == "00000000" && flags & RTF_UP != 0).then(|| (metric.parse::<u32>().ok(), iface.to_string()))
        })
        .filter_map(|(metric, iface)| Some((metric?, iface)))
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, iface)| iface)
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/* Every interface's IPv4 address, via getifaddrs() -- the C library call
 * `ip addr` itself uses. */
pub fn ipv4_addresses() -> HashMap<String, Ipv4Addr> {
    let mut result = HashMap::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    /* SAFETY: getifaddrs allocates a linked list and stores its head in
     * `list`; we only read it, following ifa_next until null, and hand it
     * back to freeifaddrs exactly once. ifa_addr may be null (interfaces
     * without an address) and is checked before use; it's only cast to
     * sockaddr_in when its family says it IS one (AF_INET). */
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return result;
        }
        let mut entry = list;
        while !entry.is_null() {
            let ifa = &*entry;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                /* s_addr is in network byte order (big-endian). */
                result.insert(name, Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)));
            }
            entry = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssids_are_unescaped() {
        assert_eq!(unescape_ssid("DIGI-F9rT"), "DIGI-F9rT");
        assert_eq!(unescape_ssid(r#"Joe\"s \\ net"#), r#"Joe"s \ net"#);
        /* UTF-8 "é" is the two bytes c3 a9. */
        assert_eq!(unescape_ssid(r"Caf\xc3\xa9"), "Café");
        /* A stray backslash at the end is kept as is. */
        assert_eq!(unescape_ssid(r"a\"), r"a\");
    }

    #[test]
    fn scan_results_are_merged_by_name_and_sorted() {
        let text = "bssid / frequency / signal level / flags / ssid\n\
            5c:a6:e6:ed:a5:57\t2422\t-26\t[WPA-PSK-CCMP][WPA2-PSK-CCMP][WPS][ESS]\tDIGI-F9rT\n\
            7e:a6:e6:ed:a5:57\t2422\t-26\t[WPA2-PSK-CCMP][ESS]\t\n\
            7c:f1:7e:bc:73:c8\t2422\t-53\t[WPA-PSK-CCMP][WPA2-PSK-CCMP][WPS][ESS]\tDIGI-F9rT\n\
            b0:a7:b9:0f:d2:3b\t2437\t-91\t[ESS]\tFlory - Birou\n\
            b8:3a:08:3b:2e:42\t2417\t-76\t[WPA2-PSK-CCMP][ESS]\tN4@2.4GHz - D+P\n";
        let networks = parse_scan_results(text, Some("DIGI-F9rT"));
        let names: Vec<&str> = networks.iter().map(|n| n.ssid.as_str()).collect();
        /* Hidden (empty) one left out, the two DIGI routers merged. */
        assert_eq!(names, vec!["DIGI-F9rT", "N4@2.4GHz - D+P", "Flory - Birou"]);
        assert_eq!(networks[0].signal_dbm, -26);
        assert!(networks[0].saved && networks[0].secure);
        assert!(!networks[2].secure && !networks[2].saved);
        assert_eq!(networks[2].bars, 0);
    }

    #[test]
    fn saved_networks_are_parsed() {
        let text = "network id / ssid / bssid / flags\n0\tDIGI-F9rT\tany\t[CURRENT]\n";
        assert_eq!(parse_saved_networks(text), vec![("0".to_string(), "DIGI-F9rT".to_string())]);
        assert!(parse_saved_networks("network id / ssid / bssid / flags\n").is_empty());
    }

    #[test]
    fn country_is_read_from_beacon_data() {
        /* The start of the real "ie=" of the author's router: SSID element
         * (id 0, 9 bytes), rates (id 1, 8 bytes), channel (id 3), then the
         * country element (id 7, 6 bytes: "DE " + channel triplet). */
        let ie = "0009444947492d46397254010882848b960c1218240301030706444520010d14";
        assert_eq!(country_from_ie(ie), Some("DE".to_string()));
        /* No country element, and cut-off data: None, not a panic. */
        assert_eq!(country_from_ie("0009444947492d463972540301"), None);
        assert_eq!(country_from_ie("0709"), None);
    }

    #[test]
    fn countries_are_validated_and_counted() {
        assert_eq!(valid_country("ro"), Some("RO".to_string()));
        assert_eq!(valid_country("00"), None);
        assert_eq!(valid_country("ROU"), None);
        let seen: Vec<String> = ["DE", "RO", "DE", "US"].iter().map(|s| s.to_string()).collect();
        assert_eq!(most_common(&seen), Some("DE".to_string()));
        /* A tie is decided alphabetically, not by scan order. */
        let tie: Vec<String> = ["US", "RO"].iter().map(|s| s.to_string()).collect();
        assert_eq!(most_common(&tie), Some("RO".to_string()));
        assert_eq!(most_common(&[]), None);
    }

    #[test]
    fn passwords_follow_wpa_rules() {
        assert_eq!(wifi_key(""), Ok(WifiKey::Open));
        assert_eq!(wifi_key("secret12"), Ok(WifiKey::Passphrase("secret12".into())));
        assert_eq!(wifi_key(r#"with "quotes" 1"#), Ok(WifiKey::Passphrase(r#"with "quotes" 1"#.into())));
        assert!(wifi_key("short").is_err());
        assert!(wifi_key(&"x".repeat(64)).is_err());
        assert!(wifi_key("line\nbreak!").is_err());
        let raw = "a".repeat(64);
        assert_eq!(wifi_key(&raw), Ok(WifiKey::RawPsk(raw.clone())));
    }

    #[test]
    fn connect_events_are_classified() {
        assert_eq!(
            classify_connect_event("<3>CTRL-EVENT-CONNECTED - Connection to 5c:a6:e6:ed:a5:57 completed [id=1 id_str=]"),
            ConnectEvent::Connected
        );
        assert_eq!(
            classify_connect_event("<3>CTRL-EVENT-SSID-TEMP-DISABLED id=1 ssid=\"DIGI\" auth_failures=1 duration=10 reason=WRONG_KEY"),
            ConnectEvent::WrongPassword
        );
        assert_eq!(classify_connect_event("<3>CTRL-EVENT-NETWORK-NOT-FOUND"), ConnectEvent::NotFound);
        assert_eq!(classify_connect_event("<3>CTRL-EVENT-SCAN-STARTED"), ConnectEvent::Other);
    }

    #[test]
    fn the_default_route_with_the_lowest_metric_wins() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            wlan0\t00000000\t0101A8C0\t0003\t0\t0\t20\t00000000\t0\t0\t0\n\
            end0\t00000000\t0101A8C0\t0003\t0\t0\t10\t00000000\t0\t0\t0\n\
            end0\t0001A8C0\t00000000\t0001\t0\t0\t10\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_route_interface(table), Some("end0".to_string()));
        /* Cable pulled: only WiFi's default route is left. */
        let wifi_only = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\n\
            wlan0\t00000000\t0101A8C0\t0003\t0\t0\t20\n";
        assert_eq!(default_route_interface(wifi_only), Some("wlan0".to_string()));
        assert_eq!(default_route_interface("Iface\tDestination\n"), None);
    }

    #[test]
    fn signal_bars() {
        assert_eq!(bars(-26), 4);
        assert_eq!(bars(-60), 3);
        assert_eq!(bars(-70), 2);
        assert_eq!(bars(-80), 1);
        assert_eq!(bars(-91), 0);
    }

    #[test]
    fn ssid_goes_out_as_hex() {
        assert_eq!(hex(b"Joe's"), "4a6f652773");
        assert!(validate_ssid("").is_err());
        assert!(validate_ssid(&"x".repeat(33)).is_err());
    }
}
