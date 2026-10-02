/*
 * ws.rs -- the local WebSocket API, ws://<hub>:8080/ws: what the touchscreen
 * UI, the phone app and tools/hub_ws.py talk to.
 *
 * TWO DOORS (issue #35):
 *   ws://127.0.0.1:8080/ws   the hub's own touchscreen UI (and other
 *                            programs ON the hub). Only reachable from the
 *                            hub itself -- not from the LAN at all -- and
 *                            trusted: everything is allowed.
 *   wss://<hub>:8443/ws      the LAN (phone app, tools/hub_ws.py): TLS with
 *                            the hub's own certificate (tls.rs), and nothing
 *                            is allowed until the client has proven who it
 *                            is: `pair` once with the code from the hub's
 *                            screen, `auth` with its key afterwards
 *                            (auth.rs). Only hello/pair/auth work before.
 *
 * PROTOCOL v2 (issue #34). JSON text messages. The client sends requests,
 * {"action": "...", ...}; the server answers each one, in order, with
 * {"type": "...", ...}. After "subscribe", the server additionally PUSHES
 * events whenever a device changes, so a client never has to poll. Replies
 * and events are told apart by their "type".
 *
 *   hello                                  -> hello {protocol, capabilities}
 *   list_devices                           -> devices {devices: [...]}
 *   get_device {id}                        -> device {device} (null: unknown)
 *   add_device {device}                    -> device {device}
 *   update_device_info {id, name?, room?, favourite?}
 *                                          -> device {device} (favourite: #95)
 *   remove_device {id}                     -> ack
 *   command {id, capability, value}        -> device {device}
 *   device_action {id, capability, name, args?}
 *                                          -> action_result {result} (issue #44:
 *                                             press a remote button, list channels...)
 *        e.g. {"action":"command","id":"lamp-1","capability":"dimmer",
 *              "value":{"level":30}}
 *   subscribe                              -> ack, then events:
 *        device_changed {device} / device_removed {id} /
 *        events_lost (too slow to keep up: list_devices again)
 *   get_network_status, wifi_scan, wifi_connect, wifi_forget,
 *   set_wifi_country                       -> see network.rs (#61)
 *   get_settings                           -> settings {mode, accent, density, time_zone,
 *                                             latitude?, longitude?}
 *   set_settings {mode?, accent?, density?, time_zone?, latitude?, longitude?}
 *                                          -> settings {...} (see settings.rs, #39, #47)
 *        after subscribe also: settings_changed {...}
 * Scenes and automations (issue #47, automations.rs):
 *   list_automations                       -> automations {scenes, automations}
 *   save_scene {scene} / delete_scene {id} -> automations {...}
 *   capture_scene {name, devices: [ids]}   -> automations {...} ("save current state")
 *   save_automation {automation} / delete_automation {id}
 *   set_automation_enabled {id, enabled}   -> automations {...}
 *   run_scene {id}, run_automation {id}    -> ack once done (error: what failed)
 *   get_automation_log                     -> automation_log {entries}
 *   webhook_url {id}                       -> webhook {url} (issue #72: the device's
 *                                             secret address, made the first time)
 *        after subscribe also: automations_changed (list again),
 *        automation_ran {entry}
 *   list_found                             -> found {devices: [...]} (the "Found on
 *                                             your network" inbox, discovery.rs, #40)
 *   discover_now                           -> ack (a search round now)
 *        after subscribe also: found_changed (list_found again)
 * Adding a device, the wizard (issue #40, wizard.rs -- one per connection):
 *   list_templates                         -> templates {templates: [{id, name,
 *                                             category, description, variants}]}
 *   wizard_start {template, variant?, found?}
 *                                          -> wizard_step {session, template, number,
 *                                             step: "info"|"discover"|"form"|..., ...}
 *        found: an address from list_found (that device, already chosen)
 *   wizard_answer {session, values: {...}} -> the next wizard_step
 *   wizard_back {session}                  -> the previous wizard_step
 *   wizard_finish {session, name, room}    -> device {device} (added, running)
 *   wizard_cancel {session}                -> ack
 *   wizard_reauth {device}                 -> wizard_step: "pair again" (a device
 *                                             shown online: "unauthorized")
 *   wizard_reconfigure {device}            -> wizard_step: "change settings"
 *        these end with step "save": wizard_finish {session} (no name/room)
 *        -> device {device}, same id, adapter restarted
 *        a refused answer, or an ended session:
 *                                          -> wizard_error {session, field?, message, detail?}
 * On the LAN door (#35):
 *   pair {code, client_name}               -> paired {client_id, token, hub_fingerprint}
 *   auth {token}                           -> authenticated {client_id, name}
 * Only on the hub's own door (#35):
 *   start_pairing / pairing_status         -> pairing {state, code, ...}
 *   cancel_pairing                         -> ack
 *   list_clients                           -> clients {clients: [...]}
 *   revoke_client {id}                     -> ack
 *   start_hotspot / stop_hotspot           -> hotspot {active, ssid, password, ...} (#36)
 * get_network_status's reply also carries the setup hotspot's state; its
 * password only on the hub's own door.
 *   anything refused                       -> error {message}
 *
 * A device is device.rs's JSON: {"id", "name", "room", "template", "source",
 * "capabilities": {"switch": {"on": true}, ...}}. Every command goes
 * through control.rs, the same door the cloud uses.
 *
 * The full reference with examples is on the wiki (Device-Model page) --
 * it's the contract the Flutter app (#48) is built against.
 */
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Extension, State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use futures_util::stream::{FuturesOrdered, StreamExt};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::auth::{self, Auth};
use crate::automations::{self, Automations};
use crate::webhooks::Webhooks;
use crate::control::Control;
use crate::hotspot::{self, Hotspot};
use crate::discovery::{self, Discovery};
use crate::settings::{self, HubSettings, Settings};
use crate::device::{self, Device};
use crate::network;
use crate::state::{DeviceId, Event};
use crate::templates::Templates;
use crate::tls;
use crate::wizard;

/* The two doors (see the header). */
const LOCAL_ADDR: &str = "127.0.0.1:8080";
const LAN_ADDR: &str = "0.0.0.0:8443";

/* A LAN client has this long to pair or authenticate, then it's dropped:
 * no idle, anonymous connections kept open. */
const AUTH_DEADLINE: Duration = Duration::from_secs(30);
/* Every failed pair/auth answer is held back this long, which makes
 * guessing slow even within the other limits. */
const FAILURE_DELAY: Duration = Duration::from_secs(1);
/* Failed pair/auth attempts on one connection before it's closed. */
const MAX_FAILURES: u32 = 3;

