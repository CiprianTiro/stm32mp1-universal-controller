/* mqtt.rs -- cloud sync over MQTT, shaped like AWS IoT Core (issue #26).
 *
 * What it does:
 *   - connects to a broker over TLS, proving the board's identity with its
 *     own X.509 client certificate (mutual TLS -- no username/password);
 *   - keeps AWS-style "Device Shadows" in sync: one NAMED shadow per
 *     device (issue #31, see shadow.rs for the layout), published whenever
 *     that device changes; "delta" messages (what the cloud WANTS to
 *     differ from what's reported) become commands through control.rs,
 *     checked exactly like a command from the touchscreen (issue #34);
 *     deleting a device's shadow in the cloud removes the device, and
 *     removing a device locally deletes its shadow;
 *   - reports the hub's own health (health.rs, #29) to the classic shadow
 *     every report interval.
 *
 * The topics and JSON are AWS IoT's real Device Shadow format, so the same
 * code works against AWS and against the development Mosquitto broker in
 * tools/mqtt-dev-broker/ -- switching is only a config change.
 *
 * Everything local (touchscreen, LAN WebSocket, M4) works without this:
 * if no broker is configured, or the cloud is unreachable, the rest of
 * the daemon never notices.
 *
 * Documents (per device, on $aws/things/<thing>/shadow/name/<id>/...):
 *   reported:  {"state":{"reported":{"name":..., "room":..., "template":...,
 *                                    "capabilities":{"switch":{"on":true}}}}}
 *   delta:     {"version":7,"state":{"capabilities":{"switch":{"on":false}}}}
 *   after carrying out a delta, the report also clears the command:
 *              {"state":{"reported":{...},"desired":null}}
 *
 * The board's LED ("ld7") is an ordinary device since #34 (control.rs
 * drives it); nothing here treats it specially. Deleting its shadow just
 * recreates it: state.rs refuses to remove built-in hardware.
 */

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::control::Control;
use crate::health;
use crate::shadow;
use crate::state::DeviceId;
use crate::store;

/* Standard port for MQTT over TLS (what AWS IoT uses). */
const DEFAULT_PORT: u16 = 8883;

/* Where the board's certificate files live unless overridden. /usr/local
 * is the DK2's separate "userfs" partition: it survives reflashing the
 * rootfs and stays writable once the rootfs becomes read-only (#18). The
 * certificates are per device, so they must NOT be baked into the image. */
const DEFAULT_CERT_DIR: &str = "/usr/local/etc/universal-controller/mqtt";

/* How often the hub's health (`system`, classic shadow) is published.
 * Devices are NOT repeated on this timer -- only sent when they change. Overridable with MQTT_REPORT_INTERVAL (seconds) in the
 * on-device env file: 10 s is ~260k messages/month per board -- fine for
 * one, but a fleet would rather use 60. */
const DEFAULT_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/* Reconnect delays: start fast, double on every failure, cap at a minute.
 * A broker that's down for hours then costs one attempt per minute
 * instead of one per second. */
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/* rumqttc's request queue (messages waiting to be sent). A resync needs
 * ~2 per device plus 3 subscribes; 64 leaves room for a few dozen devices
 * before the reporting task simply waits its turn. */
const REQUEST_QUEUE: usize = 64;

/* How long to wait for NTP before connecting anyway -- see
 * wait_for_time_sync(). */
const TIME_SYNC_MAX_WAIT: Duration = Duration::from_secs(60);

/* Everything read from the environment -- in production, from
 * /etc/default/backend-daemon via the systemd unit's EnvironmentFile=. */
struct Config {
    host: String,
    port: u16,
    /* The device's name: MQTT client ID, the "<thing>" in the topics, and
     * (for the broker's access rules) the certificate's Common Name. */
    thing: String,
    ca_file: String,
    cert_file: String,
    key_file: String,
    report_interval: Duration,
}

