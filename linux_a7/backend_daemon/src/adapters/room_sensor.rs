/*
 * room_sensor.rs -- the adapter for the hub's own room sensor add-on
 * (issue #86; template templates/room-sensor.json; firmware and protocol:
 * firmware_sensor/, PROTOCOL.md there).
 *
 * WHAT IT IS. An ESP32 with a temperature/humidity sensor (SHT3x, AHT20 or
 * SHT21) on a short cable, placed where the room's air is. In the hub it's
 * a `sensor` device: temperature and humidity, new every minute.
 *
 * THE CONNECTION, PAIRING AND SETUP are the IR blaster's (addon.rs): the
 * hub connects with mutual TLS, pinned both ways; a new sensor gets the
 * WiFi over Bluetooth and is paired with the code on its label. Only the
 * names differ (KIND below).
 *
 * THE DEVICE'S TASK:
 *   connect   pinned mutual TLS; online
 *   session   every "reading" event becomes the `sensor` capability; ping
 *             every PING_EVERY, give up after SILENT_LIMIT of silence
 *   lost      offline (or "unauthorized" if the sensor refuses us: pair
 *             again), wait, doubling up to RETRY_MAX, try again
 *
 * TEMPERATURE CORRECTION. Even on a cable, a sensor near a warm ESP32 (or
 * in a corner, or in the sun) may read a constant bit high or low. The
 * person compares it with a thermometer they trust and sets a correction:
 * the `options` capability's "temperature_offset", -2 to +1 C in half
 * degrees, kept in the device's config. The hub applies it to every
 * reading (the sensor itself always sends its own value), and it works
 * while the sensor is offline too.
 */
use serde_json::{json, Value};
use std::time::Instant;
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::mpsc;

use super::addon::{self, read_lines, Kind, Stream, PING_EVERY, RETRY_MAX, RETRY_MIN, SILENT_LIMIT};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Device, Health};
use crate::templates::ErrorKind;

/* The sensor's names (addon.rs; the firmware's main.c says the same). The
 * Bluetooth service is the template's provision_ble step's too. */
pub const KIND: Kind = Kind {
    adapter: "room-sensor",
    product: "Room sensor",
    short: "sensor",
    pair_label: "uc-ths-pair-v1",
    ble_service: "0395185a-39fd-49b5-bcfb-ee47c0a2f65b",
    id_key: "sensor_id",
};

/* The correction: its option id (also its config key), and the values a
 * person can pick. Mostly downwards: an ESP32 warms its sensor, it never
 * cools it. */
const OFFSET: &str = "temperature_offset";
const OFFSETS: [&str; 7] = ["-2", "-1.5", "-1", "-0.5", "0", "+0.5", "+1"];

pub struct RoomSensor;

impl Adapter for RoomSensor {
    fn id(&self) -> &'static str {
        "room-sensor"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            host: device.config.get("host").cloned(),
            pin: device.config.get("fingerprint").cloned(),
            offset: device.config.get(OFFSET).map(|v| parse_offset(v)).unwrap_or(0.0),
            last: None,
            hub,
        };
        tokio::spawn(task.run(commands_rx));
        DeviceHandle::new(commands)
    }

    /* The test step: hello through the pinned connection, then the
     * current reading, so the person sees it works ("21.4 °C"). */
    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(async move {
            let (mut probe, mut link) = addon::probe(&KIND, values).await?;
            let reading = link.request("read", json!({})).await?;
            match reading["sensor"].as_str() {
                Some("") | None => {
                    return Err(SetupError::new(
                        ErrorKind::Unsupported,
                        "the sensor module answers, but finds no sensor on its cable: check the wiring (its console: i2cscan)",
                    ))
                }
                Some(name) => {
                    if let Some(t) = reading["temperature"].as_f64() {
                        probe.summary = format!("{} -- {name}: {t:.1} °C", probe.summary);
                    }
                }
            }
            Ok(probe)
        })
    }

    fn action<'a>(&'a self, name: &'a str, values: &'a SetupValues) -> BoxFuture<'a, Result<SetupValues, SetupError>> {
        Box::pin(async move {
            match name {
                "pair" => addon::pair(&KIND, values).await,
                "provision" => addon::provision(&KIND, values).await,
                other => Err(SetupError::new(ErrorKind::Unsupported, format!("room-sensor has no action {other:?}"))),
            }
        })
    }
}