/* The protocol version "hello" reports. v1 (before #34) had property bags
 * and LED-specific actions; clients check this to know what they talk to. */
const PROTOCOL_VERSION: u32 = 2;

/* What a client may ask. `#[serde(tag = "action", rename_all =
 * "snake_case")]`: the JSON's "action" field picks the variant, e.g.
 * {"action": "get_device", "id": "lamp-1"}. An unknown action or a missing
 * field is refused with serde's own (quite clear) message. */
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
// AddDevice carries a whole Device, the rest a few strings: clippy calls
// the size difference wasteful, but a request is parsed once and handled
// at once -- boxing would only make the code noisier (as in state.rs).
#[allow(clippy::large_enum_variant)]
enum ClientRequest {
    Hello,
    ListDevices,
    GetDevice {
        id: DeviceId,
    },
    AddDevice {
        device: Device,
    },
    UpdateDeviceInfo {
        id: DeviceId,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        room: Option<String>,
        #[serde(default)]
        favourite: Option<bool>,
    },
    RemoveDevice {
        id: DeviceId,
    },
    Command {
        id: DeviceId,
        capability: String,
        value: serde_json::Value,
    },
    /* Issue #44: a one-off action (device::check_action). */
    DeviceAction {
        id: DeviceId,
        capability: String,
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    Subscribe,
    /* Issue #35: pairing and logging in (LAN door). */
    Pair {
        code: String,
        client_name: String,
    },
    Auth {
        token: String,
    },
    /* Issue #35: managing pairing (hub's own door only). */
    StartPairing,
    PairingStatus,
    CancelPairing,
    ListClients,
    RevokeClient {
        id: String,
    },
    /* Issue #36: the setup hotspot (hub's own door only). */
    StartHotspot,
    StopHotspot,
    /* Network (issue #61), answered by network.rs. Reading the status is
     * allowed for every client; everything else only for clients on the
     * hub itself -- see local_only below. */
    GetNetworkStatus,
    WifiScan,
    WifiConnect {
        ssid: String,
        /* Omitted or empty for an open network. */
        #[serde(default)]
        password: Option<String>,
    },
    WifiForget,
    SetWifiCountry {
        country: String,
    },
    /* The hub's settings (issue #39, settings.rs): allowed for every
     * client, they only change how things look. */
    /* The inbox of devices found on the LAN (issue #40, discovery.rs). */
    ListFound,
    DiscoverNow,
    /* Adding a device (issue #40, wizard.rs). */
    ListTemplates,
    WizardStart {
        template: String,
        #[serde(default)]
        variant: Option<String>,
        #[serde(default)]
        found: Option<String>,
    },
    WizardAnswer {
        session: String,
        #[serde(default)]
        values: serde_json::Map<String, serde_json::Value>,
    },
    WizardBack {
        session: String,
    },
    WizardFinish {
        session: String,
        /* Not needed after "pair again" / "change settings". */
        #[serde(default)]
        name: String,
        #[serde(default)]
        room: String,
    },
    WizardReauth {
        device: DeviceId,
    },
    WizardReconfigure {
        device: DeviceId,
    },
    WizardCancel {
        session: String,
    },
    GetSettings,
    SetSettings {
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        accent: Option<String>,
        #[serde(default)]
        density: Option<String>,
        #[serde(default)]
        time_zone: Option<String>,
        /* Issue #47: the hub's location, for sunrise and sunset. */
        #[serde(default)]
        latitude: Option<f64>,
        #[serde(default)]
        longitude: Option<f64>,
    },
    /* Scenes and automations (issue #47, automations.rs). */
    ListAutomations,
    SaveScene {
        scene: automations::Scene,
    },
    DeleteScene {
        id: String,
    },
    CaptureScene {
        name: String,
        devices: Vec<DeviceId>,
    },
    RunScene {
        id: String,
    },
    SaveAutomation {
        automation: automations::Automation,
    },
    DeleteAutomation {
        id: String,
    },
    SetAutomationEnabled {
        id: String,
        enabled: bool,
    },
    RunAutomation {
        id: String,
    },
    GetAutomationLog,
    /* Issue #72: a device's secret webhook address. */
    WebhookUrl {
        id: DeviceId,
    },
}

impl ClientRequest {
    /* Only through the hub's own door (127.0.0.1): changing the hub's
     * network, listing the neighbours' networks (#61), and managing who is
     * paired (#35) -- none of that should be possible from the LAN, not
     * even for a paired phone, until each gets its own permission model. */
    fn local_only(&self) -> bool {
        matches!(
            self,
            ClientRequest::WifiScan
                | ClientRequest::WifiConnect { .. }
                | ClientRequest::WifiForget
                | ClientRequest::SetWifiCountry { .. }
                | ClientRequest::StartPairing
                | ClientRequest::PairingStatus
                | ClientRequest::CancelPairing
                | ClientRequest::ListClients
                | ClientRequest::RevokeClient { .. }
                | ClientRequest::StartHotspot
                | ClientRequest::StopHotspot
        )
    }

    /* What a LAN client may send before it's authenticated. */
    fn allowed_anonymously(&self) -> bool {
        matches!(self, ClientRequest::Hello | ClientRequest::Pair { .. } | ClientRequest::Auth { .. })
    }
}

/* What the server sends: replies and (after subscribe) events. */
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Hello {
        protocol: u32,
        /* The capability names this hub knows (device.rs). */
        capabilities: Vec<&'static str>,
        /* false: pair or auth first (LAN door, #35). */
        authenticated: bool,
    },
    Paired {
        client_id: String,
        token: String,
        /* The certificate the client should pin from now on (tls.rs). */
        hub_fingerprint: String,
    },
    Authenticated {
        client_id: String,
        name: String,
    },
    Pairing {
        #[serde(flatten)]
        status: auth::PairingStatus,
        /* What the pairing screen shows next to the code (and puts in the
         * QR code): where to connect, and the certificate's fingerprint. */
        addresses: Vec<String>,
        port: u16,
        fingerprint: String,
        /* The first 16 hex digits, grouped, for comparing by eye. */
        fingerprint_short: String,
    },
    Clients {
        clients: Vec<auth::ClientInfo>,
    },
    Devices {
        devices: Vec<Device>,
    },
    Device {
        device: Option<Device>,
    },
    ActionResult {
        result: serde_json::Value,
    },
    Ack,
    NetworkStatus {
        status: network::Status,
        /* The setup hotspot (#36). */
        hotspot: hotspot::HotspotStatus,
    },
    Hotspot {
        #[serde(flatten)]
        status: hotspot::HotspotStatus,
    },
    WifiNetworks {
        networks: Vec<network::Network>,
    },
    Settings {
        #[serde(flatten)]
        settings: HubSettings,
    },
    Found {
        devices: Vec<discovery::Found>,
    },
    Templates {
        templates: Vec<wizard::TemplateInfo>,
    },
    WizardStep {
        #[serde(flatten)]
        step: wizard::StepView,
    },
    WizardError {
        session: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        field: Option<String>,
        message: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        detail: String,
    },
    /* Events. */
    DeviceChanged {
        device: Device,
    },
    DeviceRemoved {
        id: DeviceId,
    },
    EventsLost,
    SettingsChanged {
        #[serde(flatten)]
        settings: HubSettings,
    },
    FoundChanged,
    /* Issue #47. */
    Automations {
        #[serde(flatten)]
        book: automations::Book,
    },
    AutomationLog {
        entries: Vec<automations::LogEntry>,
    },
    AutomationsChanged,
    AutomationRan {
        entry: automations::LogEntry,
    },
    Webhook {
        url: String,
    },
    Error {
        message: String,
    },
}

