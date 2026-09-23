/* mqtt.rs -- cloud sync over MQTT, shaped like AWS IoT Core (issue #26).
 *
 * What it does:
 *   - connects to a broker over TLS, proving the board's identity with its
 *     own X.509 client certificate (mutual TLS -- no username/password);
 *   - keeps an AWS-style "Device Shadow" in sync: periodically publishes
 *     the board's device states as the shadow's "reported" section, and
 *     applies "delta" messages (what the cloud WANTS to differ from what's
 *     reported) to state.rs, exactly like a WebSocket UpdateDevice would.
 *
 * The topics and JSON are AWS IoT's real Device Shadow format, so the same
 * code works against AWS and against the development Mosquitto broker in
 * tools/mqtt-dev-broker/ -- switching is only a config change.
 *
 * Everything local (touchscreen, LAN WebSocket, M4) works without this:
 * if no broker is configured, or the cloud is unreachable, the rest of
 * the daemon never notices.
 *
 * Shadow document shape used here (all devices under one "devices" key):
 *   reported:  {"state":{"reported":{"devices":{"lamp-1":{"on":true}}}}}
 *   delta:     {"version":7,"state":{"devices":{"lamp-1":{"on":false}}},...}
 */

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

use crate::state::{DeviceId, DeviceState, Msg};

/* Standard port for MQTT over TLS (what AWS IoT uses). */
const DEFAULT_PORT: u16 = 8883;

/* Where the board's certificate files live unless overridden. /usr/local
 * is the DK2's separate "userfs" partition: it survives reflashing the
 * rootfs and stays writable once the rootfs becomes read-only (#18). The
 * certificates are per device, so they must NOT be baked into the image. */
const DEFAULT_CERT_DIR: &str = "/usr/local/etc/universal-controller/mqtt";

/* How often the full state is published even if nothing asked for it. */
const REPORT_INTERVAL: Duration = Duration::from_secs(10);

/* Reconnect delays: start fast, double on every failure, cap at a minute.
 * A broker that's down for hours then costs one attempt per minute
 * instead of one per second. */
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

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

/* AWS IoT's topic names for one thing's (classic, unnamed) shadow. */
struct Topics {
    /* we publish our reported state here */
    update: String,
    /* the broker/cloud tells us here what differs from desired */
    delta: String,
    /* we publish an empty message here to ask for the whole shadow... */
    get: String,
    /* ...and the answer arrives here (AWS only -- the dev broker has no
     * shadow service, so nothing ever answers; harmless) */
    get_accepted: String,
}

impl Topics {
    fn new(thing: &str) -> Self {
        let base = format!("$aws/things/{thing}/shadow");
        Topics {
            update: format!("{base}/update"),
            delta: format!("{base}/update/delta"),
            get: format!("{base}/get"),
            get_accepted: format!("{base}/get/accepted"),
        }
    }
}

/* The part of the shadow this daemon owns: every device's properties. */
#[derive(serde::Deserialize, Default)]
struct DevicesDoc {
    #[serde(default)]
    devices: HashMap<DeviceId, DeviceState>,
}

/* A message on .../update/delta. Only "state" matters here; AWS also sends
 * version/timestamp/metadata, which serde ignores. */
#[derive(serde::Deserialize)]
struct DeltaMsg {
    state: DevicesDoc,
}

/* A message on .../get/accepted: the whole shadow. Only its "delta" part
 * (desired-but-not-yet-reported changes, e.g. commands sent while the
 * board was offline) needs applying. */
#[derive(serde::Deserialize)]
struct GetAcceptedMsg {
    state: GetAcceptedState,
}

#[derive(serde::Deserialize)]
struct GetAcceptedState {
    #[serde(default)]
    delta: Option<DevicesDoc>,
}

