/* mqtt.rs -- cloud sync over MQTT, shaped like AWS IoT Core (issue #26).
 *
 * What it does:
 *   - connects to a broker over TLS, proving the board's identity with its
 *     own X.509 client certificate (mutual TLS -- no username/password);
 *   - keeps AWS-style "Device Shadows" in sync: one NAMED shadow per
 *     device (issue #31, see shadow.rs for the layout), published whenever
 *     that device changes; "delta" messages (what the cloud WANTS to
 *     differ from what's reported) are applied to state.rs, exactly like a
 *     WebSocket UpdateDevice would; deleting a device's shadow in the cloud
 *     removes the device, and removing a device locally deletes its shadow;
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
 *   reported:  {"state":{"reported":{"on":true}}}
 *   delta:     {"version":7,"state":{"on":false},...}
 *   after carrying out a delta, the report also clears the command:
 *              {"state":{"reported":{"on":false},"desired":null}}
 *
 * One device is special: "ld7", the board's real LED (driven by the M4).
 * It doesn't live in state.rs -- its truth is whatever the M4 says -- so
 * it's added to the device list from rpmsg.rs's latest known state, and a
 * delta for it becomes an rpmsg SetLed command instead of a state.rs
 * update. Deleting its shadow just recreates it -- it's hardware. Tapping the touchscreen is therefore reported to the cloud, and
 * a cloud command switches the physical LED (and the screen follows).
 */

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

use crate::health;
use crate::rpmsg;
use crate::shadow;
use crate::state::{DeviceId, DeviceState, Msg};
use crate::store;

/* The shadow name of the board's real LED (LD7 on the DK2 silkscreen). */
const LED_DEVICE_ID: &str = "ld7";

/* How long to wait for the M4 to answer a command from the cloud. The M4
 * normally answers in well under a millisecond; this only matters if it
 * hangs, so MQTT isn't stalled forever along with it. */
const M4_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

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

/* `rpmsg_tx` sends LED commands to the M4 actor; `led_rx` reads the LED's
 * latest known state (see rpmsg::run for the watch channel);
 * `state_changed_rx` wakes us when any device in state.rs changes;
 * `local_clients` is ws.rs's count of connected clients (for health.rs). */