/* Axum's `State` extractor accepts ONE state type per router -- this struct
 * bundles what every connection needs. Cloning is cheap (channel handles
 * and Arcs); Axum clones it for every connection. */
#[derive(Clone)]
struct AppState {
    control: Control,
    auth: Arc<Auth>,
    hotspot: Arc<Hotspot>,
    /* The hub certificate's fingerprint ("" if the LAN door is off). */
    fingerprint: Arc<String>,
    network_tx: mpsc::Sender<network::Cmd>,
    /* To subscribe a client to state.rs's device events. */
    events_tx: broadcast::Sender<Event>,
    /* How many clients are connected right now (issue #29) -- shared with
     * health.rs, which reports it to the cloud as "local_clients". */
    local_clients: Arc<AtomicUsize>,
    /* The hub's settings (issue #39). */
    settings: Arc<Settings>,
    /* Devices found on the LAN (issue #40). */
    discovery: Arc<Discovery>,
    /* What the wizard can add (issue #40). */
    templates: Arc<Templates>,
    /* Scenes and automations (issue #47). */
    automations: Arc<Automations>,
    /* Devices calling the hub (issue #72). */
    webhooks: Arc<Webhooks>,
}

/* Counts one connected client for as long as it exists: +1 when created,
 * -1 when dropped. Tying the -1 to `Drop` (Rust runs it automatically when
 * the value goes out of scope) means it happens however the connection
 * ends -- clean close, network error, or the task being cancelled -- with
 * no way to forget it on some early-return path. */
struct ClientCount(Arc<AtomicUsize>);

impl ClientCount {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        ClientCount(counter.clone())
    }
}

impl Drop for ClientCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/* Which door a connection came through (see the header). */
#[derive(Clone, Copy, PartialEq, Debug)]
enum Door {
    Local,
    Lan,
}

/* Entry point, spawned once from main(). `identity`: the hub's TLS
 * identity; None = the LAN door stays closed (the local one still works). */
#[allow(clippy::too_many_arguments)] /* all distinct handles; a struct would just rename them */
pub async fn run(
    control: Control,
    auth: Arc<Auth>,
    hotspot: Arc<Hotspot>,
    identity: Option<tls::Identity>,
    network_tx: mpsc::Sender<network::Cmd>,
    events_tx: broadcast::Sender<Event>,
    local_clients: Arc<AtomicUsize>,
    settings: Arc<Settings>,
    discovery: Arc<Discovery>,
    templates: Arc<Templates>,
    automations: Arc<Automations>,
    webhooks: Arc<Webhooks>,
) {
    let fingerprint = Arc::new(identity.as_ref().map(|i| i.fingerprint.clone()).unwrap_or_default());
    let state = AppState {
        control,
        auth,
        hotspot,
        fingerprint,
        network_tx,
        events_tx,
        local_clients,
        settings,
        discovery,
        templates,
        automations,
        webhooks,
    };
    /* The same routes behind both doors; `Extension(Door)` tells the
     * handler which one a connection used. */
    let router = |door: Door| {
        Router::new()
            .route("/ws", get(ws_handler))
            .layer(Extension(door))
            .with_state(state.clone())
    };

    if let Some(identity) = identity {
        match tokio::net::TcpListener::bind(LAN_ADDR).await {
            Ok(listener) => {
                println!("ws.rs listening on wss://{LAN_ADDR} (LAN, pairing required)");
                let acceptor = tokio_rustls::TlsAcceptor::from(identity.config);
                tokio::spawn(serve_tls(listener, acceptor, router(Door::Lan)));
            }
            Err(e) => println!("ws.rs: LAN door closed, can't listen on {LAN_ADDR}: {e}"),
        }
    }

    let listener = tokio::net::TcpListener::bind(LOCAL_ADDR)
        .await
        .expect("failed to bind the local WebSocket listener");
    println!("ws.rs listening on ws://{LOCAL_ADDR} (this hub only)");
    /* `into_make_service_with_connect_info`: makes each connection's
     * address available to ws_handler (ConnectInfo). */
    axum::serve(listener, router(Door::Local).into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("WebSocket server crashed");
}

/* The LAN door: accepts TCP connections, wraps each one in TLS, and hands
 * it to the same axum router (through hyper, which axum is built on --
 * axum::serve itself only does plain TCP). One task per connection, so a
 * slow or stuck TLS handshake never holds up the others. */
async fn serve_tls(listener: tokio::net::TcpListener, acceptor: tokio_rustls::TlsAcceptor, router: Router) {
    /* A handshake that doesn't finish in this time is dropped (a
     * half-open connection shouldn't tie up a task forever). */
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                /* e.g. too many open files: wait a moment, don't spin. */
                println!("ws.rs: LAN accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            /* A failed handshake is usually a scanner or a client that
             * doesn't trust our certificate yet: not logged, it's noise. */
            let Ok(Ok(tls)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await else {
                return;
            };
            let service = hyper::service::service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
                /* What axum::serve's connect-info does for the local door. */
                request.extensions_mut().insert(ConnectInfo(peer));
                let mut router = router.clone();
                async move {
                    use tower_service::Service;
                    router.call(request.map(axum::body::Body::new)).await
                }
            });
            let connection = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                /* Lets the connection be taken over by the WebSocket after
                 * the HTTP "upgrade" handshake. */
                .with_upgrades();
            let _ = connection.await;
        });
    }
}

/* Runs once per incoming HTTP request to /ws: completes the WebSocket
 * handshake and hands the connection to handle_socket. */
async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(door): Extension<Door>,
    State(app_state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, app_state, door, peer))
}