pub async fn run(state_tx: mpsc::Sender<Msg>) {
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
    let (client, mut eventloop) = AsyncClient::new(options, 10);

    let topics = Topics::new(&config.thing);
    tokio::spawn(report_periodically(
        client.clone(),
        state_tx.clone(),
        topics.update.clone(),
    ));

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
     * Note the `try_` calls (try_subscribe/try_publish) inside this loop:
     * they queue the request without waiting. The awaiting versions would
     * wait for room in rumqttc's request queue -- but that queue is only
     * emptied by eventloop.poll(), i.e. by this very loop, so waiting
     * here could deadlock. */
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                println!("mqtt: connected to {broker}");
                last_error = None;
                backoff = BACKOFF_MIN;
                /* Subscriptions don't survive a reconnect (clean session),
                 * so (re)subscribe on EVERY connect, not once at startup.
                 * Then ask for the full shadow, to catch commands that were
                 * sent while the board was offline. */
                let _ = client.try_subscribe(topics.delta.as_str(), QoS::AtLeastOnce);
                let _ = client.try_subscribe(topics.get_accepted.as_str(), QoS::AtLeastOnce);
                let _ = client.try_publish(topics.get.as_str(), QoS::AtLeastOnce, false, "");
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let devices = if publish.topic == topics.delta {
                    serde_json::from_slice::<DeltaMsg>(&publish.payload)
                        .map(|m| m.state.devices)
                        .map_err(|e| e.to_string())
                } else if publish.topic == topics.get_accepted {
                    serde_json::from_slice::<GetAcceptedMsg>(&publish.payload)
                        .map(|m| m.state.delta.unwrap_or_default().devices)
                        .map_err(|e| e.to_string())
                } else {
                    continue;
                };
                match devices {
                    Ok(devices) if !devices.is_empty() => {
                        apply_desired(devices, &state_tx).await;
                        /* Report right away rather than at the next tick:
                         * the cloud keeps showing a delta until the device
                         * reports that it has caught up. */
                        let snapshot = reported_payload(&state_tx).await;
                        if let Some(payload) = snapshot {
                            let _ = client.try_publish(
                                topics.update.as_str(),
                                QoS::AtLeastOnce,
                                false,
                                payload,
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) => println!("mqtt: ignoring malformed message on {}: {e}", publish.topic),
                }
            }
            Ok(_) => {}
            Err(e) => {
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

/* Applies the cloud's desired changes, one device at a time, through the
 * same state.rs message a WebSocket client would send. state.rs merges
 * properties, which matches the delta's semantics (only changed fields). */
async fn apply_desired(devices: HashMap<DeviceId, DeviceState>, state_tx: &mpsc::Sender<Msg>) {
    for (id, properties) in devices {
        println!("mqtt: applying desired state for {id}");
        let _ = state_tx.send(Msg::UpdateDevice { id, properties }).await;
    }
}

/* Builds the shadow "reported" document from state.rs's current snapshot.
 * `None` only if state.rs has stopped (the daemon is shutting down). */
async fn reported_payload(state_tx: &mpsc::Sender<Msg>) -> Option<Vec<u8>> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state_tx.send(Msg::GetAllDevices { reply: reply_tx }).await.ok()?;
    let devices = reply_rx.await.unwrap_or_default();
    let doc = serde_json::json!({ "state": { "reported": { "devices": devices } } });
    serde_json::to_vec(&doc).ok()
}

/* Publishes the reported state every REPORT_INTERVAL. Uses try_publish:
 * while disconnected, rumqttc's small request queue fills up and further
 * reports are simply dropped instead of piling up -- only the latest state
 * matters, and it's re-sent on the next tick anyway. */
async fn report_periodically(client: AsyncClient, state_tx: mpsc::Sender<Msg>, topic: String) {
    let mut tick = tokio::time::interval(REPORT_INTERVAL);
    loop {
        tick.tick().await;
        let Some(payload) = reported_payload(&state_tx).await else {
            break;
        };
        let _ = client.try_publish(topic.as_str(), QoS::AtLeastOnce, false, payload);
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
