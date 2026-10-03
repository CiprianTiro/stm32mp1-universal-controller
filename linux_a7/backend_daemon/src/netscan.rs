/*
 * netscan.rs -- finding devices that announce NOTHING (issue #73).
 *
 * Most devices say "I'm here" (mDNS, SSDP, a UDP answer: discovery.rs).
 * Some don't: a camera, a plug with the vendor's firmware, a TV whose
 * discovery is switched off. This file finds them anyway, in two steps:
 *
 *   1. WHO IS THERE (network scan). Every device on the hub's own network
 *      must answer ARP -- "who has 192.168.1.50?" -- or nothing could ever
 *      reach it. The kernel asks that by itself whenever we send a packet
 *      to an address it doesn't know yet, and keeps the answers in its
 *      neighbour table, which any program can read (/proc/net/arp). So:
 *      one tiny UDP packet to every address of the subnet (to port 9,
 *      "discard"), a few seconds' wait, then read the table. No raw socket,
 *      no ping, no root: backend_daemon needs no new permission (#37).
 *      Each answer is a MAC address; its first three bytes (the "OUI")
 *      say who made the network chip -- oui.rs looks the name up.
 *
 *   2. WHAT IS IT (port probe). A template can name a TCP port its devices
 *      answer on (WLED: 80, the LG TV: 3000), optionally a page to GET
 *      there, and values the reply must have ("brand": "WLED"). Only the
 *      hosts step 1 found are tried, and only those of the template's
 *      manufacturers -- never every port of every address.
 *
 * GENTLE ON THE NETWORK. A hub must never look like an attacker's port
 * scanner to the router or to anyone watching. So:
 *   - only the hub's own subnet (at most MAX_HOSTS addresses), never the
 *     hotspot's (uap0), never routed networks;
 *   - paced: one packet every SWEEP_PACE, two passes (a /24 takes ~11 s);
 *   - only when asked: the wizard's "search" (discover_now), or a re-find
 *     of a device that went offline (discovery.rs), and at most once per
 *     MIN_SWEEP_GAP whatever asks;
 *   - port probes: a few at a time (PROBE_PARALLEL), short timeouts, one
 *     connection per host and port.
 * Reading the neighbour table alone costs nothing (no packet) and is done
 * freely: it already knows every device the hub talked to recently.
 */
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::net::{TcpStream, UdpSocket};

use crate::network;
use crate::oui;

/* The biggest network swept whole: a /22. On a bigger one (a campus, a
 * badly configured router) only the hub's own /24 is swept. */
const MAX_HOSTS: u32 = 1024;
/* Time between two sweep packets: 100 per second. */
const SWEEP_PACE: Duration = Duration::from_millis(10);
/* After the last packet: the kernel asks an address 3 times, 1 s apart,
 * before giving up on it. */
const SWEEP_SETTLE: Duration = Duration::from_secs(3);
/* At most one sweep this often, whoever asks. */
pub const MIN_SWEEP_GAP: Duration = Duration::from_secs(60);
/* A port probe's connection (and its GET) must be done in this time. A
 * device on the LAN answers in milliseconds. */
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/* Port probes at once. */
const PROBE_PARALLEL: usize = 8;
/* The "discard" port: the packet's only job is to make the kernel ask ARP.
 * (Nothing listens there; a device may answer "port unreachable", which
 * the firewall lets in as "related" and the kernel drops.) */
const DISCARD_PORT: u16 = 9;

/* One device on the network, as the neighbour table knows it. */
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub address: Ipv4Addr,
    /* "aa:bb:cc:dd:ee:ff", lowercase. */
    pub mac: String,
    /* oui::manufacturer: "Espressif", "private address", or "". */
    pub manufacturer: String,
    /* The hub's interface it was seen on ("wlan0"). */
    pub interface: String,
}