impl Config {
    /* `Ok(None)` = no broker configured, sync deliberately off.
     * `Err` = half-configured, which is a mistake worth reporting. */
    fn from_env() -> Result<Option<Config>, String> {
        let Ok(host) = std::env::var("MQTT_BROKER_HOST") else {
            return Ok(None);
        };
        let port = match std::env::var("MQTT_BROKER_PORT") {
            Ok(p) => p.parse().map_err(|_| format!("MQTT_BROKER_PORT is not a port: {p}"))?,
            Err(_) => DEFAULT_PORT,
        };
        let thing = std::env::var("MQTT_THING_NAME")
            .map_err(|_| "MQTT_BROKER_HOST is set but MQTT_THING_NAME is not".to_string())?;
        let report_interval = match std::env::var("MQTT_REPORT_INTERVAL") {
            Ok(v) => match v.parse::<u64>() {
                Ok(secs) if secs >= 1 => Duration::from_secs(secs),
                _ => return Err(format!("MQTT_REPORT_INTERVAL must be whole seconds >= 1, got {v}")),
            },
            Err(_) => DEFAULT_REPORT_INTERVAL,
        };
        let file = |var: &str, default: &str| {
            std::env::var(var).unwrap_or_else(|_| format!("{DEFAULT_CERT_DIR}/{default}"))
        };
        Ok(Some(Config {
            host,
            port,
            ca_file: file("MQTT_CA_FILE", "ca.crt"),
            cert_file: file("MQTT_CERT_FILE", "device.crt"),
            key_file: file("MQTT_KEY_FILE", "device.key"),
            thing,
            report_interval,
        }))
    }

    /* Reads the three PEM files into rumqttc's TLS settings: the CA that
     * signed the broker's certificate (so we can tell the real broker from
     * an impostor), plus our own certificate and private key (so the
     * broker can tell us apart from any other device). */
    fn tls(&self) -> Result<TlsConfiguration, String> {
        let read = |path: &str| std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"));
        Ok(TlsConfiguration::Simple {
            ca: read(&self.ca_file)?,
            alpn: None,
            client_auth: Some((read(&self.cert_file)?, read(&self.key_file)?)),
        })
    }
}

/* Messages from the run() loop (which receives from the cloud) to the
 * reporting task (which is the only place that publishes). One publisher
 * keeps the bookkeeping of "what did we last send" in one place. */
enum Sync {
    /* Just (re)connected: publish everything, and ask each device's shadow
     * for commands that arrived while we were offline. */
    Resync,
    /* A cloud command for this device was carried out: report it and clear
     * its "desired", even if nothing actually changed (e.g. "turn on" for
     * a lamp that was already on -- the command still has to be removed). */
    CommandDone(DeviceId),
    /* This device's shadow was deleted (in the console, or by us). */
    ShadowDeleted(DeviceId),
}

/* `control` reads the devices and carries out cloud commands (control.rs);
 * `state_changed_rx` wakes us when any device in state.rs changes;
 * `uplink_rx` says which network link carries traffic (issue #61, see the
 * run loop); `local_clients` is ws.rs's count of
 * connected clients (for health.rs). */