/* Who is on the other end of one connection. */
struct Session {
    door: Door,
    peer: SocketAddr,
    /* The paired client, once authenticated (LAN door). */
    client: Option<auth::ClientInfo>,
    failures: u32,
}

impl Session {
    fn trusted(&self) -> bool {
        self.door == Door::Local || self.client.is_some()
    }
}

/* What handle_socket should do after a message. */
// Wizard carries a whole session, the others a message: moved once per
// request, not worth boxing (clippy::large_enum_variant, as elsewhere).
#[allow(clippy::large_enum_variant)]
enum Next {
    Send(ServerMessage),
    SendAndClose(ServerMessage),
    /* Nothing to send now: the request became background work (see
     * Background), its reply comes when that's done. */
    Queued,
    /* A wizard step done in the background: its session goes back to the
     * connection, then the reply is sent. */
    Wizard(Option<wizard::Session>, ServerMessage),
}

/* Requests whose answer can take seconds -- a device's command or action
 * (an unplugged WLED: 5 s until "can't reach"; waking a TV: up to 30 s),
 * a wizard's test or pairing step (up to a minute) -- run in the
 * BACKGROUND, so they hold up nothing else of this connection: other
 * requests (the LED, while the WLED times out -- seen on the DK2), events,
 * the keep-alive. Their replies still go out in the order the requests
 * came (FuturesOrdered): clients match replies to requests by order. */
type Work = std::pin::Pin<Box<dyn std::future::Future<Output = Next> + Send>>;

/* At most this many requests of one connection in the background; more
 * wait until one is done (a client can't make the hub pile up work). */
const MAX_BACKGROUND: usize = 32;

/* `req` as background work, if it's one of the slow kinds and this
 * session may send it; else it's handed back to be handled at once. (The
 * request itself is the "error" handed back: large, once per request.) */
#[allow(clippy::result_large_err)]
fn background(
    req: ClientRequest,
    session: &Session,
    wizard: &mut Option<wizard::Session>,
    app_state: &AppState,
) -> Result<Work, ClientRequest> {
    if !session.trusted() {
        return Err(req);
    }
    let app = app_state.clone();
    match req {
        ClientRequest::Command { .. } | ClientRequest::DeviceAction { .. } => {
            Ok(Box::pin(async move { Next::Send(handle_request(req, &app).await) }))
        }
        /* A scene may wait for slow devices (a TV waking). */
        ClientRequest::RunScene { id } => {
            let asker = if session.door == Door::Local { automations::Asker::Screen } else { automations::Asker::App };
            Ok(Box::pin(async move { Next::Send(ack_or_error(app.automations.run_scene(&id, asker).await)) }))
        }
        ClientRequest::RunAutomation { id } => {
            Ok(Box::pin(async move { Next::Send(ack_or_error(app.automations.run_automation_now(&id).await)) }))
        }
        ClientRequest::WizardStart { .. }
        | ClientRequest::WizardAnswer { .. }
        | ClientRequest::WizardBack { .. }
        | ClientRequest::WizardFinish { .. }
        | ClientRequest::WizardReauth { .. }
        | ClientRequest::WizardReconfigure { .. } => {
            /* The session travels with the work and comes back with its
             * reply (Next::Wizard). */
            let mut session = wizard.take();
            Ok(Box::pin(async move {
                let reply = handle_wizard(req, &mut session, &app).await;
                Next::Wizard(session, reply)
            }))
        }
        other => Err(other),
    }
}

/* One of these runs per connected client, for as long as it's connected.
 * It waits for whichever comes first: a request from the client, a device
 * event to pass on (once subscribed), this client being revoked, or -- for
 * a LAN client that hasn't authenticated -- the deadline. */
async fn handle_socket(mut socket: WebSocket, app_state: AppState, door: Door, peer: SocketAddr) {
    /* `_count` only has to live until this function returns; its Drop
     * lowers the count again. (A plain `_` would drop it immediately.) */
    let _count = ClientCount::new(&app_state.local_clients);
    let mut session = Session {
        door,
        peer,
        client: None,
        failures: 0,
    };
    /* None until the client subscribes. */
    let mut events: Option<broadcast::Receiver<Event>> = None;
    let mut settings_rx: Option<watch::Receiver<HubSettings>> = None;
    let mut found_rx: Option<watch::Receiver<u64>> = None;
    /* Issue #47: the scenes/automations list, and what ran. */
    let mut automations_rx: Option<watch::Receiver<u64>> = None;
    let mut ran_rx: Option<broadcast::Receiver<automations::LogEntry>> = None;
    /* The device this client is adding, if any (wizard.rs). While one of
     * its steps runs in the background, the session is with that work. */
    let mut wizard: Option<wizard::Session> = None;
    /* A wizard cancelled while one of its steps was still running: that
     * step's session isn't taken back when it's done. */
    let mut cancelled: Option<String> = None;
    /* Requests being carried out in the background, in arrival order. */
    let mut background_work: FuturesOrdered<Work> = FuturesOrdered::new();
    let mut revocations = app_state.auth.revocations();
    let deadline = tokio::time::sleep(AUTH_DEADLINE);
    tokio::pin!(deadline);

    loop {
        let next = tokio::select! {
            /* Not while MAX_BACKGROUND requests are running (see there). */
            incoming = socket.recv(), if background_work.len() < MAX_BACKGROUND => {
                /* None / Err: the client is gone. */
                let Some(Ok(msg)) = incoming else { break };
                let Message::Text(text) = msg else { continue };
                let next = match serde_json::from_str::<ClientRequest>(&text) {
                    Ok(ClientRequest::WizardCancel { session: id }) if wizard.is_none() => {
                        /* Its step is running in the background: forget the
                         * session when that's done. */
                        cancelled = Some(id);
                        Next::Send(ServerMessage::Ack)
                    }
                    Ok(req) => match background(req, &session, &mut wizard, &app_state) {
                        Ok(work) => {
                            background_work.push_back(work);
                            Next::Queued
                        }
                        Err(req) => {
                            let subscriptions = Subscriptions {
                                events: &mut events,
                                settings: &mut settings_rx,
                                found: &mut found_rx,
                                automations: &mut automations_rx,
                                ran: &mut ran_rx,
                            };
                            handle_message(req, &mut session, subscriptions, &mut wizard, &app_state).await
                        }
                    },
                    Err(e) => Next::Send(ServerMessage::Error { message: e.to_string() }),
                };
                /* A quick reply must not overtake the slower replies before
                 * it: while work is running, it queues up behind them. */
                match next {
                    Next::Send(message) if !background_work.is_empty() => {
                        background_work.push_back(Box::pin(async move { Next::Send(message) }));
                        Next::Queued
                    }
                    other => other,
                }
            }
            /* Background work done: its reply, in order. */
            Some(next) = background_work.next(), if !background_work.is_empty() => next,
            /* This branch only exists while subscribed (the `if`). */
            event = next_event(&mut events), if events.is_some() => Next::Send(event),
            /* Settings changes (issue #39), also only while subscribed. */
            Some(settings) = next_settings(&mut settings_rx), if settings_rx.is_some() => {
                Next::Send(ServerMessage::SettingsChanged { settings })
            }
            /* The inbox changed (issue #40), also only while subscribed. */
            Some(()) = next_found(&mut found_rx), if found_rx.is_some() => Next::Send(ServerMessage::FoundChanged),
            /* Scenes / automations changed, or one ran (issue #47). */
            Some(()) = next_found(&mut automations_rx), if automations_rx.is_some() => Next::Send(ServerMessage::AutomationsChanged),
            Some(entry) = next_ran(&mut ran_rx), if ran_rx.is_some() => Next::Send(ServerMessage::AutomationRan { entry }),
            Ok(id) = revocations.recv() => {
                if session.client.as_ref().is_some_and(|c| c.id == id) {
                    println!("ws.rs: {id} was removed on the hub, closing its connection");
                    Next::SendAndClose(ServerMessage::Error { message: "this device's access was removed on the hub".into() })
                } else {
                    continue;
                }
            }
            _ = &mut deadline, if !session.trusted() => {
                Next::SendAndClose(ServerMessage::Error { message: "pair or authenticate first (timed out)".into() })
            }
        };

        let (message, close) = match next {
            Next::Send(m) => (m, false),
            Next::SendAndClose(m) => (m, true),
            Next::Queued => continue,
            Next::Wizard(session, m) => {
                let dropped = match (&session, &cancelled) {
                    (Some(s), Some(id)) => s.id() == id,
                    _ => false,
                };
                if dropped {
                    cancelled = None;
                } else if session.is_some() || wizard.is_none() {
                    wizard = session;
                }
                (m, false)
            }
        };
        let payload = serde_json::to_string(&message).expect("ServerMessage is always valid JSON");
        if socket.send(Message::Text(payload)).await.is_err() {
            /* The client disconnected. */
            break;
        }
        if close {
            /* We end it: say so with a proper WebSocket close message, so
             * the client sees a clean "closed by the hub", not a broken
             * connection. */
            let _ = socket.send(Message::Close(None)).await;
            break;
        }
    }
}

