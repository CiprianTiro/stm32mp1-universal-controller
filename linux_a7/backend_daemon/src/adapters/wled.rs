/*
 * wled.rs -- the adapter for WLED lights (issue #40; template
 * templates/wled.json): LED strips and lamps on an ESP8266/ESP32 running
 * the open-source WLED firmware (https://kno.wled.ge).
 *
 * WLED's LOCAL API needs no account and no pairing (setup pattern P1):
 *   GET  /json/state   the light's state: {"on":true,"bri":128,"seg":[...]}
 *   POST /json/state   change it; with "v":true the reply is the NEW state,
 *                      so a command's reply is also its confirmation
 *   GET  /json/info    name, MAC, version -- the wizard's test step (probe)
 *   ws://…/ws          a WebSocket on which WLED PUSHES its state after
 *                      every change, whoever made it (the WLED app, a
 *                      button, a timer) -- so the hub sees those at once
 *
 * HOW THE CAPABILITIES MAP (device.rs):
 *   switch  {"on"}      <-> "on"
 *   dimmer  {"level"}   <-> "bri" (0-255 <-> 0-100 %). Level 0 means off:
 *                       WLED itself treats "bri":0 as off and keeps the
 *                       last brightness, so we send "on":false instead and
 *                       the level stays where it was. Setting a level >0
 *                       also switches the light on (as any dimmer does).
 *   color   {"hex"}     <-> the first colour of the main segment ("seg")
 *           {"kelvin"}  -> converted to RGB (most WLED strips are RGB).
 *                       WLED only reports RGB back, so the task remembers
 *                       the last kelvin it set, and reports that kelvin
 *                       again as long as WLED's colour is still exactly
 *                       its RGB.
 * Effects, palettes, segments and presets aren't capabilities (yet): the
 * hub leaves them as they are.
 *
 * THE DEVICE'S TASK (see adapters/mod.rs) loops over:
 *   connect  GET /json/state (reachable? report state + online), then
 *            open the WebSocket; if WLED refuses it (builds without
 *            WebSocket support exist, and an ESP8266 allows few clients),
 *            fall back to POLLING /json/state every POLL_EVERY.
 *   session  report every pushed state; carry out commands (HTTP POST);
 *            ping every PING_EVERY -- a WebSocket to an unplugged ESP
 *            doesn't close by itself, silence is the only sign.
 *   lost     report offline, wait RETRY (doubling up to RETRY_MAX), try
 *            again. Commands that arrive meanwhile are still tried (the
 *            light may be back already); one that works reconnects at once.
 */
use futures_util::{SinkExt, StreamExt};
use hyper::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message};

use super::net::{self, WebSocket};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Color, Device, Dimmer, Health, Switch};
use crate::templates::ErrorKind;

/* With a WebSocket: ping this often... */
const PING_EVERY: Duration = Duration::from_secs(15);
/* ...and give up after this long without hearing anything (a pong or a
 * state). Two missed pings plus some slack. */
const SILENT_LIMIT: Duration = Duration::from_secs(40);
/* Without a WebSocket: read the state this often. */
const POLL_EVERY: Duration = Duration::from_secs(10);
/* Waiting between connection attempts: from RETRY_MIN, doubled after
 * every failure, at most RETRY_MAX (a light that's off at the wall for
 * days costs one attempt a minute). */
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);

pub struct Wled;

impl Adapter for Wled {
    fn id(&self) -> &'static str {
        "wled"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            host: device.config.get("host").cloned(),
            has: Has {
                switch: device.capabilities.switch.is_some(),
                dimmer: device.capabilities.dimmer.is_some(),
                color: device.capabilities.color.is_some(),
            },
            kelvin: None,
            hub,
        };
        tokio::spawn(task.run(commands_rx));
        DeviceHandle { commands }
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(probe(values))
    }
}

/* Which capabilities the device was created with: only those are
 * reported (a template may leave out color, for a white-only strip). */
struct Has {
    switch: bool,
    dimmer: bool,
    color: bool,
}

/* One WLED device's task (see the header). */
struct Task {
    id: String,
    /* From the device's config (setup, or discovery's IP update). */
    host: Option<String>,
    has: Has,
    /* The last white temperature set through the hub (see the header). */
    kelvin: Option<u16>,
    hub: Hub,
}

