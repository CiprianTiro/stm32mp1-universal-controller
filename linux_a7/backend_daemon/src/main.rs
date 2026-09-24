use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::interval;

/* `mod state;` / `mod ws;` tell the compiler "compile state.rs / ws.rs as
 * part of this crate" -- this is what actually makes their code exist in
 * the final binary at all. It does NOT run anything in them; nothing in
 * either file executes until something below explicitly spawns it. */
mod health;
mod mqtt;
mod network;
mod rpmsg;
mod shadow;
mod state;
mod store;
mod ws;

/* worker_threads = 2 -- pinned explicitly to match the DK2's 2 physical
 * Cortex-A7 cores, rather than relying on Tokio's auto-detect (which
 * happens to land on the same number here, but this documents the intent). */
#[tokio::main(worker_threads = 2)]
async fn main() {
    println!("backend_daemon starting (arch: {})", std::env::consts::ARCH);

    tokio::spawn(heartbeat());

    /*
     * SETTING UP state.rs'S ACTOR -- THIS IS WHERE THE ANSWER TO "does data
     * get copied when a socket opens" ACTUALLY LIVES. Read this block
     * carefully; it corrects a real mix-up.
     *
     * `tokio::sync::mpsc::channel(32)` creates ONE mailbox -- one shared
     * queue -- and hands back BOTH ends of it as a pair:
     *   - state_tx: the SENDING half. This is just a small handle/ticket
     *     that lets whoever holds it drop messages into the mailbox. It is
     *     NOT the data itself, and cloning it (which ws.rs will do, below
     *     and inside itself) is cheap -- like photocopying a permission
     *     slip, not duplicating a filing cabinet.
     *   - state_rx: the RECEIVING half -- the mailbox's inbox tray itself.
     *     There is only ever ONE of these, and it's about to be handed to
     *     exactly one task (state::run) below. That task becomes the sole
     *     owner of the actual device data -- nobody else ever gets a copy
     *     of state_rx, so nobody else can ever pull messages out of this
     *     mailbox except that one task.
     */
    let (state_tx, state_rx) = tokio::sync::mpsc::channel(32);
    /* state.rs rings this whenever a device changes, so mqtt.rs can report
     * straight away (see state::run's comment). */
    let (state_changed_tx, state_changed_rx) = tokio::sync::watch::channel(());

    /* This is the ONLY place state::run (the actor from state.rs) is ever
     * spawned in the real, running daemon -- tokio::spawn hands state_rx
     * over, and from this point on, the HashMap of device data inside
     * state::run's function body is the one and only copy of it that
     * exists anywhere in the process. */
    /* The device registry (issue #33): loaded from the userfs partition
     * BEFORE the actor starts, so the first client to connect already sees
     * every device from before the restart. Loading is a quick read of one
     * small file, fine to do right here once at start. Every outcome --
     * first start, a damaged file, a fallback to the previous copy -- is
     * logged by load_or_default (see store.rs). */
    let registry = store::Store::new(&store::data_dir(), "devices.json");
    let devices = registry.load_or_default("device registry", state::decode_registry);
    /* No file name here: when the previous copy had to be used, the line
     * store.rs logged just before says so (and names the file). */
    println!("state: {} device(s) loaded", devices.len());
    let save_tx = store::writer(registry, state::REGISTRY_SCHEMA);
    tokio::spawn(state::run(state_rx, state_changed_tx, devices, save_tx));

    /* A second, completely separate mailbox for rpmsg.rs's actor -- this is
     * NOT state_tx again. rpmsg.rs doesn't manage the generic "device
     * property bag" state.rs owns; it manages one specific piece of real
     * hardware (the M4's LED) that only makes sense to talk to via a direct
     * command/reply round trip, not a stored property. See rpmsg.rs's own
     * header comment for why this stays a separate actor instead of being
     * folded into state.rs's Msg enum.
     *
     * led_tx/led_rx is a `watch` channel: rpmsg.rs writes the LED's latest
     * known state into it, mqtt.rs reads it (see rpmsg::run's comment). */
    let (rpmsg_tx, rpmsg_rx) = tokio::sync::mpsc::channel(8);
    let (led_tx, led_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(rpmsg::run(rpmsg_rx, led_tx));

    /* Same pattern again: mqtt.rs gets its own clone of state_tx, so it can
     * both publish periodic state snapshots (asking state.rs via
     * GetAllDevices) and apply incoming commands (via UpdateDevice) --
     * talking to the exact same single actor as everyone else here, never a
     * copy of it. It also gets the LED's channels, so the real LED shows up
     * in the cloud as device "ld7" and can be switched from there.
     *
     * local_clients: how many WebSocket clients are connected right now.
     * ws.rs counts, health.rs (inside mqtt.rs) reports it -- one shared
     * number, hence Arc (shared ownership) + AtomicUsize (lock-free). */
    let local_clients = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    /* Which link carries traffic (issue #61): network.rs keeps it up to
     * date, mqtt.rs reconnects when it changes. */
    let (uplink_tx, uplink_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(network::watch_uplink(uplink_tx));
    tokio::spawn(mqtt::run(
        state_tx.clone(),
        rpmsg_tx.clone(),
        led_rx,
        state_changed_rx,
        uplink_rx,
        local_clients.clone(),
    ));

    /* ws.rs is the last user of state_tx, so it gets the original handle
     * moved in (no .clone() needed) -- same reasoning as before, ws.rs's
     * server will go on to clone THIS handle again once per connected
     * client (see ws.rs's ws_handler/handle_socket), and it now also gets
     * rpmsg_tx to forward LED commands from those same clients on to the
     * M4. */
    /* The network actor (issue #61): Ethernet/WiFi status, WiFi scan,
     * connect, forget, country. Only ws.rs talks to it. */
    let (network_tx, network_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(network::run(network_rx));

    tokio::spawn(ws::run(state_tx, rpmsg_tx, network_tx, local_clients));

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