pub async fn run(
    state_tx: mpsc::Sender<Msg>,
    rpmsg_tx: mpsc::Sender<rpmsg::Cmd>,
    led_rx: watch::Receiver<Option<bool>>,
    state_changed_rx: watch::Receiver<()>,
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
        state_tx: state_tx.clone(),
        led_rx: led_rx.clone(),
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
        rpmsg_tx.clone(),
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
    loop {
        match eventloop.poll().await {
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
                handle_event(id, event, &publish.payload, &state_tx, &rpmsg_tx, &sync_tx).await;
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
async fn handle_event(
    id: DeviceId,
    event: shadow::Event,
    payload: &[u8],
    state_tx: &mpsc::Sender<Msg>,
    rpmsg_tx: &mpsc::Sender<rpmsg::Cmd>,
    sync_tx: &mpsc::Sender<Sync>,
) {
    let command = match event {
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
    match command {
        Ok(Some(properties)) if !properties.is_empty() => {
            println!("mqtt: applying desired state for {id}");
            if id == LED_DEVICE_ID {
                set_led(&properties, rpmsg_tx).await;
            } else {
                /* state.rs merges properties, which matches a delta's
                 * meaning (only the fields that should change). */
                let _ = state_tx
                    .send(Msg::UpdateDevice {
                        id: id.clone(),
                        properties,
                    })
                    .await;
            }
            let _ = sync_tx.try_send(Sync::CommandDone(id));
        }
        Ok(_) => {} /* nothing pending */
        Err(e) => println!("mqtt: ignoring malformed message for {id}: {e}"),
    }
}

/* Turns a delta for "ld7" into an M4 command. The only property the LED
 * has is "on" (true/false); anything else is reported and ignored. We wait
 * for the M4's answer so that the report sent right after this already
 * contains the LED's new state (rpmsg.rs updates the watch channel before
 * it replies). */
async fn set_led(properties: &DeviceState, rpmsg_tx: &mpsc::Sender<rpmsg::Cmd>) {
    let Some(on) = properties.get("on").and_then(|v| v.as_bool()) else {
        println!("mqtt: {LED_DEVICE_ID} only supports {{\"on\": true|false}}, ignoring {properties:?}");
        return;
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if rpmsg_tx.send(rpmsg::Cmd::SetLed { on, reply: reply_tx }).await.is_err() {
        return;
    }
    /* Three layers of "did it work": the timeout (Err = the M4 took too
     * long), the oneshot (Err = the actor dropped the reply), and the
     * actor's own answer (Err = couldn't reach the M4). */
    match tokio::time::timeout(M4_REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(_))) => {}
        Ok(Ok(Err(e))) => println!("mqtt: could not switch {LED_DEVICE_ID}: {e}"),
        Ok(Err(_)) | Err(_) => println!("mqtt: no answer from the M4 for {LED_DEVICE_ID}"),
    }
}

/* Where the reporting task gets its data from. Cloning is cheap: a channel
 * handle, a watch receiver, and an Arc (a shared pointer, not a copy).
 *
 * `health` sits behind a Mutex out of caution, even though only the
 * reporting task uses it now; the lock is only held for the few
 * microseconds of Sampler::sample(), never across an .await. */
struct Reporter {
    state_tx: mpsc::Sender<Msg>,
    led_rx: watch::Receiver<Option<bool>>,
    health: Arc<Mutex<health::Sampler>>,
}

impl Reporter {
    /* Every device the hub has right now: state.rs's devices plus the real
     * LED, if we know its state yet (None: the M4 hasn't answered yet, so
     * we say nothing rather than guess). `None` only if state.rs has
     * stopped (the daemon is shutting down). */
    async fn devices(&self) -> Option<HashMap<DeviceId, DeviceState>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.state_tx.send(Msg::GetAllDevices { reply: reply_tx }).await.ok()?;
        let mut devices = reply_rx.await.ok()?;
        /* Copy the LED value out in one statement: borrow() holds a read
         * lock on the watch channel, which must not be kept any longer. */
        let led = *self.led_rx.borrow();
        if let Some(on) = led {
            devices.insert(
                LED_DEVICE_ID.to_string(),
                HashMap::from([("on".to_string(), serde_json::json!(on))]),
            );
        }
        Some(devices)
    }

    /* Removes a device from state.rs and waits until that has actually
     * happened: state.rs handles its messages strictly in order, so once
     * the GetDevice sent right after RemoveDevice is answered, the removal
     * is done. false only if state.rs has stopped. */
    async fn remove_device(&self, id: &DeviceId) -> bool {
        if self.state_tx.send(Msg::RemoveDevice { id: id.clone() }).await.is_err() {
            return false;
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let asked = self.state_tx.send(Msg::GetDevice { id: id.clone(), reply: reply_tx }).await;
        asked.is_ok() && reply_rx.await.is_ok()
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

/* The ONLY place that publishes. It keeps `published`: the properties of
 * each device as last sent to the cloud. Whenever something might have
 * changed, it compares that with what the hub has now (shadow::changes)
 * and sends exactly the difference: a report for each new or changed
 * device, a delete for each device that's gone. So devices are only sent
 * when they change, and nothing needs to tell this task WHICH device
 * changed.
 *
 * It wakes up on:
 *   - a Sync message from run()  (reconnect, command done, shadow deleted)
 *   - the LED or state.rs changing
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
 * `published` (with empty properties), so the first Resync deletes the
 * ones whose device is gone and re-reports the rest. Whenever the SET of
 * ids in `published` changes, it's saved again through `shadows_tx`. */
#[allow(clippy::too_many_arguments)] /* all distinct handles; a struct would just rename them */
async fn reporting_task(
    client: AsyncClient,
    reporter: Reporter,
    rpmsg_tx: mpsc::Sender<rpmsg::Cmd>,
    mut state_changed_rx: watch::Receiver<()>,
    mut sync_rx: mpsc::Receiver<Sync>,
    topics: shadow::Topics,
    connected: Arc<AtomicBool>,
    interval: Duration,
    known_shadows: BTreeSet<DeviceId>,
    shadows_tx: watch::Sender<Vec<u8>>,
) {
    /* The LED is left out: it's built-in hardware, its shadow is never
     * stale. And while the M4 hasn't answered yet, ld7 is missing from the
     * device list, so a seeded ld7 would be deleted at the first Resync
     * and recreated a moment later. */
    let mut published: HashMap<DeviceId, DeviceState> = known_shadows
        .into_iter()
        .filter(|id| id != LED_DEVICE_ID)
        .map(|id| (id, DeviceState::new()))
        .collect();
    /* The id set last handed to shadows_tx. */
    let mut saved_names: BTreeSet<DeviceId> = published.keys().cloned().collect();
    /* Our own receiver for the LED, only to be woken by its changes (the
     * Reporter's copy is for reading the value). */
    let mut led_rx = reporter.led_rx.clone();
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
                    sync_devices(&publisher, &topics, &mut published, &devices, None).await;
                    /* ...then send every device again: the cloud may have
                     * changed while we were away (e.g. a shadow deleted and
                     * recreated), so don't trust `published` for those. */
                    published.retain(|id, _| !devices.contains_key(id));
                    sync_devices(&publisher, &topics, &mut published, &devices, None).await;
                    for id in devices.keys() {
                        publisher.send(&topics.device_get(id), Vec::new()).await;
                    }
                    let system = reporter.system(devices.len());
                    publisher.send(&topics.classic_update(), shadow::classic_report(&system, true)).await;
                    continue;
                }
                Sync::CommandDone(id) => {
                    let Some(devices) = reporter.devices().await else { break };
                    sync_devices(&publisher, &topics, &mut published, &devices, Some(&id)).await;
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
                    if id == LED_DEVICE_ID {
                        /* Real hardware can't be removed: fall through to
                         * the sync below, which sees ld7 as new and
                         * recreates its shadow. */
                        println!("mqtt: shadow of {LED_DEVICE_ID} was deleted; it's built-in hardware, recreating it");
                    } else {
                        /* Remove the device here and WAIT until state.rs
                         * has done it, before this task does anything else
                         * -- so no sync can see it still there and recreate
                         * its shadow. The bell state.rs rings afterwards
                         * finds nothing to do: gone here, gone in the cloud. */
                        if reporter.remove_device(&id).await {
                            println!("mqtt: shadow of {id} was deleted in the cloud, device removed");
                        }
                        continue;
                    }
                }
            },
            _ = tick.tick() => {
                /* Nobody has asked the M4 about the LED yet (e.g. the
                 * touchscreen UI isn't running): ask once per tick until it
                 * answers, so the LED appears in the cloud anyway. The
                 * answer lands in led_rx through rpmsg.rs's watch channel. */
                if led_rx.borrow().is_none() {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if rpmsg_tx.send(rpmsg::Cmd::GetLedState { reply: reply_tx }).await.is_ok() {
                        let _ = tokio::time::timeout(M4_REPLY_TIMEOUT, reply_rx).await;
                    }
                }
                let Some(devices) = reporter.devices().await else { break };
                let system = reporter.system(devices.len());
                publisher.send(&topics.classic_update(), shadow::classic_report(&system, false)).await;
                continue;
            }
            Ok(()) = led_rx.changed() => {}
            Ok(()) = state_changed_rx.changed() => {}
        }
        /* A device may have changed: send the difference. mark_unchanged
         * marks the current values as seen, so changed() above only fires
         * again for a NEW change. */
        led_rx.mark_unchanged();
        state_changed_rx.mark_unchanged();
        let Some(devices) = reporter.devices().await else { break };
        sync_devices(&publisher, &topics, &mut published, &devices, None).await;
    }
}

/* Brings the cloud from `published` to `current`, and records what was
 * sent. `command_done`: a device whose cloud command was just carried out
 * -- its report also clears "desired" (see shadow::device_report), and it's
 * sent even if its properties didn't change.
 *
 * Nothing is recorded while disconnected, so the next Resync still sends
 * it. A change made BY the cloud can occasionally be reported twice (once
 * when the device changes, once for CommandDone) -- same content, harmless. */
async fn sync_devices(
    publisher: &Publisher<'_>,
    topics: &shadow::Topics,
    published: &mut HashMap<DeviceId, DeviceState>,
    current: &HashMap<DeviceId, DeviceState>,
    command_done: Option<&DeviceId>,
) {
    let mut changes = shadow::changes(published, current);
    if let Some(id) = command_done {
        if current.contains_key(id) && !changes.updated.contains(id) {
            changes.updated.push(id.clone());
        }
    }
    for id in changes.updated {
        let properties = &current[&id];
        let clear = command_done == Some(&id);
        if publisher.send(&topics.device_update(&id), shadow::device_report(properties, clear)).await {
            published.insert(id, properties.clone());
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