/* Why a session ended. */
#[derive(PartialEq)]
enum End {
    /* The connection is gone: reconnect. */
    Lost,
    /* The device was removed or restarted with a new config (its command
     * channel closed): the task ends. */
    Stopped,
}

impl Task {
    async fn run(mut self, mut commands: mpsc::Receiver<DeviceCmd>) {
        let Some(host) = self.host.clone() else {
            /* Can't happen through the wizard (host is a required input);
             * a hand-edited registry could. Stay put and say why. */
            println!("wled: {} has no address (config \"host\")", self.id);
            self.hub.set_online(&self.id, Health::Offline).await;
            while let Some(DeviceCmd::Command { reply, .. }) = commands.recv().await {
                let _ = reply.send(Err(format!("{} has no address set", self.id)));
            }
            return;
        };

        let mut retry = RETRY_MIN;
        /* Log a failing device once per outage, not once per attempt. */
        let mut said_unreachable = false;
        loop {
            match self.connect(&host).await {
                Ok(ws) => {
                    retry = RETRY_MIN;
                    said_unreachable = false;
                    let how = if ws.is_some() { "live updates" } else { "polling: no WebSocket" };
                    println!("wled: {} connected at {host} ({how})", self.id);
                    if self.session(&host, ws, &mut commands).await == End::Stopped {
                        return;
                    }
                    println!("wled: {} at {host} stopped answering", self.id);
                }
                Err(e) => {
                    if !said_unreachable {
                        println!("wled: {}: {e} (will keep trying)", self.id);
                        said_unreachable = true;
                    }
                }
            }
            self.hub.set_online(&self.id, Health::Offline).await;

            /* Wait before the next attempt -- but keep answering commands. */
            let wait = tokio::time::sleep(retry);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    _ = &mut wait => break,
                    cmd = commands.recv() => match cmd {
                        None => return,
                        /* It answered: it's back, reconnect right away. */
                        Some(cmd) => if self.handle(&host, cmd).await { break },
                    },
                }
            }
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    /* Reachable? Reports the state, then tries for the WebSocket
     * (None = WLED refused it: poll instead). */
    async fn connect(&mut self, host: &str) -> Result<Option<WebSocket>, String> {
        let state = net::http_json(Method::GET, host, "/json/state", None).await?;
        self.report_state(&state).await?;
        let Ok(mut ws) = net::ws_connect(host, "/ws").await else {
            return Ok(None);
        };
        /* WLED greets a new WebSocket client with its full state. Taken
         * HERE, before any command can run: read later, that greeting is
         * older than the command's result and would briefly undo it (seen
         * on the real ESP: "off" reported as on until the next push). */
        if let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout(net::HTTP_TIMEOUT, ws.next()).await {
            self.report_push(&text).await?;
        }
        Ok(Some(ws))
    }

    /* A message WLED pushed: {"state": {...}, "info": {...}}. Anything
     * else (other message kinds exist) is ignored. */
    async fn report_push(&mut self, text: &str) -> Result<(), String> {
        if let Ok(Value::Object(mut pushed)) = serde_json::from_str::<Value>(text) {
            if let Some(state) = pushed.remove("state") {
                return self.report_state(&state).await;
            }
        }
        Ok(())
    }