impl Host {
    /* The values a template's "fill" and "match" can use:
     * {address}, {mac} (aa:bb:..), {mac_hex} (aabbcc.., as WLED and Shelly
     * write it), {manufacturer}, {interface}. */
    pub fn vars(&self) -> crate::discovery::Vars {
        [
            ("address", self.address.to_string()),
            ("mac", self.mac.clone()),
            ("mac_hex", self.mac.replace(':', "")),
            ("manufacturer", self.manufacturer.clone()),
            ("interface", self.interface.clone()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }
}

/* A MAC address written any common way -- "AA:BB:CC:DD:EE:FF",
 * "aa-bb-...", "aabbccddeeff" -- as "aa:bb:cc:dd:ee:ff". None if it isn't
 * one (a device identity can also be a UUID or a name). */
pub fn normalize_mac(text: &str) -> Option<String> {
    let hex: String = text.chars().filter(|c| !matches!(c, ':' | '-' | '.')).collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let hex = hex.to_ascii_lowercase();
    Some((0..6).map(|i| &hex[i * 2..i * 2 + 2]).collect::<Vec<_>>().join(":"))
}

/* Who is on the network right now, as far as the kernel knows: no packet
 * sent. */
pub fn neighbours() -> Vec<Host> {
    std::fs::read_to_string("/proc/net/arp")
        .map(|text| parse_arp(&text))
        .unwrap_or_default()
}

/* /proc/net/arp, e.g.:
 *   IP address       HW type     Flags       HW address            Mask     Device
 *   192.168.1.1      0x1         0x2         7c:f1:7e:bc:73:c8     *        wlan0
 * Flags 0x2 ("complete") = the device answered; without it the kernel is
 * still asking, or gave up (MAC all zeros). */
fn parse_arp(text: &str) -> Vec<Host> {
    const ATF_COM: u32 = 0x2;
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let address: Ipv4Addr = cols.first()?.parse().ok()?;
            let flags = u32::from_str_radix(cols.get(2)?.trim_start_matches("0x"), 16).ok()?;
            let mac = normalize_mac(cols.get(3)?)?;
            let interface = cols.get(5)?.to_string();
            (flags & ATF_COM != 0 && mac != "00:00:00:00:00:00").then(|| Host {
                address,
                manufacturer: oui::manufacturer(&mac),
                mac,
                interface,
            })
        })
        .collect()
}

/* The addresses one sweep sends to: every host address of every network
 * the hub is on (but the hotspot, loopback and the hub itself). A network
 * bigger than MAX_HOSTS: just the hub's own /24 of it. */
fn sweep_targets(interfaces: &[network::Ipv4Interface]) -> Vec<Ipv4Addr> {
    let mut targets = Vec::new();
    for interface in interfaces {
        if interface.address.is_loopback() || interface.name == "uap0" {
            continue;
        }
        let address = u32::from(interface.address);
        let mut mask = u32::from(interface.netmask);
        if !mask >= MAX_HOSTS {
            mask = 0xffff_ff00;
        }
        let network = address & mask;
        let broadcast = network | !mask;
        /* /31 and /32 have no network and broadcast addresses: nothing to
         * sweep there anyway (a point-to-point link). */
        for host in network.saturating_add(1)..broadcast {
            if host != address {
                targets.push(Ipv4Addr::from(host));
            }
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

/* Remembers when the last sweep ran, so no caller can make the hub sweep
 * more often than MIN_SWEEP_GAP. */
pub struct Scanner {
    last_sweep: Mutex<Option<Instant>>,
}

impl Scanner {
    pub fn new() -> Self {
        Scanner {
            last_sweep: Mutex::new(None),
        }
    }

    /* Sweeps the hub's network (see the top of this file), then returns who
     * is there. Within MIN_SWEEP_GAP of the last sweep it doesn't send
     * anything: the table is still fresh, so it's just read. */
    pub async fn scan(&self) -> Vec<Host> {
        let due = {
            let mut last = self.last_sweep.lock().unwrap();
            let due = last.is_none_or(|t| t.elapsed() >= MIN_SWEEP_GAP);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        if due {
            let targets = sweep_targets(&network::ipv4_interfaces());
            if let Err(e) = sweep(&targets).await {
                println!("netscan: sweep failed: {e}");
            }
        }
        neighbours()
    }
}

/* One UDP packet to every target, paced; the kernel's ARP is given time
 * to finish; then once more to the addresses that didn't answer. The
 * second pass is for ESP32s and other battery-minded WiFi chips: in
 * power save they sleep through broadcasts, and the kernel's 3 ARP
 * questions can all fall into one nap -- seen on the home LAN (an ESP32
 * a ping found, the first pass didn't). */
async fn sweep(targets: &[Ipv4Addr]) -> std::io::Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    println!("netscan: sweeping {} addresses", targets.len());
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    send_paced(&socket, targets).await;
    let known: Vec<Ipv4Addr> = neighbours().iter().map(|h| h.address).collect();
    let silent: Vec<Ipv4Addr> = targets.iter().filter(|t| !known.contains(t)).copied().collect();
    send_paced(&socket, &silent).await;
    Ok(())
}

async fn send_paced(socket: &UdpSocket, targets: &[Ipv4Addr]) {
    let mut ticker = tokio::time::interval(SWEEP_PACE);
    for target in targets {
        ticker.tick().await;
        /* Errors are normal here: an address the kernel recently failed to
         * reach answers "host unreachable" at once. */
        let _ = socket.send_to(&[0], (*target, DISCARD_PORT)).await;
    }
    tokio::time::sleep(SWEEP_SETTLE).await;
}

/* Is something listening on this TCP port? */
pub async fn port_open(address: Ipv4Addr, port: u16) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(SocketAddr::new(IpAddr::V4(address), port))).await,
        Ok(Ok(_))
    )
}

