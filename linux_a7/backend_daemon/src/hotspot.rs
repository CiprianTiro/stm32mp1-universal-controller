/*
 * hotspot.rs -- the setup hotspot (issue #36): the hub's own WiFi network
 * "UC-Setup-XXXX", for setting it up when it has no network at all (no
 * cable, no WiFi yet).
 *
 * HOW IT GOES, for the person setting the hub up:
 *   1. The hub has had no network for a while (see OPEN_WHEN_*) -- or someone
 *      tapped "Start setup hotspot" on the Network page. The hub opens
 *      "UC-Setup-XXXX", with a password that's NEW every time and shown only
 *      on the hub's screen (with a QR code a phone camera joins with).
 *   2. The phone joins it and opens http://192.168.4.1: a small page lists
 *      the WiFi networks the hub sees; pick one, type its password, Connect.
 *   3. The hub joins the chosen WiFi exactly like the touchscreen does it
 *      (network.rs). The chip has ONE radio, so it can run the hotspot and
 *      a WiFi connection at the same time only on the same channel:
 *        - the chosen network is on the hotspot's channel: the hotspot
 *          stays open during the attempt -- the phone never leaves, and the
 *          page shows the result after a few seconds; after a success the
 *          hotspot closes a moment later;
 *        - another channel: the hotspot closes for the attempt, and comes
 *          back with the SAME password if it failed (the phone may have to
 *          rejoin it).
 *      The "Joining..." page asks the hub every 2 s for the result
 *      (/status), and keeps asking while the phone is away, so the result
 *      shows the moment it's back.
 *
 * SECURITY. Joining the hotspot needs its password, which only the hub's
 * own screen shows -- the same "you're physically at the hub" proof as the
 * touchscreen itself, and the reason the setup page needs no login. The
 * page only answers phones ON the hotspot (192.168.4.0/24), not the LAN.
 * Phones on the hotspot can't see each other (ap_isolate). The hotspot
 * leads nowhere else: no router, no DNS (26-hotspot.network).
 *
 * WHO DOES WHAT: hub-hotspot.service (the hub-wifi recipe) creates the
 * access point interface (uap0) and runs hostapd; systemd-networkd gives
 * phones an address; this file writes hostapd's config, starts/stops the
 * service, serves the page and decides when to open the hotspot.
 */
use axum::{
    extract::{ConnectInfo, Form, State},
    response::Html,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::network;

const SERVICE: &str = "hub-hotspot.service";
const CONF_DIR: &str = "/run/hub-hotspot";
/* The hub's address on the hotspot (26-hotspot.network). */
const ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
/* How long the hub must have been without any network before the hotspot
 * opens by itself -- depending on whether something could still come:
 *   - no saved WiFi and no cable: nothing will come by itself (a new hub,
 *     or its WiFi was forgotten): open soon;
 *   - a WiFi is saved: most likely the router is still booting after a
 *     power cut (routers take 1-3 minutes, the hub ~16 s), so wait -- else
 *     every power cut would open a pointless setup hotspot.
 * (First version: one fixed 90 s after start for both. A real setup then
 * waited ~2 minutes for nothing, and a slow router could still trigger it.) */
const OPEN_WHEN_NOTHING_SAVED: Duration = Duration::from_secs(20);
const OPEN_WHEN_WIFI_SAVED: Duration = Duration::from_secs(180);
/* How often the automatic check looks. */
const AUTO_CHECK: Duration = Duration::from_secs(5);
/* The setup page shows the last scan's networks at once, and starts a new
 * scan in the background when they're older than this. */
const SCAN_MAX_AGE: Duration = Duration::from_secs(20);
/* The very first visit has no earlier scan: it waits at most this long. */
const FIRST_SCAN_WAIT: Duration = Duration::from_secs(8);
/* The channel when the hub isn't on any WiFi (the middle of the 2.4 GHz
 * band, legal everywhere). */
const DEFAULT_CHANNEL: u8 = 6;
/* The hotspot password's letters: lowercase and digits WITHOUT the easily
 * confused ones (l/1, o/0, i), since it may be read off the screen and
 * typed. 12 of them: ~59 bits, far beyond guessing in the minutes a
 * hotspot is open. */
const PASSWORD_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
const PASSWORD_LENGTH: usize = 12;

/* What the touchscreen (and, without the password, LAN clients) sees. */
#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct HotspotStatus {
    pub active: bool,
    pub ssid: String,
    /* Only ever sent to the hub's own screen (ws.rs removes it for LAN
     * clients). */
    pub password: Option<String>,
    pub url: String,
    /* Why the last attempt to join a network from the setup page failed. */
    pub last_error: Option<String>,
    /* A network chosen on the setup page is being joined right now. */
    pub connecting: bool,
    /* The network the setup page joined successfully (last attempt). */
    pub joined: Option<String>,
}