/* What a connection is subscribed to (all None until "subscribe"). */
struct Subscriptions<'a> {
    events: &'a mut Option<broadcast::Receiver<Event>>,
    settings: &'a mut Option<watch::Receiver<HubSettings>>,
    found: &'a mut Option<watch::Receiver<u64>>,
    automations: &'a mut Option<watch::Receiver<u64>>,
    ran: &'a mut Option<broadcast::Receiver<automations::LogEntry>>,
}

/* Checks what this session may do, then carries the request out. */
async fn handle_message(
    req: ClientRequest,
    session: &mut Session,
    subscriptions: Subscriptions<'_>,
    wizard: &mut Option<wizard::Session>,
    app_state: &AppState,
) -> Next {
    if req.local_only() && session.door != Door::Local {
        return Next::Send(ServerMessage::Error {
            message: "only allowed from the hub's own screen".into(),
        });
    }
    if !session.trusted() && !req.allowed_anonymously() {
        return Next::Send(ServerMessage::Error {
            message: "not authenticated: pair (code from the hub's screen) or auth (your key) first".into(),
        });
    }
    let auth = &app_state.auth;
    match req {
        ClientRequest::Hello => Next::Send(ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: device::CAPABILITY_NAMES.to_vec(),
            authenticated: session.trusted(),
        }),
        ClientRequest::Subscribe => {
            /* Events from now on; the client lists the devices once to
             * know where to start. */
            *subscriptions.events = Some(app_state.events_tx.subscribe());
            /* The settings too (issue #39) -- from now on: the current
             * ones count as seen (the client asks get_settings once). */
            let mut rx = app_state.settings.subscribe();
            rx.mark_unchanged();
            *subscriptions.settings = Some(rx);
            /* And the inbox (issue #40), the same way. */
            let mut rx = app_state.discovery.subscribe();
            rx.mark_unchanged();
            *subscriptions.found = Some(rx);
            /* And scenes / automations (issue #47). */
            let mut rx = app_state.automations.subscribe();
            rx.mark_unchanged();
            *subscriptions.automations = Some(rx);
            *subscriptions.ran = Some(app_state.automations.subscribe_log());
            Next::Send(ServerMessage::Ack)
        }
        ClientRequest::Pair { code, client_name } => match auth.pair(&code, &client_name) {
            Ok(paired) => {
                println!("ws.rs: {client_name:?} paired as {} from {}", paired.client_id, session.peer.ip());
                session.client = auth.authenticate(&paired.token);
                Next::Send(ServerMessage::Paired {
                    client_id: paired.client_id,
                    token: paired.token,
                    hub_fingerprint: app_state.fingerprint.to_string(),
                })
            }
            Err(message) => failed(session, "pairing", message).await,
        },
        ClientRequest::Auth { token } => match auth.authenticate(&token) {
            Some(client) => {
                let reply = ServerMessage::Authenticated {
                    client_id: client.id.clone(),
                    name: client.name.clone(),
                };
                session.client = Some(client);
                Next::Send(reply)
            }
            None => failed(session, "authentication", "unknown or removed key: pair again".into()).await,
        },
        ClientRequest::StartPairing => Next::Send(pairing_message(auth.start_pairing(), app_state)),
        ClientRequest::PairingStatus => Next::Send(pairing_message(auth.pairing_status(), app_state)),
        ClientRequest::CancelPairing => {
            auth.cancel_pairing();
            Next::Send(ServerMessage::Ack)
        }
        ClientRequest::ListClients => Next::Send(ServerMessage::Clients { clients: auth.list() }),
        ClientRequest::RevokeClient { id } => Next::Send(ack_or_error(auth.revoke(&id))),
        ClientRequest::StartHotspot => Next::Send(match app_state.hotspot.open().await {
            Ok(status) => ServerMessage::Hotspot { status },
            Err(message) => ServerMessage::Error { message },
        }),
        ClientRequest::StopHotspot => Next::Send(ServerMessage::Hotspot {
            status: app_state.hotspot.close().await,
        }),
        ClientRequest::GetNetworkStatus => {
            let reply = match ask_network(&app_state.network_tx, |reply| network::Cmd::GetStatus { reply }).await {
                Ok(status) => {
                    let mut hotspot = app_state.hotspot.status();
                    /* The hotspot password is the proof of being AT the
                     * hub: only its own screen ever gets it. */
                    if session.door != Door::Local {
                        hotspot.password = None;
                    }
                    ServerMessage::NetworkStatus { status, hotspot }
                }
                Err(message) => ServerMessage::Error { message },
            };
            Next::Send(reply)
        }
        ClientRequest::ListTemplates
        | ClientRequest::WizardStart { .. }
        | ClientRequest::WizardAnswer { .. }
        | ClientRequest::WizardBack { .. }
        | ClientRequest::WizardFinish { .. }
        | ClientRequest::WizardCancel { .. }
        | ClientRequest::WizardReauth { .. }
        | ClientRequest::WizardReconfigure { .. } => Next::Send(handle_wizard(req, wizard, app_state).await),
        other => Next::Send(handle_request(other, app_state).await),
    }
}

