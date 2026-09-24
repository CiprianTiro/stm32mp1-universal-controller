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
 *   update_device_info {id, name?, room?}  -> device {device}
 *   remove_device {id}                     -> ack
 *   command {id, capability, value}        -> device {device}
 *        e.g. {"action":"command","id":"lamp-1","capability":"dimmer",
 *              "value":{"level":30}}
 *   subscribe                              -> ack, then events:
 *        device_changed {device} / device_removed {id} /
 *        events_lost (too slow to keep up: list_devices again)
 *   get_network_status, wifi_scan, wifi_connect, wifi_forget,
 *   set_wifi_country                       -> see network.rs (#61)
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
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::auth::{self, Auth};
use crate::control::Control;
use crate::hotspot::{self, Hotspot};
use crate::device::{self, Device};
use crate::network;
use crate::state::{DeviceId, Event};
use crate::tls;

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
    },
    RemoveDevice {
        id: DeviceId,
    },
    Command {
        id: DeviceId,
        capability: String,
        value: serde_json::Value,
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
    /* Events. */
    DeviceChanged {
        device: Device,
    },
    DeviceRemoved {
        id: DeviceId,
    },
    EventsLost,
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
enum Next {
    Send(ServerMessage),
    SendAndClose(ServerMessage),
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
    let mut revocations = app_state.auth.revocations();
    let deadline = tokio::time::sleep(AUTH_DEADLINE);
    tokio::pin!(deadline);

    loop {
        let next = tokio::select! {
            incoming = socket.recv() => {
                /* None / Err: the client is gone. */
                let Some(Ok(msg)) = incoming else { break };
                let Message::Text(text) = msg else { continue };
                match serde_json::from_str::<ClientRequest>(&text) {
                    Ok(req) => handle_message(req, &mut session, &mut events, &app_state).await,
                    Err(e) => Next::Send(ServerMessage::Error { message: e.to_string() }),
                }
            }
            /* This branch only exists while subscribed (the `if`). */
            event = next_event(&mut events), if events.is_some() => Next::Send(event),
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

/* Checks what this session may do, then carries the request out. */
async fn handle_message(
    req: ClientRequest,
    session: &mut Session,
    events: &mut Option<broadcast::Receiver<Event>>,
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
            *events = Some(app_state.events_tx.subscribe());
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
        other => Next::Send(handle_request(other, app_state).await),
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
        ClientRequest::UpdateDeviceInfo { id, name, room } => device(control.update_info(&id, name, room).await),
        ClientRequest::RemoveDevice { id } => ack_or_error(control.remove(&id).await),
        ClientRequest::Command { id, capability, value } => device(control.command(&id, &capability, value).await),
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
        | ClientRequest::GetNetworkStatus => ServerMessage::Error {
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