    async fn session(&mut self, host: &str, mut ws: Option<WebSocket>, commands: &mut mpsc::Receiver<DeviceCmd>) -> End {
        let mut tick = tokio::time::interval(if ws.is_some() { PING_EVERY } else { POLL_EVERY });
        /* An interval's first tick is immediate: use it up (we just read
         * the state). */
        tick.tick().await;
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                message = next_message(&mut ws) => match message {
                    Some(Ok(Message::Text(text))) => {
                        heard = Instant::now();
                        if let Err(e) = self.report_push(&text).await {
                            println!("wled: {}: {e}", self.id);
                        }
                    }
                    /* Pong, ping (tungstenite answers those itself), binary
                     * (WLED's live LED preview, which we never ask for):
                     * only proof of life. */
                    Some(Ok(_)) => heard = Instant::now(),
                    Some(Err(_)) | None => return End::Lost,
                },
                cmd = commands.recv() => match cmd {
                    None => return End::Stopped,
                    Some(cmd) => {
                        self.handle(host, cmd).await;
                    }
                },
                _ = tick.tick() => match ws.as_mut() {
                    Some(ws) => {
                        if heard.elapsed() > SILENT_LIMIT || ws.send(Message::Ping(Vec::new())).await.is_err() {
                            return End::Lost;
                        }
                    }
                    None => {
                        let polled = net::http_json(Method::GET, host, "/json/state", None).await;
                        match polled {
                            Ok(state) => {
                                if let Err(e) = self.report_state(&state).await {
                                    println!("wled: {}: {e}", self.id);
                                }
                            }
                            Err(_) => return End::Lost,
                        }
                    }
                },
            }
        }
    }

    /* Carries out one command and answers it; true if WLED confirmed. */
    async fn handle(&mut self, host: &str, cmd: DeviceCmd) -> bool {
        let DeviceCmd::Command { capability, value, reply } = cmd;
        let result = self.command(host, &capability, &value).await;
        let confirmed = result.is_ok();
        let _ = reply.send(result);
        confirmed
    }

    async fn command(&mut self, host: &str, capability: &str, value: &Value) -> Result<(), String> {
        let (mut body, kelvin) = to_wled(capability, value)?;
        /* "v": reply with the new state -- the confirmation. */
        body["v"] = json!(true);
        let state = net::http_json(Method::POST, host, "/json/state", Some(&body)).await?;
        if capability == "color" {
            self.kelvin = kelvin;
        }
        self.report_state(&state).await
    }

    /* WLED's state -> the hub (and: it answered, so it's online). */
    async fn report_state(&mut self, state: &Value) -> Result<(), String> {
        let state: WledState =
            serde_json::from_value(state.clone()).map_err(|e| format!("not a WLED state: {e}"))?;
        let now = from_wled(&state, self.kelvin);
        let mut reports = Vec::new();
        if self.has.switch {
            reports.push(("switch", json!(now.switch)));
        }
        if self.has.dimmer {
            reports.push(("dimmer", json!(now.dimmer)));
        }
        if let (true, Some(color)) = (self.has.color, now.color) {
            reports.push(("color", json!(color)));
        }
        for (capability, value) in reports {
            self.hub.report(&self.id, capability, value).await?;
        }
        /* Online only after the state: a screen that sees "online" never
         * shows it with the values from before the outage. */
        self.hub.set_online(&self.id, Health::Online).await;
        Ok(())
    }
}

/* The WebSocket's next message; never resolves when there is none
 * (polling), so select! simply never picks that branch. */
async fn next_message(ws: &mut Option<WebSocket>) -> Option<Result<Message, tungstenite::Error>> {
    match ws {
        Some(ws) => ws.next().await,
        None => std::future::pending().await,
    }
}

/* ------------------------------------------------------------------ */
/* WLED's JSON <-> capabilities (pure, unit-tested below)              */
/* ------------------------------------------------------------------ */

/* The parts of WLED's state we use. Not deny_unknown_fields: WLED sends
 * dozens more, and adds some with every version. */
#[derive(Deserialize, Debug)]
struct WledState {
    on: bool,
    bri: u8,
    /* The segment WLED's own UI shows as "the" colour. */
    #[serde(default)]
    mainseg: u32,
    #[serde(default)]
    seg: Vec<Segment>,
}

#[derive(Deserialize, Debug)]
struct Segment {
    #[serde(default)]
    id: u32,
    /* Up to three colour slots, each [r, g, b] or [r, g, b, w]; the first
     * is the one effects like "Solid" show. Values, not numbers: be lenient
     * about the shape, a strange segment mustn't hide on/brightness. */
    #[serde(default)]
    col: Vec<Value>,
}

struct Reported {
    switch: Switch,
    dimmer: Dimmer,
    /* None: WLED sent no usable colour (no segments). */
    color: Option<Color>,
}