pub async fn run(
    control: Control,
    state_changed_rx: watch::Receiver<()>,
    uplink_rx: watch::Receiver<Option<String>>,
    local_clients: Arc<AtomicUsize>,
) {
    let config = match Config::from_env() {
        Ok(Some(config)) => config,
        Ok(None) => {
            println!("mqtt: MQTT_BROKER_HOST not set, cloud sync disabled");
            return;
        }
        Err(e) => {
            println!("mqtt: {e} -- cloud sync disabled");
            return;
        }
    };
    /* Missing/unreadable certificate files won't fix themselves by
     * retrying, so report once and stop instead of looping. */
    let tls = match config.tls() {
        Ok(tls) => tls,
        Err(e) => {
            println!("mqtt: {e} -- cloud sync disabled");
            return;
        }
    };

    wait_for_time_sync().await;

    let mut options = MqttOptions::new(config.thing.as_str(), config.host.as_str(), config.port);
    /* 30 s is the shortest keep-alive AWS IoT accepts. */
    options.set_keep_alive(Duration::from_secs(30));
    options.set_transport(Transport::Tls(tls));
    /* The request queue: big enough for a full resync (a report and a get
     * per device, plus the subscribes) -- see reporting_task for why the
     * queue must never fill up. */
    let (client, mut eventloop) = AsyncClient::new(options, REQUEST_QUEUE);

    /* Whether we're connected right now -- set by the loop below, read by
     * the reporting task. Arc = shared ownership between the two tasks;
     * AtomicBool = a bool both can read/write safely without a lock. */
    let connected = Arc::new(AtomicBool::new(false));

    let reporter = Reporter {
        control: control.clone(),
        health: Arc::new(Mutex::new(health::Sampler::new(local_clients))),
    };

    /* Which shadows exist in the cloud, as far as we know (issue #33, see
     * shadow::SHADOWS_SCHEMA for why this is saved). */
    let shadow_list = store::Store::new(&store::data_dir(), "shadows.json");
    let known_shadows = shadow_list.load_or_default("cloud shadow list", shadow::decode_names);
    let shadows_tx = store::writer(shadow_list, shadow::SHADOWS_SCHEMA);

    let (sync_tx, sync_rx) = mpsc::channel(32);
    tokio::spawn(reporting_task(
        client.clone(),
        reporter,
        state_changed_rx,
        sync_rx,
        shadow::Topics::new(&config.thing),
        connected.clone(),
        config.report_interval,
        known_shadows,
        shadows_tx,
    ));

    let topics = shadow::Topics::new(&config.thing);
    let broker = format!("{}:{}", config.host, config.port);
    println!("mqtt: connecting to {broker} as \"{}\"", config.thing);

    /* Logging only on CHANGES: "connected", "connection lost", or a
     * different error than the one last logged. Retrying the same failing
     * connection over and over logs it once, not every attempt. */
    let mut last_error: Option<String> = None;
    let mut backoff = BACKOFF_MIN;

    /* rumqttc reconnects by itself whenever eventloop.poll() is called again
     * after an error; this loop just paces those retries and reacts to
     * what arrives.
     *
     * Nothing in this loop may WAIT on rumqttc's request queue or on the
     * reporting task: the queue is only emptied by eventloop.poll(), i.e.
     * by this very loop, and the reporting task itself waits on that queue.
     * Hence try_subscribe and try_send (queue the request, don't wait). */
    let mut uplink_rx = uplink_rx;
    /* The first value is just the link at start, not a change. */
    uplink_rx.mark_unchanged();
    loop {
        /* Wait for the next MQTT event -- or for the network link to change
         * (issue #61). When traffic moves to another link (cable pulled ->
         * WiFi, or back), the TCP connection to AWS is usually dead: it's
         * tied to the old link's address. Nothing tells the socket, though;
         * MQTT would only notice when a keep-alive goes unanswered, which
         * took 45 s on the DK2. Politely disconnecting doesn't help either:
         * that message goes into the same dead connection and waits there.
         *
         * So the connection is dropped right here: clean() closes it
         * locally and puts unconfirmed messages back in the queue, and the
         * next poll() connects anew -- over the new link. Interrupting
         * poll() for this is safe precisely because the connection it was
         * working on is thrown away. */
        let event = tokio::select! {
            event = eventloop.poll() => event,
            Ok(()) = uplink_rx.changed() => {
                let uplink = uplink_rx.borrow_and_update().clone();
                /* Offline: nothing to reconnect over yet; the next change
                 * (a link coming back) triggers it. */
                if uplink.is_some() && connected.load(Ordering::Relaxed) {
                    println!("mqtt: network link changed, reconnecting over it");
                    connected.store(false, Ordering::Relaxed);
                    eventloop.clean();
                }
                continue;
            }
        };
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                println!("mqtt: connected to {broker}");
                last_error = None;
                backoff = BACKOFF_MIN;
                /* Subscriptions don't survive a reconnect (clean session),
                 * so (re)subscribe on EVERY connect -- and BEFORE the
                 * reporting task starts its resync, so they're first in the
                 * queue. A lost subscribe means the board silently stops
                 * hearing commands (this happened in #26), so a failure is
                 * logged, never ignored. */
                for topic in topics.subscriptions() {
                    if let Err(e) = client.try_subscribe(topic.as_str(), QoS::AtLeastOnce) {
                        println!("mqtt: could not subscribe to {topic}: {e}");
                    }
                }
                connected.store(true, Ordering::Relaxed);
                if sync_tx.try_send(Sync::Resync).is_err() {
                    println!("mqtt: reporting task busy, resync skipped");
                }
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let Some((id, event)) = topics.parse(&publish.topic) else {
                    continue;
                };
                handle_event(id, event, &publish.payload, &control, &sync_tx).await;
            }
            Ok(_) => {}
            Err(e) => {
                connected.store(false, Ordering::Relaxed);
                let message = e.to_string();
                if last_error.as_deref() != Some(message.as_str()) {
                    println!("mqtt: connection to {broker} failed: {message} (retrying with backoff)");
                    last_error = Some(message);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/* Reacts to one message on a device's shadow. */
async fn handle_event(id: DeviceId, event: shadow::Event, payload: &[u8], control: &Control, sync_tx: &mpsc::Sender<Sync>) {
    let desired = match event {
        shadow::Event::Delta => shadow::parse_delta(payload).map(Some),
        shadow::Event::GetAccepted => shadow::parse_get_accepted(payload),
        shadow::Event::DeleteAccepted => {
            /* The reporting task does the removal itself (see
             * Sync::ShadowDeleted) -- that way no publish can slip in
             * between and recreate the shadow while the device still
             * exists here. */
            if sync_tx.try_send(Sync::ShadowDeleted(id.clone())).is_err() {
                println!("mqtt: reporting task busy, deletion of {id} not handled");
            }
            return;
        }
    };
    match desired {
        Ok(Some(desired)) => {
            println!("mqtt: applying desired state for {id}");
            apply_desired(&id, &desired, control).await;
            /* Report and clear "desired" even if the command was refused:
             * otherwise AWS would re-send the same refused command forever. */
            let _ = sync_tx.try_send(Sync::CommandDone(id));
        }
        Ok(None) => {} /* nothing pending */
        Err(e) => println!("mqtt: ignoring malformed message for {id}: {e}"),
    }
}

/* Turns what the cloud wants into commands, one per capability, each
 * checked and carried out by control.rs exactly like a command from the
 * touchscreen. Refusals are logged (the cloud has no one to answer to).
 * Only capabilities can be changed from the cloud; anything else -- e.g.
 * a pre-#34 command like {"on": false} -- is reported and ignored.
 *
 * This runs inside run()'s loop, which also keeps the MQTT connection
 * going, so it must be quick: a virtual device is a message to state.rs,
 * the LED a round trip to the M4 (normally under a millisecond, at most
 * control.rs's 2 s timeout). */
async fn apply_desired(id: &DeviceId, desired: &Value, control: &Control) {
    let Some(fields) = desired.as_object() else {
        println!("mqtt: {id}: the cloud command isn't a JSON object, ignored");
        return;
    };
    for (key, value) in fields {
        let capabilities = match (key.as_str(), value.as_object()) {
            ("capabilities", Some(capabilities)) => capabilities,
            _ => {
                println!("mqtt: {id}: {key:?} can't be changed from the cloud (only \"capabilities\"), ignored");
                continue;
            }
        };
        for (capability, value) in capabilities {
            if let Err(e) = control.command(id, capability, value.clone()).await {
                println!("mqtt: {id}: cloud command for {capability} refused: {e}");
            }
        }
    }
}

/* Where the reporting task gets its data from. Cloning is cheap: channel
 * handles and an Arc (a shared pointer, not a copy).
 *
 * `health` sits behind a Mutex out of caution, even though only the
 * reporting task uses it now; the lock is only held for the few
 * microseconds of Sampler::sample(), never across an .await. */
struct Reporter {
    control: Control,
    health: Arc<Mutex<health::Sampler>>,
}

impl Reporter {
    /* Every device the hub has right now, as the JSON its shadow reports
     * (shadow::reported). `None` only if state.rs has stopped (the daemon
     * is shutting down). */
    async fn devices(&self) -> Option<HashMap<DeviceId, Value>> {
        let list = self.control.list().await.ok()?;
        Some(list.iter().map(|d| (d.id.clone(), shadow::reported(d))).collect())
    }

    /* The hub's health section (see health.rs). A poisoned Mutex (another
     * thread panicked while holding it) can't happen here, but if it did,
     * the Sampler inside is still usable -- into_inner() takes it anyway
     * instead of crashing the report. */
    fn system(&self, device_count: usize) -> serde_json::Value {
        self.health
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sample(device_count)
    }
}

/* The ONLY place that publishes. It keeps `published`: each device's
 * report (shadow::reported) as last sent to the cloud. Whenever something might have
 * changed, it compares that with what the hub has now (shadow::changes)
 * and sends exactly the difference: a report for each new or changed
 * device, a delete for each device that's gone. So devices are only sent
 * when they change, and nothing needs to tell this task WHICH device
 * changed.
 *
 * It wakes up on:
 *   - a Sync message from run()  (reconnect, command done, shadow deleted)
 *   - state.rs changing (any device, the LED included)
 *   - the timer: the hub's `system` report, every `interval`
 *
 * Only while connected: rumqttc's request queue is only emptied once the
 * connection is back. In #26, reports queued during an outage filled it
 * completely, so after reconnecting the cloud first got stale states and
 * the resubscribes no longer fit -- the board never heard commands again.
 * While disconnected nothing is sent; the Resync after reconnecting brings
 * the cloud up to date in one go. Unlike run(), this task CAN wait for
 * room in the queue (publish().await): run() keeps emptying it.
 *
 * Across restarts (issue #33): `known_shadows` is the list of shadows
 * that existed in the cloud when the daemon last ran. They start out in
 * `published` (with an empty report), so the first Resync deletes the
 * ones whose device is gone and re-reports the rest. Whenever the SET of
 * ids in `published` changes, it's saved again through `shadows_tx`. */
#[allow(clippy::too_many_arguments)] /* all distinct handles; a struct would just rename them */
async fn reporting_task(
    client: AsyncClient,
    reporter: Reporter,
    mut state_changed_rx: watch::Receiver<()>,
    mut sync_rx: mpsc::Receiver<Sync>,
    topics: shadow::Topics,
    connected: Arc<AtomicBool>,
    interval: Duration,
    known_shadows: BTreeSet<DeviceId>,
    shadows_tx: watch::Sender<Vec<u8>>,
) {
    let mut published: HashMap<DeviceId, Value> = known_shadows.into_iter().map(|id| (id, Value::Null)).collect();
    /* The id set last handed to shadows_tx. */
    let mut saved_names: BTreeSet<DeviceId> = published.keys().cloned().collect();
    let mut tick = tokio::time::interval(interval);
    let publisher = Publisher {
        client: &client,
        connected: &connected,
    };

    loop {
        /* Every branch below ends up back here, so this one check catches
         * any change to which shadows exist. Comparing the id sets costs
         * microseconds; the (rare) actual save happens in store.rs's
         * background writer. */
        if published.len() != saved_names.len() || !published.keys().all(|id| saved_names.contains(id)) {
            saved_names = published.keys().cloned().collect();
            shadows_tx.send_replace(shadow::encode_names(&saved_names));
        }

        /* select! waits for whichever happens first. `biased` = check the
         * branches in this order instead of randomly, so a Sync message
         * (e.g. "shadow deleted") is always handled before a state change
         * that was caused by it. `changed()` returns Err only if the
         * sending actor is gone; then that branch is simply skipped. */
        tokio::select! {
            biased;
            Some(msg) = sync_rx.recv() => match msg {
                Sync::Resync => {
                    let Some(devices) = reporter.devices().await else { break };
                    /* First delete the shadows of devices removed while we
                     * were offline (still in `published`, gone here)... */
                    sync_devices(&publisher, &topics, &mut published, &devices, None, false).await;
                    /* ...then send every device again: the cloud may have
                     * changed while we were away (e.g. a shadow deleted and
                     * recreated), so don't trust `published` for those.
                     * These first reports also delete the pre-#34 keys
                     * (shadow::device_report's drop_old_keys). */
                    published.retain(|id, _| !devices.contains_key(id));
                    sync_devices(&publisher, &topics, &mut published, &devices, None, true).await;
                    for id in devices.keys() {
                        publisher.send(&topics.device_get(id), Vec::new()).await;
                    }
                    let system = reporter.system(devices.len());
                    publisher.send(&topics.classic_update(), shadow::classic_report(&system, true)).await;
                    continue;
                }
                Sync::CommandDone(id) => {
                    let Some(devices) = reporter.devices().await else { break };
                    sync_devices(&publisher, &topics, &mut published, &devices, Some(&id), false).await;
                    continue;
                }
                Sync::ShadowDeleted(id) => {
                    /* It's gone in the cloud: forget it was ever sent.
                     * AWS also sends this as confirmation when WE deleted
                     * the shadow (device removed locally). We recognise that
                     * echo because sync_devices already took the device out
                     * of `published` when it sent the delete -- nothing
                     * left to do then. */
                    let ours = published.remove(&id).is_none();
                    if ours {
                        continue;
                    }
                    /* Remove the device here and WAIT until state.rs has
                     * done it, before this task does anything else -- so
                     * no sync can see it still there and recreate its
                     * shadow. The bell state.rs rings afterwards finds
                     * nothing to do: gone here, gone in the cloud. */
                    match reporter.control.remove(&id).await {
                        Ok(()) => {
                            println!("mqtt: shadow of {id} was deleted in the cloud, device removed");
                            continue;
                        }
                        /* Built-in hardware (the LED) can't be removed:
                         * fall through to the sync below, which sees the
                         * device as new and recreates its shadow. */
                        Err(e) => println!("mqtt: shadow of {id} was deleted in the cloud, but {e}; recreating it"),
                    }
                }
            },
            _ = tick.tick() => {
                let Some(devices) = reporter.devices().await else { break };
                let system = reporter.system(devices.len());
                publisher.send(&topics.classic_update(), shadow::classic_report(&system, false)).await;
                continue;
            }
            Ok(()) = state_changed_rx.changed() => {}
        }
        /* A device may have changed: send the difference. mark_unchanged
         * marks the current value as seen, so changed() above only fires
         * again for a NEW change. */
        state_changed_rx.mark_unchanged();
        let Some(devices) = reporter.devices().await else { break };
        sync_devices(&publisher, &topics, &mut published, &devices, None, false).await;
    }
}

/* Brings the cloud from `published` to `current`, and records what was
 * sent. `command_done`: a device whose cloud command was just carried out
 * -- its report also clears "desired" (see shadow::device_report), and it's
 * sent even if its report didn't change. `first`: the first report of each
 * device after connecting -- it also deletes the shadow's pre-#34 keys.
 *
 * Nothing is recorded while disconnected, so the next Resync still sends
 * it. A change made BY the cloud can occasionally be reported twice (once
 * when the device changes, once for CommandDone) -- same content, harmless. */
async fn sync_devices(
    publisher: &Publisher<'_>,
    topics: &shadow::Topics,
    published: &mut HashMap<DeviceId, Value>,
    current: &HashMap<DeviceId, Value>,
    command_done: Option<&DeviceId>,
    first: bool,
) {
    let mut changes = shadow::changes(published, current);
    if let Some(id) = command_done {
        if current.contains_key(id) && !changes.updated.contains(id) {
            changes.updated.push(id.clone());
        }
    }
    for id in changes.updated {
        let report = &current[&id];
        let clear = command_done == Some(&id);
        if publisher
            .send(&topics.device_update(&id), shadow::device_report(report, clear, first))
            .await
        {
            published.insert(id, report.clone());
        }
    }
    for id in changes.removed {
        if publisher.send(&topics.device_delete(&id), Vec::new()).await {
            println!("mqtt: {id} was removed, deleting its shadow");
            published.remove(&id);
        }
    }
}

/* Publishes only while connected (see reporting_task for why). */
struct Publisher<'a> {
    client: &'a AsyncClient,
    connected: &'a AtomicBool,
}

impl Publisher<'_> {
    /* true = handed to rumqttc (it delivers it once the broker is
     * reachable); false = not sent, because we're offline. */
    async fn send(&self, topic: &str, payload: Vec<u8>) -> bool {
        if !self.connected.load(Ordering::Relaxed) {
            return false;
        }
        match self.client.publish(topic, QoS::AtLeastOnce, false, payload).await {
            Ok(()) => true,
            Err(e) => {
                println!("mqtt: could not publish to {topic}: {e}");
                false
            }
        }
    }
}

/* Waits (up to TIME_SYNC_MAX_WAIT) until NTP has set the clock. Needed
 * because the DK2 has no battery-backed clock: it boots with an old date,
 * and TLS rejects certificates that aren't valid "yet" at that date.
 * Waiting avoids a burst of confusing TLS errors at every boot. Gives up
 * waiting rather than blocking forever, so a board without network time
 * still tries (the backoff handles the rest). */
async fn wait_for_time_sync() {
    if clock_synced() {
        return;
    }
    println!("mqtt: waiting for NTP time sync (TLS certificates are date-checked)");
    let start = Instant::now();
    while !clock_synced() {
        if start.elapsed() >= TIME_SYNC_MAX_WAIT {
            println!("mqtt: still no NTP sync after {}s, connecting anyway", TIME_SYNC_MAX_WAIT.as_secs());
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    println!("mqtt: clock synced");
}

/* Asks the kernel whether the system clock is NTP-synchronized. Every NTP
 * client (systemd-timesyncd on the board, chrony/ntpd elsewhere) reports
 * sync to the kernel by clearing the STA_UNSYNC flag; adjtimex() with no
 * changes requested just reads that status back. This is the same check
 * `timedatectl` uses for "System clock synchronized: yes". */
fn clock_synced() -> bool {
    // SAFETY: an all-zero timex means "modes = 0", i.e. read-only query;
    // adjtimex only fills in the struct we own.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut tx) };
    state != libc::TIME_ERROR && (tx.status & libc::STA_UNSYNC) == 0
}