/* The wizard's requests (issue #40). The session lives in the connection
 * (handle_socket's `wizard`): one per client, gone with it. Adapters'
 * probes and pairing actions run inside the request, so this client's
 * other messages wait meanwhile (up to a step's timeout); other clients
 * aren't affected. */
async fn handle_wizard(req: ClientRequest, wizard: &mut Option<wizard::Session>, app_state: &AppState) -> ServerMessage {
    /* Only a start pre-fills the WiFi name (#42): ask the network actor
     * then, not on every step. */
    let hub_wifi = match &req {
        ClientRequest::WizardStart { .. } => ask_network(&app_state.network_tx, |reply| network::Cmd::GetStatus { reply })
            .await
            .ok()
            .filter(|s| s.wifi.connected)
            .and_then(|s| s.wifi.ssid),
        _ => None,
    };
    let ctx = wizard::Context {
        templates: &app_state.templates,
        control: &app_state.control,
        discovery: &app_state.discovery,
        hub_wifi,
    };
    /* A step's reply, or its error for this session. */
    let reply = |id: &str, result: Result<wizard::StepView, wizard::WizardError>| match result {
        Ok(step) => ServerMessage::WizardStep { step },
        Err(e) => wizard_error(id, e),
    };
    match req {
        ClientRequest::ListTemplates => ServerMessage::Templates {
            templates: wizard::list(&app_state.templates),
        },
        ClientRequest::WizardStart { template, variant, found } => {
            match wizard::Session::start(&ctx, &template, variant.as_deref(), found.as_deref()).await {
                Ok((session, step)) => {
                    /* A new start replaces an unfinished one. */
                    *wizard = Some(session);
                    ServerMessage::WizardStep { step }
                }
                Err(e) => wizard_error("", e),
            }
        }
        ClientRequest::WizardReauth { .. } | ClientRequest::WizardReconfigure { .. } => {
            let (device, mode) = match req {
                ClientRequest::WizardReauth { device } => (device, wizard::Mode::Reauth),
                ClientRequest::WizardReconfigure { device } => (device, wizard::Mode::Reconfigure),
                _ => unreachable!("matched above"),
            };
            match wizard::Session::start_for(&ctx, &device, mode).await {
                Ok((session, step)) => {
                    *wizard = Some(session);
                    ServerMessage::WizardStep { step }
                }
                Err(e) => wizard_error("", e),
            }
        }
        ClientRequest::WizardAnswer { session, values } => match current(wizard, &session) {
            Ok(s) => reply(&session, s.answer(&ctx, values).await),
            Err(e) => wizard_error(&session, e),
        },
        ClientRequest::WizardBack { session } => match current(wizard, &session) {
            Ok(s) => reply(&session, s.back(&ctx).await),
            Err(e) => wizard_error(&session, e),
        },
        ClientRequest::WizardFinish { session, name, room } => match current(wizard, &session) {
            Ok(s) => match s.finish(&ctx, &name, &room).await {
                Ok(device) => {
                    println!("ws.rs: {} set up with the wizard ({})", device.id, device.template);
                    *wizard = None;
                    ServerMessage::Device { device: Some(device) }
                }
                Err(e) => wizard_error(&session, e),
            },
            Err(e) => wizard_error(&session, e),
        },
        ClientRequest::WizardCancel { session } => {
            if wizard.as_ref().is_some_and(|s| s.id() == session) {
                *wizard = None;
            }
            ServerMessage::Ack
        }
        _ => ServerMessage::Error {
            message: "internal: not a wizard request".into(),
        },
    }
}

/* The running session with this id -- or the message saying it's over
 * (idle too long, replaced by a newer start, or never existed). */
fn current<'a>(wizard: &'a mut Option<wizard::Session>, id: &str) -> Result<&'a mut wizard::Session, wizard::WizardError> {
    if wizard.as_ref().is_some_and(|s| s.expired()) {
        *wizard = None;
    }
    match wizard {
        Some(s) if s.id() == id => Ok(s),
        _ => Err(wizard::WizardError {
            field: None,
            message: "This setup has ended (it waited too long, or another one started). Please start again.".into(),
            detail: String::new(),
        }),
    }
}

fn wizard_error(session: &str, e: wizard::WizardError) -> ServerMessage {
    ServerMessage::WizardError {
        session: session.to_string(),
        field: e.field,
        message: e.message,
        detail: e.detail,
    }
}

/* A failed pair/auth: logged (without the code or key), answered only
 * after FAILURE_DELAY, and after MAX_FAILURES the connection is closed. */
async fn failed(session: &mut Session, what: &str, message: String) -> Next {
    session.failures += 1;
    println!("ws.rs: failed {what} from {} ({message})", session.peer.ip());
    tokio::time::sleep(FAILURE_DELAY).await;
    let reply = ServerMessage::Error { message };
    if session.failures >= MAX_FAILURES {
        Next::SendAndClose(reply)
    } else {
        Next::Send(reply)
    }
}

/* The pairing screen's data: the status plus where to connect. */
fn pairing_message(status: auth::PairingStatus, app_state: &AppState) -> ServerMessage {
    let mut addresses: Vec<String> = network::ipv4_addresses()
        .into_iter()
        .filter(|(name, _)| name != "lo")
        .map(|(_, ip)| ip.to_string())
        .collect();
    addresses.sort();
    ServerMessage::Pairing {
        status,
        addresses,
        port: 8443,
        fingerprint: app_state.fingerprint.to_string(),
        fingerprint_short: tls::short_fingerprint(&app_state.fingerprint),
    }
}

/* The next device event for a subscribed client, as a message. The
 * broadcast channel keeps a limited backlog per subscriber (see main.rs);
 * a client too slow to keep up misses events ("Lagged") and is told so, so
 * it can list the devices again instead of showing stale ones. */