fn from_wled(state: &WledState, kelvin: Option<u16>) -> Reported {
    let main = state.seg.iter().find(|s| s.id == state.mainseg).or(state.seg.first());
    let rgb = main.and_then(|s| s.col.first()).and_then(rgb_of);
    let color = rgb.map(|rgb| match kelvin {
        Some(k) if kelvin_to_rgb(k) == rgb => Color {
            hex: None,
            kelvin: Some(k),
        },
        _ => Color {
            hex: Some(format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])),
            kelvin: None,
        },
    });
    Reported {
        switch: Switch { on: state.on },
        dimmer: Dimmer {
            level: level_from_bri(state.bri),
        },
        color,
    }
}

/* [r, g, b, ...] -> [r, g, b]; None if it isn't that. */
fn rgb_of(slot: &Value) -> Option<[u8; 3]> {
    let parts = slot.as_array()?;
    let mut rgb = [0u8; 3];
    for (i, channel) in rgb.iter_mut().enumerate() {
        *channel = u8::try_from(parts.get(i)?.as_u64()?).ok()?;
    }
    Some(rgb)
}

/* A capability command -> the JSON to POST, plus the kelvin to remember
 * (for a color command). The value was already checked by control.rs. */
fn to_wled(capability: &str, value: &Value) -> Result<(Value, Option<u16>), String> {
    match capability {
        "switch" => {
            let on = value["on"].as_bool().ok_or("switch needs {\"on\": true|false}")?;
            Ok((json!({ "on": on }), None))
        }
        "dimmer" => {
            let level = value["level"].as_u64().ok_or("dimmer needs {\"level\": 0-100}")?;
            let level = u8::try_from(level.min(100)).unwrap_or(100);
            Ok(if level == 0 {
                (json!({ "on": false }), None)
            } else {
                (json!({ "on": true, "bri": bri_from_level(level) }), None)
            })
        }
        "color" => {
            let color: Color = serde_json::from_value(value.clone()).map_err(|e| format!("invalid color: {e}"))?;
            let (rgb, kelvin) = match (color.hex, color.kelvin) {
                (Some(hex), _) => (hex_to_rgb(&hex).ok_or_else(|| format!("invalid color {hex:?}"))?, None),
                (None, Some(k)) => (kelvin_to_rgb(k), Some(k)),
                (None, None) => return Err("color needs \"hex\" or \"kelvin\"".into()),
            };
            /* "seg" as an OBJECT (not a list): WLED applies it to every
             * selected segment -- what its own colour picker does. */
            Ok((json!({ "seg": { "col": [rgb] } }), kelvin))
        }
        other => Err(format!("WLED lights have no capability {other:?}")),
    }
}

/* 0-100 % <-> WLED's 0-255, rounded to nearest; a lit strip (bri >= 1)
 * never shows as 0 %. Every level survives the round trip (a test checks). */
fn bri_from_level(level: u8) -> u8 {
    ((u32::from(level) * 255 + 50) / 100) as u8
}

fn level_from_bri(bri: u8) -> u8 {
    if bri == 0 {
        return 0;
    }
    ((u32::from(bri) * 100 + 127) / 255).max(1) as u8
}

/* "#FF8800" -> [255, 136, 0] */
fn hex_to_rgb(hex: &str) -> Option<[u8; 3]> {
    let digits = hex.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/* A white temperature as RGB: the usual approximation of a black body's
 * colour (Tanner Helland's curve fit, good from 1000 to 40000 K). Only an
 * RGB strip's best guess -- warm white from red+green LEDs never looks
 * like a real warm-white LED. */
fn kelvin_to_rgb(kelvin: u16) -> [u8; 3] {
    let t = f64::from(kelvin) / 100.0;
    let red = if t <= 66.0 {
        255.0
    } else {
        329.698_727_446 * (t - 60.0).powf(-0.133_204_759_2)
    };
    let green = if t <= 66.0 {
        99.470_802_586_1 * t.ln() - 161.119_568_166_1
    } else {
        288.122_169_528_3 * (t - 60.0).powf(-0.075_514_849_2)
    };
    let blue = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.517_731_223_1 * (t - 10.0).ln() - 305.044_792_730_7
    };
    /* `as u8` on a float saturates (negative -> 0, >255 -> 255). */
    [red.round() as u8, green.round() as u8, blue.round() as u8]
}

/* ------------------------------------------------------------------ */
/* Setup                                                               */
/* ------------------------------------------------------------------ */

/* The wizard's test step: is there a WLED at "host"? Its MAC (the
 * template's identity -- 12 lowercase hex digits, the same form as mDNS's
 * txt.mac) and its own name come back. */