/* Runs `probe` for every host, PROBE_PARALLEL at a time, and returns what
 * the probes gave back (None = not this). */
pub async fn probe_all<T, F, Fut>(hosts: Vec<Host>, probe: F) -> Vec<T>
where
    F: Fn(Host) -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    use futures_util::stream::{self, StreamExt};
    stream::iter(hosts)
        .map(probe)
        .buffer_unordered(PROBE_PARALLEL)
        .filter_map(|result| async move { result })
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_are_normalized() {
        assert_eq!(normalize_mac("AA:BB:CC:DD:EE:0F").unwrap(), "aa:bb:cc:dd:ee:0f");
        assert_eq!(normalize_mac("8CBFEA9A1B2C").unwrap(), "8c:bf:ea:9a:1b:2c");
        assert_eq!(normalize_mac("8c-bf-ea-9a-1b-2c").unwrap(), "8c:bf:ea:9a:1b:2c");
        assert!(normalize_mac("shellyplugsg3-8cbfea9a1b2c").is_none());
        assert!(normalize_mac("1234").is_none());
    }

    #[test]
    fn arp_table_is_read() {
        let text = "IP address       HW type     Flags       HW address            Mask     Device\n\
                    192.168.1.1      0x1         0x2         7c:f1:7e:bc:73:c8     *        wlan0\n\
                    192.168.1.60     0x1         0x0         00:00:00:00:00:00     *        wlan0\n\
                    192.168.1.61     0x1         0x2         24:0a:c4:11:22:33     *        wlan0\n";
        let hosts = parse_arp(text);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[1].address, Ipv4Addr::new(192, 168, 1, 61));
        assert_eq!(hosts[1].manufacturer, "Espressif");
        assert_eq!(hosts[1].vars()["mac_hex"], "240ac4112233");
    }

    fn interface(name: &str, address: [u8; 4], netmask: [u8; 4]) -> network::Ipv4Interface {
        network::Ipv4Interface {
            name: name.into(),
            address: address.into(),
            netmask: netmask.into(),
        }
    }

    #[test]
    fn sweeps_only_the_own_bounded_subnet() {
        /* A /24: 254 hosts, minus the hub itself. */
        let targets = sweep_targets(&[interface("wlan0", [192, 168, 1, 141], [255, 255, 255, 0])]);
        assert_eq!(targets.len(), 253);
        assert!(!targets.contains(&Ipv4Addr::new(192, 168, 1, 141)));
        assert_eq!(targets[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(*targets.last().unwrap(), Ipv4Addr::new(192, 168, 1, 254));
        /* A /16 is too big: only the hub's own /24. The hotspot and
         * loopback: never. */
        let targets = sweep_targets(&[
            interface("eth0", [10, 0, 7, 9], [255, 255, 0, 0]),
            interface("uap0", [192, 168, 4, 1], [255, 255, 255, 0]),
            interface("lo", [127, 0, 0, 1], [255, 0, 0, 0]),
        ]);
        assert_eq!(targets.len(), 253);
        assert!(targets.iter().all(|t| t.octets()[..3] == [10, 0, 7]));
        /* A /22 is swept whole. */
        let targets = sweep_targets(&[interface("eth0", [10, 0, 4, 9], [255, 255, 252, 0])]);
        assert_eq!(targets.len(), 1021);
    }

    /* The real network (`cargo test live_scan -- --ignored --nocapture`):
     * who is there, with makers. */
    #[tokio::test]
    #[ignore]
    async fn live_scan() {
        let start = Instant::now();
        let hosts = Scanner::new().scan().await;
        for host in &hosts {
            println!("{:15} {} {:12} {}", host.address, host.mac, host.interface, host.manufacturer);
        }
        println!("{} hosts in {:.1} s", hosts.len(), start.elapsed().as_secs_f32());
    }

    #[tokio::test]
    async fn open_ports_are_found() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_open(Ipv4Addr::LOCALHOST, port).await);
        drop(listener);
        assert!(!port_open(Ipv4Addr::LOCALHOST, port).await);
    }
}