/* "-1.5" -> -1.5; anything unexpected -> no correction. */
fn parse_offset(text: &str) -> f64 {
    text.trim_start_matches('+').parse::<f64>().ok().filter(|v| v.is_finite() && v.abs() <= 5.0).unwrap_or(0.0)
}

/* The offset as its choice is written: -1.5 -> "-1.5", 0.5 -> "+0.5". */
fn offset_choice(offset: f64) -> String {
    let text = format!("{offset}");
    if offset > 0.0 {
        format!("+{text}")
    } else if offset == 0.0 {
        "0".into()
    } else {
        text
    }
}

/* The `options` capability: the correction, as a row of choices. */
fn options_state(offset: f64) -> Value {
    json!({"options": [{
        "id": OFFSET,
        "name": "Temperature correction (°C)",
        "value": offset_choice(offset),
        "choices": OFFSETS,
    }]})
}

/* A "reading" (PROTOCOL.md) as the `sensor` capability, with the
 * correction applied. None if the module found no sensor. */
fn sensor_state(reading: &Value, offset: f64) -> Option<Value> {
    if reading["sensor"].as_str().unwrap_or_default().is_empty() {
        return None;
    }
    let mut readings = serde_json::Map::new();
    if let Some(t) = reading["temperature"].as_f64() {
        /* Two decimals, as the sensor sends them: 21.43 + -0.5 is
         * 20.93, not 20.929999999999999. */
        let corrected = ((t + offset) * 100.0).round() / 100.0;
        readings.insert("temperature".into(), json!({"value": corrected, "unit": "°C"}));
    }
    if let Some(h) = reading["humidity"].as_f64() {
        readings.insert("humidity".into(), json!({"value": h, "unit": "%"}));
    }
    Some(json!({"readings": readings}))
}

/* ------------------------------------------------------------------ */
/* The device's task                                                   */
/* ------------------------------------------------------------------ */

struct Task {
    id: String,
    host: Option<String>,
    /* The sensor's pinned certificate fingerprint (from pairing). */
    pin: Option<String>,
    /* The temperature correction, C. */
    offset: f64,
    /* The latest "reading" event: reported again when the correction
     * changes. */
    last: Option<Value>,
    hub: Hub,
}

/* One live connection: the write half here, lines read by a separate
 * task (addon::read_lines) and handed over through `incoming`. */
struct Conn {
    writer: WriteHalf<Stream>,
    incoming: mpsc::Receiver<Value>,
    next_id: u64,
    last_heard: Instant,
    last_ping: Instant,
}

enum End {
    /* The device was removed (its command channel closed). */
    Stop,
    /* The connection broke. */
    Lost(String),
}