async fn probe(values: &SetupValues) -> Result<Probe, SetupError> {
    let host = values
        .plain
        .get("host")
        .ok_or_else(|| SetupError::new(ErrorKind::Unsupported, "no address given"))?;
    let info = net::http_json(Method::GET, host, "/json/info", None).await?;
    if info["brand"] != "WLED" {
        return Err(SetupError::new(ErrorKind::Unsupported, format!("{host} answers, but isn't a WLED device")));
    }
    let text = |key: &str| info[key].as_str().unwrap_or_default().to_string();
    let mut probe = Probe {
        summary: format!("WLED {}", text("ver")),
        ..Default::default()
    };
    if !text("mac").is_empty() {
        probe.values.insert("mac".into(), text("mac"));
    }
    /* "WLED" is the factory name: not worth offering. */
    if !matches!(text("name").as_str(), "" | "WLED") {
        probe.name = Some(text("name"));
    }
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::wled_sim::Sim;
    use crate::adapters::Registry;
    use crate::control::Control;
    use crate::device::{Capabilities, Source};
    use crate::secrets::Secrets;
    use crate::state::{self, Event, Outputs};
    use std::sync::Arc;
    use tokio::sync::{broadcast, watch};

    #[test]
    fn every_level_survives_the_round_trip() {
        for level in 0..=100 {
            assert_eq!(level_from_bri(bri_from_level(level)), level, "level {level}");
        }
        assert_eq!(bri_from_level(100), 255);
        assert_eq!(level_from_bri(1), 1);
    }

    #[test]
    fn commands_become_wled_json() {
        assert_eq!(to_wled("switch", &json!({"on": true})).unwrap().0, json!({"on": true}));
        assert_eq!(to_wled("dimmer", &json!({"level": 0})).unwrap().0, json!({"on": false}));
        assert_eq!(to_wled("dimmer", &json!({"level": 50})).unwrap().0, json!({"on": true, "bri": 128}));
        assert_eq!(
            to_wled("color", &json!({"hex": "#FF8800"})).unwrap(),
            (json!({"seg": {"col": [[255, 136, 0]]}}), None)
        );
        let (body, kelvin) = to_wled("color", &json!({"kelvin": 2700})).unwrap();
        assert_eq!(kelvin, Some(2700));
        assert_eq!(body["seg"]["col"][0], json!(kelvin_to_rgb(2700)));
        assert!(to_wled("sensor", &json!({})).is_err());
    }

    #[test]
    fn white_temperatures_look_right() {
        /* Warm: full red, less green, little blue; daylight ~6500 K: near
         * white; cold: blue ahead. */
        let [r, g, b] = kelvin_to_rgb(2700);
        assert!(r == 255 && g > 150 && g < 190 && b < 110, "{r} {g} {b}");
        let [r, g, b] = kelvin_to_rgb(6500);
        assert!(r > 245 && g > 240 && b > 240, "{r} {g} {b}");
        let [r, _, b] = kelvin_to_rgb(10000);
        assert!(b == 255 && r < 210, "{r} {b}");
    }

    #[test]
    fn wled_state_becomes_capabilities() {
        let state: WledState = serde_json::from_value(json!({
            "on": true, "bri": 128, "mainseg": 1, "transition": 7,
            "seg": [
                {"id": 0, "col": [[1, 2, 3], [0, 0, 0], [0, 0, 0]]},
                {"id": 1, "col": [[255, 136, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]], "fx": 0}
            ]
        }))
        .unwrap();
        let now = from_wled(&state, None);
        assert_eq!(now.switch, Switch { on: true });
        assert_eq!(now.dimmer, Dimmer { level: 50 });
        assert_eq!(now.color.unwrap().hex.as_deref(), Some("#FF8800"));

        /* A kelvin set through the hub is reported back as long as the
         * colour is still its RGB... */
        let warm = kelvin_to_rgb(2700);
        let state: WledState =
            serde_json::from_value(json!({"on": true, "bri": 255, "seg": [{"id": 0, "col": [warm]}]})).unwrap();
        assert_eq!(from_wled(&state, Some(2700)).color.unwrap().kelvin, Some(2700));
        /* ...and not once something else changed it. */
        let state: WledState =
            serde_json::from_value(json!({"on": true, "bri": 255, "seg": [{"id": 0, "col": [[0, 0, 255]]}]})).unwrap();
        assert_eq!(from_wled(&state, Some(2700)).color.unwrap().hex.as_deref(), Some("#0000FF"));

        /* No segments: no colour, but on/off and brightness still count. */
        let state: WledState = serde_json::from_value(json!({"on": false, "bri": 0})).unwrap();
        let now = from_wled(&state, None);
        assert!(now.color.is_none() && !now.switch.on && now.dimmer.level == 0);
    }

    /* ---- end to end, against a simulated WLED (wled_sim.rs) ---- */

    /* The hub's parts a device's task talks to: the state actor, the
     * registry with this adapter, and control.rs. */
    struct TestHub {
        control: Control,
        events: broadcast::Receiver<Event>,
        registry: Arc<Registry>,
    }

    async fn hub_with_strip(host: &str) -> TestHub {
        let strip = Device {
            id: "strip".into(),
            name: "Strip".into(),
            room: String::new(),
            template: "wled".into(),
            source: Source::new("wled"),
            config: [("host".to_string(), host.to_string())].into(),
            identity: String::new(),
            online: None,
            capabilities: Capabilities {
                switch: Some(Switch { on: false }),
                dimmer: Some(Dimmer { level: 100 }),
                color: Some(Color {
                    hex: Some("#FFFFFF".into()),
                    kelvin: None,
                }),
                ..Default::default()
            },
        };
        let (state_tx, state_rx) = mpsc::channel(8);
        let (events_tx, events) = broadcast::channel(64);
        let outputs = Outputs {
            changed_tx: watch::channel(()).0,
            events_tx,
            save_tx: watch::channel(Vec::new()).0,
        };
        tokio::spawn(state::run(state_rx, [(strip.id.clone(), strip)].into(), outputs));
        let registry = Arc::new(Registry::new(vec![Box::new(Wled)]));
        let secrets = Arc::new(Secrets::new(Default::default(), watch::channel(Vec::new()).0));
        let control = Control::new(state_tx, registry.clone(), secrets);
        registry.start_all(&control).await;
        TestHub {
            control,
            events,
            registry,
        }
    }

    /* Waits (up to a few seconds) for the strip to look like `wanted`. */
    async fn until(hub: &mut TestHub, wanted: impl Fn(&Device) -> bool) -> Device {
        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        if let Some(d) = hub.control.get("strip").await.unwrap() {
            if wanted(&d) {
                return d;
            }
        }
        loop {
            tokio::select! {
                _ = &mut deadline => panic!("timed out; strip is {:?}", hub.control.get("strip").await),
                event = hub.events.recv() => if let Ok(Event::Changed(d)) = event {
                    if wanted(&d) { return d }
                },
            }
        }
    }

    #[tokio::test]
    async fn connects_follows_and_commands_a_wled() {
        let sim = Sim::start(true).await;
        let mut hub = hub_with_strip(&sim.host()).await;

        /* Connecting reports the real state (the sim starts on, 50 %,
         * orange) and marks the device online. */
        let d = until(&mut hub, |d| d.online == Some(Health::Online)).await;
        assert_eq!(d.capabilities.switch, Some(Switch { on: true }));
        assert_eq!(d.capabilities.dimmer, Some(Dimmer { level: 50 }));
        assert_eq!(d.capabilities.color.unwrap().hex.as_deref(), Some("#FF8800"));

        /* A command: confirmed state comes back in the reply. */
        let d = hub.control.command("strip", "switch", json!({"on": false})).await.unwrap();
        assert_eq!(d.capabilities.switch, Some(Switch { on: false }));
        assert!(!sim.state()["on"].as_bool().unwrap());

        let d = hub.control.command("strip", "dimmer", json!({"level": 20})).await.unwrap();
        assert_eq!(d.capabilities.dimmer, Some(Dimmer { level: 20 }));
        assert_eq!(d.capabilities.switch, Some(Switch { on: true }), "dimming up switches on");
        assert_eq!(sim.state()["bri"], 51);

        let d = hub.control.command("strip", "color", json!({"kelvin": 3000})).await.unwrap();
        assert_eq!(d.capabilities.color.unwrap().kelvin, Some(3000));

        /* A change made elsewhere (the WLED app) is pushed over the
         * WebSocket and shows up without anyone asking. */
        sim.change_from_outside(json!({"seg": {"col": [[0, 0, 255]]}}));
        let d = until(&mut hub, |d| d.capabilities.color.as_ref().and_then(|c| c.hex.as_deref()) == Some("#0000FF")).await;
        assert_eq!(d.online, Some(Health::Online));

        /* Unplugged: offline, and commands fail with a reason. */
        sim.stop();
        until(&mut hub, |d| d.online == Some(Health::Offline)).await;
        let err = hub.control.command("strip", "switch", json!({"on": true})).await.unwrap_err();
        assert!(err.contains("reach") || err.contains("answer"), "{err}");

        hub.registry.stop("strip");
    }

    #[tokio::test]
    async fn polls_when_there_is_no_websocket() {
        /* A sim without /ws: the state is still read, commands still work. */
        let sim = Sim::start(false).await;
        let mut hub = hub_with_strip(&sim.host()).await;
        until(&mut hub, |d| d.online == Some(Health::Online)).await;
        let d = hub.control.command("strip", "color", json!({"hex": "#00FF00"})).await.unwrap();
        assert_eq!(d.capabilities.color.unwrap().hex.as_deref(), Some("#00FF00"));
        assert_eq!(sim.state()["seg"][0]["col"][0], json!([0, 255, 0, 0]));
    }

    #[tokio::test]
    async fn unreachable_at_start_is_offline() {
        let mut hub = hub_with_strip("127.0.0.1:9").await;
        until(&mut hub, |d| d.online == Some(Health::Offline)).await;
    }

    fn host(host: &str) -> SetupValues {
        SetupValues {
            plain: [("host".to_string(), host.to_string())].into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn probe_recognises_wled() {
        let sim = Sim::start(true).await;
        let found = Wled.probe(&host(&sim.host())).await.unwrap();
        assert_eq!(found.values["mac"], "aabbccddeeff");
        assert_eq!(found.name.as_deref(), Some("WLED Sim"));
        assert_eq!(found.summary, "WLED 0.0.0-sim");

        /* Nothing there / something else there: different kinds. */
        assert_eq!(Wled.probe(&host("127.0.0.1:9")).await.unwrap_err().kind, ErrorKind::Unreachable);
        let not_wled = Sim::start_not_wled().await;
        assert_eq!(Wled.probe(&host(&not_wled.host())).await.unwrap_err().kind, ErrorKind::Unsupported);
    }

    /* The whole adapter against a real WLED (not run by default):
     *   WLED_HOST=192.168.1.139 cargo test wled::tests::live -- --ignored --nocapture
     * Switches it off and on, dims it, colours it -- then puts back the
     * state it found. */
    #[tokio::test]
    #[ignore]
    async fn live() {
        let host = std::env::var("WLED_HOST").expect("set WLED_HOST");
        println!("probe: {:?}", Wled.probe(&super::tests::host(&host)).await.unwrap());
        let mut hub = hub_with_strip(&host).await;
        let found = until(&mut hub, |d| d.online == Some(Health::Online)).await;
        println!("found: {:?}", found.capabilities);
        let pause = || tokio::time::sleep(Duration::from_millis(1500));
        for (capability, value) in [
            ("switch", json!({"on": false})),
            ("switch", json!({"on": true})),
            ("dimmer", json!({"level": 10})),
            ("dimmer", json!({"level": 100})),
            ("color", json!({"kelvin": 2700})),
            ("color", json!({"hex": "#0000FF"})),
        ] {
            let d = hub.control.command("strip", capability, value.clone()).await.unwrap();
            println!("{capability} {value} -> {:?}", d.capabilities);
            pause().await;
        }
        /* Put it back. */
        let caps = found.capabilities;
        hub.control.command("strip", "color", json!(caps.color.unwrap())).await.unwrap();
        hub.control.command("strip", "dimmer", json!(caps.dimmer.unwrap())).await.unwrap();
        hub.control.command("strip", "switch", json!(caps.switch.unwrap())).await.unwrap();
        hub.registry.stop("strip");
    }
}
