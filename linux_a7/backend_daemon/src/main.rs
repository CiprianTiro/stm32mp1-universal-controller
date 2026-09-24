use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::interval;

/* `mod state;` / `mod ws;` tell the compiler "compile state.rs / ws.rs as
 * part of this crate" -- this is what actually makes their code exist in
 * the final binary at all. It does NOT run anything in them; nothing in
 * either file executes until something below explicitly spawns it. */
mod auth;
mod ble;
mod control;
mod device;
mod health;
mod hotspot;
mod mqtt;
mod network;
mod rpmsg;
mod shadow;
mod state;
mod store;
mod tls;
mod ws;

/* worker_threads = 2 -- pinned explicitly to match the DK2's 2 physical
 * Cortex-A7 cores, rather than relying on Tokio's auto-detect (which
 * happens to land on the same number here, but this documents the intent). */
#[tokio::main(worker_threads = 2)]
async fn main() {
    println!("backend_daemon starting (arch: {})", std::env::consts::ARCH);

    tokio::spawn(heartbeat());

    /* THE DEVICES (state.rs). `mpsc::channel(32)` creates the actor's
     * mailbox and hands back both ends: state_tx, the SENDING half -- a
     * cheap handle anyone can clone to drop messages in -- and state_rx,
     * the RECEIVING half, of which there is only ever one: it goes to the
     * one task that owns the devices. Cloning state_tx never copies the
     * devices; it copies a permission slip to write to the same mailbox.
     *
     * Three more channels carry news of changes out of the actor (see
     * state.rs's header): a `watch` bell for mqtt.rs, a `broadcast` channel
     * of events for ws.rs's subscribed clients (64 = how many events a slow
     * client may fall behind before it's told it missed some), and
     * store.rs's writer for the registry file. */
    let (state_tx, state_rx) = tokio::sync::mpsc::channel(32);
    let (state_changed_tx, state_changed_rx) = tokio::sync::watch::channel(());
    let (events_tx, _) = tokio::sync::broadcast::channel(64);

    /* The device registry (issues #33, #34): loaded from the userfs
     * partition BEFORE the actor starts, so the first client to connect
     * already sees every device from before the restart. A registry from
     * before #34 is converted to the capability model here (logged per
     * device). Every other outcome -- first start, a damaged file, a
     * fallback to the previous copy -- is logged by load_or_default (see
     * store.rs). */
    let registry = store::Store::new(&store::data_dir(), "devices.json");
    let devices = registry.load_or_default("device registry", state::decode_registry);
    /* No file name here: when the previous copy had to be used, the line
     * store.rs logged just before says so (and names the file). */
    println!("state: {} device(s) loaded", devices.len());
    let save_tx = store::writer(registry, state::REGISTRY_SCHEMA);
    tokio::spawn(state::run(
        state_rx,
        devices,
        state::Outputs {
            changed_tx: state_changed_tx,
            events_tx: events_tx.clone(),
            save_tx,
        },
    ));

    /* The link to the M4 (rpmsg.rs), a separate actor: it owns the RPMsg
     * device file. led_tx/led_rx is a `watch` channel holding the LED's
     * state as the M4 last reported it (see rpmsg::run). */
    let (rpmsg_tx, rpmsg_rx) = tokio::sync::mpsc::channel(8);
    let (led_tx, led_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(rpmsg::run(rpmsg_rx, led_tx));

    /* The one door for device commands (control.rs, issue #34): the
     * WebSocket clients and the cloud both go through it. The LED's driver
     * keeps the "ld7" device in step with what the M4 reports. */
    let control = control::Control::new(state_tx, rpmsg_tx);
    tokio::spawn(control::run_led_driver(control.clone(), led_rx));

    /* Cloud sync (mqtt.rs). local_clients: how many WebSocket clients are
     * connected right now -- ws.rs counts, health.rs (inside mqtt.rs)
     * reports it; one shared number, hence Arc (shared ownership) +
     * AtomicUsize (lock-free). The uplink watch (issue #61): network.rs
     * keeps it up to date with the link carrying traffic, and mqtt.rs
     * reconnects when it changes. */
    let local_clients = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (uplink_tx, uplink_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(network::watch_uplink(uplink_tx));
    tokio::spawn(mqtt::run(control.clone(), state_changed_rx, uplink_rx, local_clients.clone()));

    /* The network actor (issue #61): Ethernet/WiFi status, WiFi scan,
     * connect, forget, country. Only ws.rs talks to it. */
    let (network_tx, network_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(network::run(network_rx));

    /* The setup hotspot (issue #36): opens by itself when the hub has had
     * no network for a while after start, or by a tap on the touchscreen. */
    let hotspot = hotspot::Hotspot::new(network_tx.clone());
    tokio::spawn(hotspot::run_auto(hotspot.clone()));

    /* Who may use the LAN (issue #35): the paired clients (auth.rs, saved
     * as clients.json next to the registry) and the hub's TLS identity
     * (tls.rs, created on first start). Without a TLS identity the LAN
     * door stays closed; the touchscreen's local door works regardless. */
    let clients_store = store::Store::new(&store::data_dir(), "clients.json");
    let clients = clients_store.load_or_default("paired clients", auth::decode_clients);
    let auth = std::sync::Arc::new(auth::Auth::new(
        clients,
        store::writer(clients_store, auth::CLIENTS_SCHEMA),
    ));
    let identity = match tls::load_or_create(&tls::tls_dir()) {
        Ok(identity) => {
            println!("tls: hub certificate fingerprint {}", identity.fingerprint);
            Some(identity)
        }
        Err(e) => {
            println!("tls: ERROR {e} -- LAN connections disabled");
            None
        }
    };

    /* Bluetooth setup (issue #36): while a pairing code is on the screen,
     * the phone app can send the WiFi details over Bluetooth and get paired
     * in the same step. Advertised under the same name as the setup
     * hotspot. */
    let fingerprint = identity.as_ref().map(|i| i.fingerprint.clone()).unwrap_or_default();
    let ble = ble::Ble::new(auth.clone(), network_tx.clone(), fingerprint, hotspot.status().ssid);
    tokio::spawn(ble::run(ble));

    /* The WebSocket API (ws.rs), protocol v2: ws://127.0.0.1:8080 for the
     * hub itself, wss://<hub>:8443 (paired clients only) for the LAN. */
    tokio::spawn(ws::run(control, auth, hotspot, identity, network_tx, events_tx, local_clients));

    /* SIGTERM is what systemd sends on stop/restart; SIGINT covers Ctrl-C
     when running this interactively during development. */
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");

    /* Software has nothing else to do but wait for exit signal. */
    tokio::select! {
        _ = sigterm.recv() => println!("received SIGTERM, shutting down"),
        _ = tokio::signal::ctrl_c() => println!("received SIGINT, shutting down"),
    }
}

async fn heartbeat() {
    let mut tick = interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        /* sched_getcpu() is a raw libc call -- the Rust compiler can't verify
         * what happens on the C side, so calling it requires an unsafe block.
         * It's just a read of the current thread's last-scheduled CPU, no
         * memory/aliasing hazard here.
         */
        let core = unsafe { libc::sched_getcpu() };
        println!("backend_daemon heartbeat (running on core {core})");
    }
}