async fn next_event(events: &mut Option<broadcast::Receiver<Event>>) -> ServerMessage {
    let Some(rx) = events.as_mut() else {
        /* Not reachable: select! only polls this while subscribed. */
        return std::future::pending().await;
    };
    match rx.recv().await {
        Ok(Event::Changed(device)) => ServerMessage::DeviceChanged { device },
        Ok(Event::Removed(id)) => ServerMessage::DeviceRemoved { id },
        Err(broadcast::error::RecvError::Lagged(_)) => ServerMessage::EventsLost,
        Err(broadcast::error::RecvError::Closed) => {
            /* state.rs is gone (shutting down): no more events. */
            *events = None;
            ServerMessage::EventsLost
        }
    }
}

/* The next inbox change for a subscribed client (issue #40). */
async fn next_found(rx: &mut Option<watch::Receiver<u64>>) -> Option<()> {
    let receiver = rx.as_mut()?;
    if receiver.changed().await.is_err() {
        *rx = None;
        return None;
    }
    receiver.borrow_and_update();
    Some(())
}

/* The next log entry for a subscribed client (issue #47). A client too
 * slow to keep up just misses some (it can get_automation_log). */
async fn next_ran(rx: &mut Option<broadcast::Receiver<automations::LogEntry>>) -> Option<automations::LogEntry> {
    let receiver = rx.as_mut()?;
    loop {
        match receiver.recv().await {
            Ok(entry) => return Some(entry),
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => {
                *rx = None;
                return None;
            }
        }
    }
}

/* The next settings change for a subscribed client (issue #39). None:
 * settings.rs is gone (shutting down) -- then stop listening. */
async fn next_settings(rx: &mut Option<watch::Receiver<HubSettings>>) -> Option<HubSettings> {
    let receiver = rx.as_mut()?;
    if receiver.changed().await.is_err() {
        *rx = None;
        return None;
    }
    let settings = receiver.borrow_and_update().clone();
    Some(settings)
}

/* The scenes and automations after a change, or why it was refused. */
fn book_or_error(automations: &Automations, result: Result<(), String>) -> ServerMessage {
    match result {
        Ok(()) => ServerMessage::Automations { book: automations.book() },
        Err(message) => ServerMessage::Error { message },
    }
}

/* Carries out one request. Device requests go through control.rs;
 * network ones to network.rs. */
async fn handle_request(req: ClientRequest, app_state: &AppState) -> ServerMessage {
    let control = &app_state.control;
    let device = |result: Result<Device, String>| match result {
        Ok(device) => ServerMessage::Device { device: Some(device) },
        Err(message) => ServerMessage::Error { message },
    };
    match req {
        ClientRequest::ListDevices => match control.list().await {
            Ok(devices) => ServerMessage::Devices { devices },
            Err(message) => ServerMessage::Error { message },
        },
        ClientRequest::GetDevice { id } => match control.get(&id).await {
            Ok(device) => ServerMessage::Device { device },
            Err(message) => ServerMessage::Error { message },
        },
        ClientRequest::AddDevice { device: new } => device(control.add(new).await),
        ClientRequest::UpdateDeviceInfo { id, name, room, favourite } => {
            device(control.update_info(&id, name, room, favourite).await)
        }
        ClientRequest::RemoveDevice { id } => ack_or_error(control.remove(&id).await),
        ClientRequest::Command { id, capability, value } => device(control.command(&id, &capability, value).await),
        ClientRequest::DeviceAction { id, capability, name, args } => {
            match control.action(&id, &capability, &name, args).await {
                Ok(result) => ServerMessage::ActionResult { result },
                Err(message) => ServerMessage::Error { message },
            }
        }
        ClientRequest::ListFound => ServerMessage::Found {
            devices: app_state.discovery.inbox(control).await,
        },
        ClientRequest::DiscoverNow => {
            app_state.discovery.discover_now();
            ServerMessage::Ack
        }
        ClientRequest::GetSettings => ServerMessage::Settings {
            settings: app_state.settings.get(),
        },
        ClientRequest::SetSettings {
            mode,
            accent,
            density,
            time_zone,
            latitude,
            longitude,
        } => match app_state.settings.update(settings::Change {
            mode,
            accent,
            density,
            time_zone,
            latitude,
            longitude,
        }) {
            Ok(settings) => ServerMessage::Settings { settings },
            Err(message) => ServerMessage::Error { message },
        },
        /* Issue #47: every change answers with the whole list. */
        ClientRequest::ListAutomations => ServerMessage::Automations {
            book: app_state.automations.book(),
        },
        ClientRequest::SaveScene { scene } => book_or_error(&app_state.automations, app_state.automations.save_scene(scene).await.map(|_| ())),
        ClientRequest::DeleteScene { id } => book_or_error(&app_state.automations, app_state.automations.delete_scene(&id)),
        ClientRequest::CaptureScene { name, devices } => {
            book_or_error(&app_state.automations, app_state.automations.capture_scene(&name, &devices).await.map(|_| ()))
        }
        ClientRequest::SaveAutomation { automation } => {
            book_or_error(&app_state.automations, app_state.automations.save_automation(automation).await.map(|_| ()))
        }
        ClientRequest::DeleteAutomation { id } => book_or_error(&app_state.automations, app_state.automations.delete_automation(&id)),
        ClientRequest::SetAutomationEnabled { id, enabled } => {
            book_or_error(&app_state.automations, app_state.automations.set_enabled(&id, enabled))
        }
        ClientRequest::WebhookUrl { id } => match app_state.webhooks.url_for(&id).await {
            Ok(url) => ServerMessage::Webhook { url },
            Err(message) => ServerMessage::Error { message },
        },
        ClientRequest::GetAutomationLog => ServerMessage::AutomationLog {
            entries: app_state.automations.log(),
        },
        /* Normally run in the background (see background()); here only
         * for a client that isn't trusted -- refused before this anyway. */
        ClientRequest::RunScene { id } => ack_or_error(app_state.automations.run_scene(&id, automations::Asker::App).await),
        ClientRequest::RunAutomation { id } => ack_or_error(app_state.automations.run_automation_now(&id).await),
        /* Handled in handle_message (they concern the session or auth.rs). */
        ClientRequest::Hello
        | ClientRequest::Subscribe
        | ClientRequest::Pair { .. }
        | ClientRequest::Auth { .. }
        | ClientRequest::StartPairing
        | ClientRequest::PairingStatus
        | ClientRequest::CancelPairing
        | ClientRequest::ListClients
        | ClientRequest::RevokeClient { .. }
        | ClientRequest::StartHotspot
        | ClientRequest::StopHotspot
        | ClientRequest::GetNetworkStatus
        | ClientRequest::ListTemplates
        | ClientRequest::WizardStart { .. }
        | ClientRequest::WizardAnswer { .. }
        | ClientRequest::WizardBack { .. }
        | ClientRequest::WizardFinish { .. }
        | ClientRequest::WizardCancel { .. }
        | ClientRequest::WizardReauth { .. }
        | ClientRequest::WizardReconfigure { .. } => ServerMessage::Error {
            message: "internal: request not routed".into(),
        },
        ClientRequest::WifiScan => {
            match ask_network(&app_state.network_tx, |reply| network::Cmd::Scan { reply }).await {
                Ok(Ok(networks)) => ServerMessage::WifiNetworks { networks },
                Ok(Err(message)) | Err(message) => ServerMessage::Error { message },
            }
        }
        ClientRequest::WifiConnect { ssid, password } => {
            let answer = ask_network(&app_state.network_tx, |reply| network::Cmd::Connect {
                ssid,
                password,
                reply,
            })
            .await;
            ack_or_error(answer.and_then(|r| r))
        }
        ClientRequest::WifiForget => {
            let answer = ask_network(&app_state.network_tx, |reply| network::Cmd::Forget { reply }).await;
            ack_or_error(answer.and_then(|r| r))
        }
        ClientRequest::SetWifiCountry { country } => {
            let answer = ask_network(&app_state.network_tx, |reply| network::Cmd::SetCountry { country, reply }).await;
            ack_or_error(answer.and_then(|r| r))
        }
    }
}