impl Task {
    async fn run(mut self, mut commands: mpsc::Receiver<DeviceCmd>) {
        /* The correction can be set before the sensor ever answers. */
        self.report_options().await;
        let (Some(host), Some(pin)) = (self.host.clone(), self.pin.clone()) else {
            println!("room-sensor: {} has no address or pairing: pair it again", self.id);
            self.hub.set_online(&self.id, Health::Unauthorized).await;
            while let Some(cmd) = commands.recv().await {
                self.command(cmd).await;
            }
            return;
        };

        let mut retry = RETRY_MIN;
        loop {
            match addon::open(&host, Some(&pin)).await {
                Ok((stream, _)) => {
                    retry = RETRY_MIN;
                    let (reader, writer) = tokio::io::split(stream);
                    let (tx, incoming) = mpsc::channel(8);
                    tokio::spawn(read_lines(reader, tx));
                    let conn = Conn {
                        writer,
                        incoming,
                        next_id: 1,
                        last_heard: Instant::now(),
                        last_ping: Instant::now(),
                    };
                    self.hub.set_online(&self.id, Health::Online).await;
                    match self.session(conn, &mut commands).await {
                        End::Stop => return,
                        End::Lost(why) => println!("room-sensor: {}: connection lost: {why}", self.id),
                    }
                    self.hub.set_online(&self.id, Health::Offline).await;
                }
                Err(e) => {
                    /* Refused: the sensor shows another certificate, or
                     * closes on us (it was reset / paired elsewhere). */
                    let health = if e.kind == ErrorKind::Refused { Health::Unauthorized } else { Health::Offline };
                    self.hub.set_online(&self.id, health).await;
                }
            }

            /* Wait before the next attempt, answering commands meanwhile
             * (the correction works offline too). */
            let until = tokio::time::Instant::now() + retry;
            loop {
                tokio::select! {
                    cmd = commands.recv() => match cmd {
                        None => return,
                        Some(cmd) => self.command(cmd).await,
                    },
                    _ = tokio::time::sleep_until(until) => break,
                }
            }
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    async fn session(&mut self, mut conn: Conn, commands: &mut mpsc::Receiver<DeviceCmd>) -> End {
        /* Checked every few seconds: the ping, the silence limit. */
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    None => return End::Stop,
                    Some(cmd) => self.command(cmd).await,
                },
                msg = conn.incoming.recv() => match msg {
                    None => return End::Lost("closed".into()),
                    Some(msg) => {
                        conn.last_heard = Instant::now();
                        self.message(msg).await;
                    }
                },
                _ = tick.tick() => {
                    if conn.last_heard.elapsed() > SILENT_LIMIT {
                        return End::Lost(format!("silent for {} s", SILENT_LIMIT.as_secs()));
                    }
                    if conn.last_ping.elapsed() >= PING_EVERY {
                        conn.last_ping = Instant::now();
                        /* The reply only matters as a sign of life
                         * (last_heard): nobody waits for it. */
                        let line = format!("{}\n", json!({"id": conn.next_id, "op": "ping"}));
                        conn.next_id += 1;
                        if let Err(e) = conn.writer.write_all(line.as_bytes()).await {
                            return End::Lost(e.to_string());
                        }
                    }
                }
            }
        }
    }

    /* A line from the sensor: a "reading" event, or a ping's reply. */
    async fn message(&mut self, msg: Value) {
        if msg["event"] != json!("reading") {
            return;
        }
        if msg["sensor"].as_str().unwrap_or_default().is_empty() {
            /* Logged once per change: the sensor sends this only when it
             * loses the sensor (and on connect). */
            println!("room-sensor: {}: the module finds no sensor on its cable", self.id);
        }
        self.last = Some(msg);
        self.report_reading().await;
    }

    async fn report_reading(&self) {
        let Some(state) = self.last.as_ref().and_then(|r| sensor_state(r, self.offset)) else { return };
        if let Err(e) = self.hub.report(&self.id, "sensor", state).await {
            println!("room-sensor: {}: {e}", self.id);
        }
    }

    async fn report_options(&self) {
        if let Err(e) = self.hub.report(&self.id, "options", options_state(self.offset)).await {
            println!("room-sensor: {}: {e}", self.id);
        }
    }

    /* The only thing a client can change: the correction (options set,
     * already checked against OFFSETS by device.rs). */
    async fn command(&mut self, cmd: DeviceCmd) {
        match cmd {
            DeviceCmd::Action { capability, name, args, reply } if capability == "options" && name == "set" => {
                if args["id"] != json!(OFFSET) {
                    let _ = reply.send(Err(format!("the room sensor has no option {}", args["id"])));
                    return;
                }
                let value = args["value"].as_str().unwrap_or("0");
                self.offset = parse_offset(value);
                self.hub.store_config(&self.id, OFFSET, &offset_choice(self.offset)).await;
                self.report_options().await;
                self.report_reading().await;
                let _ = reply.send(Ok(json!({})));
            }
            cmd => cmd.refuse("A room sensor only measures: there's nothing to switch."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_round_trip() {
        for choice in OFFSETS {
            assert_eq!(offset_choice(parse_offset(choice)), choice);
        }
        assert_eq!(parse_offset("nonsense"), 0.0);
        assert_eq!(parse_offset("99"), 0.0);
    }

    #[test]
    fn readings_become_the_sensor_capability() {
        let reading = json!({"event": "reading", "sensor": "sht21", "temperature": 21.43, "humidity": 48.2, "age_s": 3});
        assert_eq!(
            sensor_state(&reading, -0.5).unwrap(),
            json!({"readings": {"temperature": {"value": 20.93, "unit": "°C"}, "humidity": {"value": 48.2, "unit": "%"}}})
        );
        /* What device.rs accepts as a sensor. */
        let _: crate::device::Sensor = serde_json::from_value(sensor_state(&reading, 0.0).unwrap()).unwrap();
        /* No sensor on the cable: nothing to report. */
        assert!(sensor_state(&json!({"event": "reading", "sensor": ""}), 0.0).is_none());
    }

    #[test]
    fn the_correction_is_a_valid_option() {
        let options: crate::device::Options = serde_json::from_value(options_state(-1.5)).unwrap();
        assert_eq!(options.options[0].value.as_deref(), Some("-1.5"));
        assert!(options.options[0].choices.iter().any(|c| c == "-1.5"));
    }
}