struct Inner {
    active: bool,
    ssid: String,
    password: String,
    last_error: Option<String>,
    connecting: bool,
    joined: Option<String>,
    /* Opened by the automatic check (then it also closes it again when a
     * network appears), rather than by a tap. */
    auto: bool,
    /* Closed by a tap: the automatic check leaves it closed until the next
     * start of the daemon. */
    closed_by_user: bool,
    /* The setup page's web server, while the hotspot is open. */
    server: Option<tokio::task::JoinHandle<()>>,
    /* The channel the hotspot runs on (see run_auto: it must follow the
     * hub's own WiFi connection). */
    channel: u8,
    /* The networks the setup page lists, and when they were scanned. */
    networks: Vec<network::Network>,
    scanned: Option<std::time::Instant>,
    scanning: bool,
}

pub struct Hotspot {
    inner: Mutex<Inner>,
    network_tx: mpsc::Sender<network::Cmd>,
}

impl Hotspot {
    pub fn new(network_tx: mpsc::Sender<network::Cmd>) -> Arc<Self> {
        Arc::new(Hotspot {
            inner: Mutex::new(Inner {
                active: false,
                ssid: ssid_for(&std::fs::read_to_string("/sys/class/net/wlan0/address").unwrap_or_default()),
                password: String::new(),
                last_error: None,
                connecting: false,
                joined: None,
                auto: false,
                closed_by_user: false,
                server: None,
                channel: DEFAULT_CHANNEL,
                networks: Vec::new(),
                scanned: None,
                scanning: false,
            }),
            network_tx,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        /* Never held across an .await; a poisoned lock's data is still
         * consistent (single assignments), so carry on with it. */
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn status(&self) -> HotspotStatus {
        let inner = self.lock();
        HotspotStatus {
            active: inner.active,
            ssid: inner.ssid.clone(),
            password: inner.active.then(|| inner.password.clone()),
            url: format!("http://{ADDRESS}"),
            last_error: inner.last_error.clone(),
            connecting: inner.connecting,
            joined: inner.joined.clone(),
        }
    }

    /* Opened by a tap on the touchscreen. */
    pub async fn open(self: &Arc<Self>) -> Result<HotspotStatus, String> {
        self.lock().closed_by_user = false;
        self.start(false, false, None).await
    }

    /* Closed by a tap on the touchscreen. */
    pub async fn close(self: &Arc<Self>) -> HotspotStatus {
        self.lock().closed_by_user = true;
        self.stop().await;
        self.status()
    }

    /* `auto`: opened by the automatic check. `keep_password`: reopening
     * after a failed attempt -- same password, so the phone rejoins by
     * itself. `channel`: the channel to use; None = the one the hub's WiFi
     * connection is on right now (or DEFAULT_CHANNEL without one).
     *
     * Why not a plain `async fn`: start -> the setup page -> join -> start
     * again (reopening after a failed attempt) is a loop, and Rust can't
     * work out the type of an async fn that ends up inside itself. A boxed
     * future ("some future, on the heap, that's Send") has a type that's
     * known up front, which breaks the loop. */
    /* The lifetime `'a` is written out: Yocto's Rust (1.75) can't infer it
     * for a `self: &Arc<Self>` method, newer versions can. */
    fn start<'a>(
        self: &'a Arc<Self>,
        auto: bool,
        keep_password: bool,
        channel: Option<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HotspotStatus, String>> + Send + 'a>> {
        Box::pin(async move {
            if self.lock().active {
                return Ok(self.status());
            }
            /* The chip has one radio: the hotspot must be on the channel
             * the hub's WiFi connection uses (if any), and follow the WiFi
             * country. */
            let net = ask(&self.network_tx, |reply| network::Cmd::GetStatus { reply }).await?;
            let channel = channel.or(net.wifi.channel).unwrap_or(DEFAULT_CHANNEL);
            let (ssid, password) = {
                let mut inner = self.lock();
                if !keep_password || inner.password.is_empty() {
                    inner.password = new_password();
                }
                (inner.ssid.clone(), inner.password.clone())
            };
            let conf = hostapd_conf(&ssid, &password, channel, net.wifi.country.as_deref());
            write_conf(&conf).map_err(|e| format!("hotspot configuration: {e}"))?;
            systemctl("start").await?;

            let server = tokio::spawn(serve_setup_page(self.clone()));
            {
                let mut inner = self.lock();
                inner.active = true;
                inner.auto = auto;
                inner.server = Some(server);
                inner.channel = channel;
            }
            println!(
                "hotspot: \"{ssid}\" open on channel {channel} ({})",
                if auto { "no network" } else { "opened on the touchscreen" }
            );
            Ok(self.status())
        })
    }

    async fn stop(&self) {
        let server = {
            let mut inner = self.lock();
            if !inner.active {
                return;
            }
            inner.active = false;
            inner.server.take()
        };
        if let Some(server) = server {
            server.abort();
        }
        if let Err(e) = systemctl("stop").await {
            println!("hotspot: {e}");
        }
        /* The config holds the password; don't leave it lying around. */
        let _ = std::fs::remove_file(format!("{CONF_DIR}/hostapd.conf"));
        println!("hotspot: closed");
    }

    /* A network was chosen on the setup page. Runs in its own task: the
     * page answers the phone first ("joining..."), then this tries the
     * network -- with the hotspot open if the network is on the hotspot's
     * channel, otherwise with it closed (see the header). */
    async fn join(self: Arc<Self>, ssid: String, password: String) {
        let stays_open = {
            let mut inner = self.lock();
            inner.connecting = true;
            inner.last_error = None;
            inner.joined = None;
            let target_channel = inner.networks.iter().find(|n| n.ssid == ssid).and_then(|n| n.channel);
            target_channel == Some(inner.channel)
        };
        /* Give the "joining..." page time to reach the phone. */
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !stays_open {
            self.stop().await;
        }
        let result = ask(&self.network_tx, |reply| network::Cmd::Connect {
            ssid: ssid.clone(),
            password: Some(password),
            reply,
        })
        .await
        .and_then(|r| r);
        match result {
            Ok(()) => {
                println!("hotspot: setup done, the hub joined \"{ssid}\"");
                {
                    let mut inner = self.lock();
                    inner.connecting = false;
                    inner.joined = Some(ssid);
                }
                if stays_open {
                    /* Let the page (asking every 2 s) show the success
                     * before the hotspot goes away. */
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    self.stop().await;
                }
            }
            Err(e) => {
                println!("hotspot: joining \"{ssid}\" from the setup page failed: {e}");
                {
                    let mut inner = self.lock();
                    inner.last_error = Some(format!("Could not join \"{ssid}\": {e}"));
                    inner.connecting = false;
                }
                if stays_open {
                    return; /* the phone never left: the page shows it */
                }
                /* If the hub had a WiFi before (still saved: a failed
                 * attempt doesn't replace it), it reconnects to it now.
                 * Wait for that, so the hotspot reopens on THAT channel --
                 * reopening at once picked the default channel while the
                 * WiFi was still reconnecting, and then the two were on
                 * different channels (seen on the DK2). */
                let mut channel = None;
                for _ in 0..20 {
                    let Ok(net) = ask(&self.network_tx, |reply| network::Cmd::GetStatus { reply }).await else {
                        break;
                    };
                    if net.wifi.connected || net.wifi.saved_ssid.is_none() {
                        channel = net.wifi.channel;
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                let auto = self.lock().auto;
                if let Err(e) = self.start(auto, true, channel).await {
                    println!("hotspot: could not reopen: {e}");
                }
            }
        }
    }
}

/* The automatic part: opens the hotspot when the hub has had no network at
 * all for long enough (see OPEN_WHEN_*), closes a hotspot it opened itself
 * once a network is there (e.g. a cable was plugged in), and keeps the
 * hotspot on the WiFi's channel. A hotspot closed by a tap stays closed. */
pub async fn run_auto(hotspot: Arc<Hotspot>) {
    let mut tick = tokio::time::interval(AUTO_CHECK);
    /* Since when the hub has been without any network (continuously: the
     * clock restarts whenever a network appears). */
    let mut offline_since: Option<std::time::Instant> = None;
    loop {
        tick.tick().await;
        let Ok(net) = ask(&hotspot.network_tx, |reply| network::Cmd::GetStatus { reply }).await else {
            return; /* shutting down */
        };
        let offline = net.uplink.is_none() && !net.ethernet.connected && !net.wifi.connected;
        if !offline {
            offline_since = None;
        } else if offline_since.is_none() {
            offline_since = Some(std::time::Instant::now());
        }
        let wait = if net.wifi.saved_ssid.is_some() {
            OPEN_WHEN_WIFI_SAVED
        } else {
            OPEN_WHEN_NOTHING_SAVED
        };
        let waited_enough = offline_since.is_some_and(|since| since.elapsed() >= wait);
        let (active, auto, closed_by_user, connecting) = {
            let inner = hotspot.lock();
            (inner.active, inner.auto, inner.closed_by_user, inner.connecting)
        };
        if connecting {
            continue;
        }
        if waited_enough && !active && !closed_by_user {
            if let Err(e) = hotspot.start(true, false, None).await {
                println!("hotspot: could not open: {e}");
            }
        } else if !offline && active && auto {
            println!("hotspot: the hub has a network now");
            hotspot.stop().await;
        } else if active {
            /* The chip can only be on ONE channel. If the hub's WiFi
             * connection is on another one than the hotspot -- e.g. the
             * hotspot reopened while the WiFi was briefly disconnected
             * after a failed attempt, then the WiFi came back (seen on the
             * DK2: hotspot on 6, WiFi on 3, and the hotspot stopped passing
             * any traffic) -- move the hotspot over. Same name and
             * password: the phone rejoins by itself. */
            let ap_channel = hotspot.lock().channel;
            if let Some(wifi_channel) = net.wifi.channel.filter(|c| *c != ap_channel) {
                println!("hotspot: the WiFi is on channel {wifi_channel}, moving the hotspot there from {ap_channel}");
                hotspot.stop().await;
                /* The channel is passed on explicitly: removing the
                 * hotspot's interface briefly knocks the hub's own WiFi
                 * connection off, so asking "which channel is the WiFi on?"
                 * right now would get no answer -- and the default channel
                 * again (seen on the DK2). */
                if let Err(e) = hotspot.start(auto, true, Some(wifi_channel)).await {
                    println!("hotspot: could not reopen: {e}");
                }
            }
        }
    }
}

/* ------------------------------------------------------------------ */
/* The setup page                                                      */
/* ------------------------------------------------------------------ */

/* Serves the page on 192.168.4.1:80 while the hotspot is open (the task
 * is aborted when it closes). The address only exists once the hotspot's
 * interface is up and configured, so binding is retried for a while. */
async fn serve_setup_page(hotspot: Arc<Hotspot>) {
    let addr = SocketAddr::from((ADDRESS, 80));
    let mut listener = None;
    for _ in 0..40 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    let Some(listener) = listener else {
        println!("hotspot: setup page not available ({addr} never appeared)");
        return;
    };
    /* Every address shows the setup page, except the form's POST to
     * /connect. Found on the DK2: after a failed attempt the phone's
     * address bar still said /connect; reloading it sent a GET there, the
     * reply was an empty "method not allowed", and the phone offered it as
     * a file download ("connect.txt"). */
    let app = Router::new()
        .route("/connect", get(page).post(connect))
        .route("/status", get(status_json))
        .fallback(page)
        .with_state(hotspot);
    if let Err(e) = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await {
        println!("hotspot: setup page stopped: {e}");
    }
}

/* Only phones on the hotspot itself. (Linux would also accept a packet
 * for 192.168.4.1 arriving from the LAN, if someone there routed it to
 * the hub -- but the answers would go out over the hotspot, so such a
 * connection can't even be established; this check says so explicitly.) */
fn on_hotspot(peer: &SocketAddr) -> bool {
    match peer.ip().to_canonical() {
        std::net::IpAddr::V4(ip) => ip.octets()[..3] == ADDRESS.octets()[..3],
        _ => false,
    }
}

async fn page(ConnectInfo(peer): ConnectInfo<SocketAddr>, State(hotspot): State<Arc<Hotspot>>) -> Html<String> {
    if !on_hotspot(&peer) {
        return Html(page_html("", &[], None, Some("Only available on the hub's setup hotspot.")));
    }
    let networks = hotspot.networks_for_page().await;
    let status = hotspot.status();
    Html(page_html(&status.ssid, &networks, status.last_error.as_deref(), None))
}

impl Hotspot {
    /* The networks for the setup page, without making the phone wait for
     * a scan: the last results at once (a new scan runs in the background
     * if they're old), and only the very first visit waits -- at most
     * FIRST_SCAN_WAIT. (Waiting for every scan made the page hang white
     * whenever the radio was busy; seen on the DK2.) */
    async fn networks_for_page(self: &Arc<Self>) -> Vec<network::Network> {
        let (cached, stale, first) = {
            let inner = self.lock();
            let stale = inner.scanned.map_or(true, |t| t.elapsed() >= SCAN_MAX_AGE);
            (inner.networks.clone(), stale, inner.scanned.is_none())
        };
        if stale {
            let scan = tokio::spawn(self.clone().rescan());
            if first {
                let _ = tokio::time::timeout(FIRST_SCAN_WAIT, scan).await;
                return self.lock().networks.clone();
            }
        }
        cached
    }

    /* One scan (unless one is already running), stored for the page. */
    async fn rescan(self: Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.scanning {
                return;
            }
            inner.scanning = true;
        }
        let result = ask(&self.network_tx, |reply| network::Cmd::Scan { reply }).await.and_then(|r| r);
        let mut inner = self.lock();
        inner.scanning = false;
        if let Ok(networks) = result {
            inner.networks = networks;
            inner.scanned = Some(std::time::Instant::now());
        }
    }
}

/* What the "joining..." page's script asks every 2 s. */
async fn status_json(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(hotspot): State<Arc<Hotspot>>,
) -> axum::Json<serde_json::Value> {
    if !on_hotspot(&peer) {
        return axum::Json(serde_json::json!({}));
    }
    let status = hotspot.status();
    axum::Json(serde_json::json!({
        "connecting": status.connecting,
        "error": status.last_error,
        "joined": status.joined,
    }))
}

#[derive(Deserialize)]
struct ConnectForm {
    /* The network picked in the list ("" = none picked). */
    #[serde(default)]
    ssid: String,
    /* Typed by hand (a hidden network, or one the list doesn't show). */
    #[serde(default)]
    other: String,
    #[serde(default)]
    password: String,
}

async fn connect(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(hotspot): State<Arc<Hotspot>>,
    Form(form): Form<ConnectForm>,
) -> Html<String> {
    if !on_hotspot(&peer) {
        return Html(page_html("", &[], None, Some("Only available on the hub's setup hotspot.")));
    }
    let ssid = if form.other.trim().is_empty() { form.ssid } else { form.other.trim().to_string() };
    if ssid.is_empty() {
        let status = hotspot.status();
        return Html(page_html(&status.ssid, &[], None, Some("Pick a network first.")));
    }
    let name = hotspot.status().ssid;
    tokio::spawn(hotspot.join(ssid.clone(), form.password));
    Html(connecting_html(&ssid, &name))
}

/* ---- HTML: one self-contained page, no scripts, no outside files ---- */

const STYLE: &str = "body{font-family:sans-serif;background:#0F172A;color:#E2E8F0;margin:0;padding:24px;max-width:480px}\
h1{font-size:22px}p{color:#94A3B8}label{display:block;margin:14px 0 6px}\
select,input,button{width:100%;box-sizing:border-box;font-size:18px;padding:12px;border-radius:8px;border:0}\
select,input{background:#1E293B;color:#E2E8F0}button{background:#38BDF8;color:#0F172A;margin-top:20px}\
.error{color:#F87171}";

fn page_html(hotspot_ssid: &str, networks: &[network::Network], last_error: Option<&str>, notice: Option<&str>) -> String {
    let mut options = String::new();
    for n in networks {
        options.push_str(&format!(
            "<option value=\"{v}\">{v}{lock}</option>",
            v = escape(&n.ssid),
            lock = if n.secure { "" } else { " (open)" }
        ));
    }
    let error = last_error
        .map(|e| format!("<p class=\"error\">{}</p>", escape(e)))
        .unwrap_or_default();
    let notice = notice.map(|n| format!("<p class=\"error\">{}</p>", escape(n))).unwrap_or_default();
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>Hub setup</title><style>{STYLE}</style></head><body>\
<h1>Universal Controller setup</h1>\
<p>Choose the WiFi the hub should use. The hub then leaves \"{ssid}\" and joins it.</p>\
{notice}{error}\
<form method=\"post\" action=\"/connect\">\
<label for=\"ssid\">Network</label><select id=\"ssid\" name=\"ssid\"><option value=\"\">Choose...</option>{options}</select>\
<label for=\"other\">...or type its name (hidden network)</label><input id=\"other\" name=\"other\" autocomplete=\"off\">\
<label for=\"password\">Password</label><input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"off\">\
<button type=\"submit\">Connect</button></form></body></html>",
        ssid = escape(hotspot_ssid),
    )
}

fn connecting_html(target: &str, hotspot_ssid: &str) -> String {
    /* Two ways the page learns the result:
     *  - a small script asks /status every 2 s and shows the result -- it
     *    keeps asking while the phone is off the hotspot (a failed request
     *    is simply tried again), so the result shows the moment it's back;
     *  - without JavaScript, the page reloads the main page every 15 s
     *    ("refresh"; a browser stops that after a failed load, hence the
     *    script as the main way). */
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<noscript><meta http-equiv=\"refresh\" content=\"15;url=/\"></noscript>\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>Hub setup</title><style>{STYLE}</style></head><body>\
<h1 id=\"title\">Joining \"{t}\"...</h1>\
<p id=\"info\">This takes a few seconds. If \"{h}\" has to close for it and your phone \
leaves it: join \"{h}\" again, this page then shows the result.</p>\
<script>\
function check(){{fetch('/status',{{cache:'no-store'}}).then(function(r){{return r.json()}}).then(function(s){{\
if(s.connecting){{setTimeout(check,2000);return}}\
if(s.joined){{document.getElementById('title').textContent='Done: the hub is on the new WiFi.';\
document.getElementById('info').textContent='The setup hotspot closes now. You can go back to your usual WiFi.';return}}\
location.href='/';\
}}).catch(function(){{setTimeout(check,2000)}})}}\
setTimeout(check,2000);\
</script></body></html>",
        t = escape(target),
        h = escape(hotspot_ssid),
    )
}

/* Text -> safe inside HTML. A network name is chosen by whoever runs that
 * network, so a name like <script>... must show as text, never run. */
fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */

/* "UC-Setup-" + the last 4 hex digits of the WiFi chip's MAC address:
 * recognisable, and different for every hub. */
fn ssid_for(mac: &str) -> String {
    let hex: String = mac.trim().chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let tail = if hex.len() >= 4 { &hex[hex.len() - 4..] } else { "0000" };
    format!("UC-Setup-{}", tail.to_uppercase())
}

fn new_password() -> String {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut password = String::with_capacity(PASSWORD_LENGTH);
    /* Rejection sampling (see auth.rs's codes): only bytes below the last
     * whole multiple of the alphabet's size are used, so every letter is
     * equally likely. */
    let limit = 256 - (256 % PASSWORD_ALPHABET.len());
    while password.len() < PASSWORD_LENGTH {
        let mut byte = [0u8; 1];
        rng.fill(&mut byte).expect("system random generator failed");
        if usize::from(byte[0]) < limit {
            password.push(char::from(PASSWORD_ALPHABET[usize::from(byte[0]) % PASSWORD_ALPHABET.len()]));
        }
    }
    password
}

/* hostapd's configuration for the hotspot. */
fn hostapd_conf(ssid: &str, password: &str, channel: u8, country: Option<&str>) -> String {
    let mut conf = format!(
        "interface=uap0\n\
driver=nl80211\n\
ssid={ssid}\n\
hw_mode=g\n\
channel={channel}\n\
wpa=2\n\
wpa_key_mgmt=WPA-PSK\n\
rsn_pairwise=CCMP\n\
wpa_passphrase={password}\n\
ap_isolate=1\n\
max_num_sta=4\n"
    );
    if let Some(country) = country {
        conf.push_str(&format!("country_code={country}\n"));
    }
    conf
}

/* In /run (RAM, gone at reboot), readable by root only: it holds the
 * password. */
fn write_conf(conf: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(CONF_DIR)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(format!("{CONF_DIR}/hostapd.conf"))?;
    file.write_all(conf.as_bytes())
}

/* `systemctl start|stop hub-hotspot.service`, run directly (no shell), on
 * Tokio's thread pool for blocking work. */
async fn systemctl(action: &'static str) -> Result<(), String> {
    let output = tokio::task::spawn_blocking(move || std::process::Command::new("systemctl").args([action, SERVICE]).output())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("systemctl: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl {action} {SERVICE} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

async fn ask<T>(
    network_tx: &mpsc::Sender<network::Cmd>,
    make: impl FnOnce(oneshot::Sender<T>) -> network::Cmd,
) -> Result<T, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    network_tx
        .send(make(reply_tx))
        .await
        .map_err(|_| "network actor unavailable".to_string())?;
    reply_rx.await.map_err(|_| "network actor dropped the reply".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssid_from_mac() {
        assert_eq!(ssid_for("7c:b8:da:de:46:d6\n"), "UC-Setup-46D6");
        assert_eq!(ssid_for(""), "UC-Setup-0000");
    }

    #[test]
    fn passwords() {
        let a = new_password();
        assert_eq!(a.len(), PASSWORD_LENGTH);
        assert!(a.bytes().all(|b| PASSWORD_ALPHABET.contains(&b)));
        /* No look-alike characters. */
        assert!(!a.contains(['l', '1', 'o', '0', 'i']));
        assert_ne!(a, new_password());
    }

    #[test]
    fn hostapd_config() {
        let conf = hostapd_conf("UC-Setup-46D6", "abcdefgh2345", 3, Some("RO"));
        assert!(conf.contains("interface=uap0\n"));
        assert!(conf.contains("ssid=UC-Setup-46D6\n"));
        assert!(conf.contains("channel=3\n"));
        assert!(conf.contains("wpa=2\n"));
        assert!(conf.contains("wpa_passphrase=abcdefgh2345\n"));
        assert!(conf.contains("ap_isolate=1\n"));
        assert!(conf.contains("country_code=RO\n"));
        assert!(!hostapd_conf("x", "y", 6, None).contains("country_code"));
    }

    #[test]
    fn html_escapes_network_names() {
        assert_eq!(escape("<b>\"x\" & 'y'</b>"), "&lt;b&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/b&gt;");
        let evil = network::Network {
            ssid: "<script>alert(1)</script>".into(),
            signal_dbm: -40,
            bars: 4,
            secure: true,
            saved: false,
            channel: Some(3),
        };
        let html = page_html("UC-Setup-46D6", &[evil], Some("bad & worse"), None);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("bad &amp; worse"));
    }

    #[test]
    fn only_hotspot_addresses() {
        assert!(on_hotspot(&"192.168.4.23:5000".parse().unwrap()));
        assert!(!on_hotspot(&"192.168.1.141:5000".parse().unwrap()));
        assert!(!on_hotspot(&"127.0.0.1:5000".parse().unwrap()));
    }
}