/* One request/reply round trip with network.rs's actor: `make` builds the
 * command around the reply envelope created here. Err only if the actor
 * is gone (the daemon shutting down). */
async fn ask_network<T>(
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

/* For requests that only succeed or fail. */
fn ack_or_error(answer: Result<(), String>) -> ServerMessage {
    match answer {
        Ok(()) => ServerMessage::Ack,
        Err(message) => ServerMessage::Error { message },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{Adapter, DeviceCmd, DeviceHandle, Hub, Registry};
    use crate::device::{Capabilities, Source, Switch};
    use crate::secrets::Secrets;
    use crate::state::{self, Outputs};
    use futures_util::SinkExt;
    use serde_json::{json, Value};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /* An adapter whose device takes 2 s to answer anything: an unplugged
     * WLED waiting for its timeout. */
    struct Slow;
    impl Adapter for Slow {
        fn id(&self) -> &'static str {
            "slow"
        }
        fn start(&self, _device: &Device, _hub: Hub) -> DeviceHandle {
            let (commands, mut rx) = mpsc::channel::<DeviceCmd>(8);
            tokio::spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    cmd.refuse("can't reach 192.168.1.139: timed out");
                }
            });
            DeviceHandle::new(commands)
        }
    }

    fn switch_device(id: &str, source: &str) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            room: String::new(),
            template: String::new(),
            source: Source::new(source),
            config: Default::default(),
            identity: String::new(),
            online: None,
            last_seen: None,
            favourite: false,
            capabilities: Capabilities {
                switch: Some(Switch { on: false }),
                ..Default::default()
            },
        }
    }

    /* The hub's own door on a free port, with a slow device and a lamp. */
    async fn serve() -> String {
        let devices = [switch_device("strip", "slow"), switch_device("lamp", "virtual")];
        let (state_tx, state_rx) = mpsc::channel(8);
        let (events_tx, _) = broadcast::channel(64);
        let outputs = Outputs {
            changed_tx: watch::channel(()).0,
            events_tx: events_tx.clone(),
            save_tx: watch::channel(Vec::new()).0,
        };
        tokio::spawn(state::run(state_rx, devices.into_iter().map(|d| (d.id.clone(), d)).collect(), outputs));
        let registry = Arc::new(Registry::new(vec![Box::new(Slow)]));
        let secrets = Arc::new(Secrets::new(Default::default(), watch::channel(Vec::new()).0));
        let control = Control::new(state_tx, registry.clone(), secrets);
        registry.start_all(&control).await;
        let state = AppState {
            control: control.clone(),
            auth: Arc::new(Auth::new(Vec::new(), watch::channel(Vec::new()).0)),
            hotspot: Hotspot::new(mpsc::channel(1).0),
            fingerprint: Arc::new(String::new()),
            network_tx: mpsc::channel(1).0,
            events_tx,
            local_clients: Arc::new(AtomicUsize::new(0)),
            settings: Arc::new(Settings::new(HubSettings::default(), watch::channel(Vec::new()).0)),
            discovery: Arc::new(Discovery::new()),
            templates: Arc::new(Templates::default()),
            automations: Arc::new(Automations::new(
                Default::default(),
                watch::channel(Vec::new()).0,
                control.clone(),
                Arc::new(Settings::new(HubSettings::default(), watch::channel(Vec::new()).0)),
            )),
            webhooks: Arc::new(Webhooks::new(control.clone())),
        };
        let router = Router::new()
            .route("/ws", get(ws_handler))
            .layer(Extension(Door::Local))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await;
        });
        format!("ws://{addr}/ws")
    }

    /* The next message from the hub, as JSON. */
    async fn next<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /* A slow device doesn't hold up the others (seen on the DK2: the LED
     * didn't react while an unplugged WLED timed out). Its reply still
     * comes first: replies keep the order of the requests. */
    #[tokio::test]
    async fn a_slow_device_holds_up_nothing_else() {
        let url = serve().await;
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        for request in [
            json!({"action": "subscribe"}),
            json!({"action": "command", "id": "strip", "capability": "switch", "value": {"on": true}}),
            json!({"action": "command", "id": "lamp", "capability": "switch", "value": {"on": true}}),
        ] {
            ws.send(WsMessage::Text(request.to_string())).await.unwrap();
        }
        assert_eq!(next(&mut ws).await["type"], "ack", "subscribe");

        /* The lamp changes at once -- while the strip is still waiting. */
        let started = std::time::Instant::now();
        let event = next(&mut ws).await;
        assert_eq!((event["type"].as_str(), event["device"]["id"].as_str()), (Some("device_changed"), Some("lamp")));
        assert!(started.elapsed() < Duration::from_secs(1), "the lamp waited {:?}", started.elapsed());

        /* Then the replies, in the order of the requests. */
        let strip = next(&mut ws).await;
        assert_eq!(strip["type"], "error");
        assert!(strip["message"].as_str().unwrap().contains("can't reach"));
        let lamp = next(&mut ws).await;
        assert_eq!((lamp["type"].as_str(), lamp["device"]["id"].as_str()), (Some("device"), Some("lamp")));
    }
}
