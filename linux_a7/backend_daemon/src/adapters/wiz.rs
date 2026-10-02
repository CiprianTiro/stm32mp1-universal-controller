/*
 * wiz.rs -- the adapter for WiZ lights (issue #75; template
 * templates/wiz.json): WiFi bulbs and lamps by WiZ (Signify, also sold as
 * "Philips Smart LED ... WiZ").
 *
 * A WiZ light has a LOCAL API -- no account, no pairing (setup pattern P1)
 * -- once "Allow local communication" is on in the WiZ app. It is JSON
 * over UDP, port 38899, one datagram each way:
 *   {"method":"getPilot","params":{}}
 *       -> {"method":"getPilot","env":"pro","result":{"mac":"a8bb50..",
 *           "state":true,"dimming":80,"r":255,"g":136,"b":0,...}}
 *   {"method":"setPilot","params":{"state":true,"dimming":50}}
 *       -> {"method":"setPilot","env":"pro","result":{"success":true}}
 *   {"method":"getSystemConfig","params":{}}
 *       -> {..."result":{"mac":"..","moduleName":"ESP01_SHRGB1C_31",
 *           "fwVersion":"1.26.0",...}}            (the wizard's test step)
 * The same getPilot, broadcast, is how discovery finds them (wiz.json).
 *
 * HOW THE CAPABILITIES MAP (device.rs):
 *   switch  {"on"}      <-> "state"
 *   dimmer  {"level"}   <-> "dimming", which WiZ only takes from 10 to 100:
 *                       a level of 1-9 is sent as 10. Level 0 is "off"
 *                       (the brightness stays, as on a WLED).
 *   color   {"hex"}     <-> "r", "g", "b"
 *           {"kelvin"}  <-> "temp" (WiZ: 2200-6500 K, most white bulbs from
 *                       2700) -- a real white, from the bulb's white LEDs.
 *                       The bulb reports either temp or r/g/b: whichever
 *                       mode it's in. While a SCENE plays (sceneId > 0, set
 *                       in the WiZ app: "Fireplace", "Ocean"...) the colour
 *                       keeps changing: not reported.
 *
 * THE DEVICE'S TASK: UDP has no connection to keep, so the task asks
 * getPilot every POLL_EVERY (changes made in the WiZ app show up within
 * that) and after every command (the confirmation). Each request is tried
 * a few times: UDP may lose a packet. A bulb that doesn't answer is
 * offline; asked again every RETRY..RETRY_MAX (switched off at the wall,
 * it can be for days).
 * (WiZ lights can also PUSH their state, after a "registration" with the
 * hub's address -- needs another open port and renewing every few
 * seconds; polling is simpler and enough for now.)
 */
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

use super::net::{self, NetError};
use super::wled::{hex_to_rgb, rgb_to_hex};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Color, Device, Health};
use crate::templates::ErrorKind;

/* WiZ's UDP port, on every light. */
pub const PORT: u16 = 38899;
/* Each request is sent up to this many times (see net::udp_request). */
const TRIES: u32 = 3;
/* The state is asked for this often while the light answers... (faster
 * in tests, which wait for polls) */
const POLL_EVERY: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 5 });
/* ...and, while it doesn't, from RETRY_MIN, doubling to RETRY_MAX. */
const RETRY_MIN: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 5 });
const RETRY_MAX: Duration = Duration::from_secs(60);
/* What WiZ lights accept. */
const MIN_DIMMING: u8 = 10;
const MIN_KELVIN: u16 = 2200;
const MAX_KELVIN: u16 = 6500;

pub struct Wiz;

impl Adapter for Wiz {
    fn id(&self) -> &'static str {
        "wiz"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            host: device.config.get("host").cloned(),
            has_color: device.capabilities.color.is_some(),
            has_dimmer: device.capabilities.dimmer.is_some(),
            hub,
        };
        tokio::spawn(task.run(commands_rx));
        DeviceHandle { commands }
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(probe(values))
    }
}

/* One WiZ light's task (see the header). */
struct Task {
    id: String,
    host: Option<String>,
    /* Only the capabilities the device was created with are reported. */
    has_color: bool,
    has_dimmer: bool,
    hub: Hub,
}

impl Task {
    async fn run(self, mut commands: mpsc::Receiver<DeviceCmd>) {
        let Some(host) = self.host.clone() else {
            println!("wiz: {} has no address (config \"host\")", self.id);
            self.hub.set_online(&self.id, Health::Offline).await;
            while let Some(cmd) = commands.recv().await {
                cmd.refuse(format!("{} has no address set", self.id));
            }
            return;
        };

        /* The first poll right away; then POLL_EVERY, or the retry wait
         * while the light is gone. */
        let mut wait = Duration::ZERO;
        let mut retry = RETRY_MIN;
        /* Log a failing light once per outage, not once per attempt. */
        let mut online = None;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                cmd = commands.recv() => match cmd {
                    None => return,
                    Some(cmd) => {
                        self.handle(&host, cmd).await;
                        /* A command doesn't restart the poll timer: next
                         * poll after a full interval again (good enough). */
                        continue;
                    }
                },
            }
            match self.poll(&host).await {
                Ok(()) => {
                    if online != Some(true) {
                        println!("wiz: {} answers at {host}", self.id);
                    }
                    online = Some(true);
                    retry = RETRY_MIN;
                    wait = POLL_EVERY;
                }
                Err(e) => {
                    if online != Some(false) {
                        println!("wiz: {}: {e} (will keep trying)", self.id);
                    }
                    online = Some(false);
                    self.hub.set_online(&self.id, Health::Offline).await;
                    wait = retry;
                    retry = (retry * 2).min(RETRY_MAX);
                }
            }
        }
    }

    /* getPilot -> report. */
    async fn poll(&self, host: &str) -> Result<(), String> {
        let pilot = request(host, "getPilot", json!({})).await?;
        self.report(&pilot).await
    }

    async fn handle(&self, host: &str, cmd: DeviceCmd) {
        let DeviceCmd::Command { capability, value, reply } = cmd else {
            cmd.refuse(format!("{} has no actions", self.id));
            return;
        };
        let result = self.command(host, &capability, &value).await;
        let _ = reply.send(result);
    }

    /* setPilot, then getPilot: what the light now really does is the
     * confirmation (and the state reported). */
    async fn command(&self, host: &str, capability: &str, value: &Value) -> Result<(), String> {
        let params = to_wiz(capability, value)?;
        let answer = request(host, "setPilot", params).await?;
        if answer["success"] != true {
            return Err(format!("{} refused the command: {answer}", self.id));
        }
        self.poll(host).await
    }

    async fn report(&self, pilot: &Value) -> Result<(), String> {
        let now = from_wiz(pilot)?;
        self.hub.report(&self.id, "switch", json!({ "on": now.on })).await?;
        if self.has_dimmer {
            self.hub.report(&self.id, "dimmer", json!({ "level": now.level })).await?;
        }
        if let (true, Some(color)) = (self.has_color, now.color) {
            self.hub.report(&self.id, "color", json!(color)).await?;
        }
        /* Online after the state (see wled.rs). */
        self.hub.set_online(&self.id, Health::Online).await;
        Ok(())
    }
}

/* One WiZ request; returns its "result". An answer to another method (a
 * late answer to an earlier try) is skipped; an "error" answer fails. */
async fn request(host: &str, method: &str, params: Value) -> Result<Value, NetError> {
    let message = json!({ "method": method, "params": params }).to_string();
    let answer = net::udp_request(host, PORT, message.as_bytes(), TRIES, |reply| {
        serde_json::from_slice::<Value>(reply).is_ok_and(|v| v["method"] == method)
    })
    .await?;
    let mut answer: Value = serde_json::from_slice(&answer).unwrap_or_default();
    if let Some(error) = answer.get("error") {
        return Err(NetError::new(
            ErrorKind::Refused,
            format!("{host} refused {method}: {}", error["message"].as_str().unwrap_or("no reason given")),
        ));
    }
    match answer.get_mut("result") {
        Some(result) => Ok(result.take()),
        None => Err(NetError::new(ErrorKind::Unsupported, format!("{host}: {method} answered without a result"))),
    }
}

/* ------------------------------------------------------------------ */
/* WiZ's JSON <-> capabilities (pure, unit-tested below)               */
/* ------------------------------------------------------------------ */

#[derive(Debug, PartialEq)]
struct Reported {
    on: bool,
    level: u8,
    /* None while a scene plays, or if the light sent no colour. */
    color: Option<Color>,
}

fn from_wiz(pilot: &Value) -> Result<Reported, String> {
    let on = pilot["state"].as_bool().ok_or_else(|| format!("not a WiZ state: {pilot}"))?;
    let level = pilot["dimming"].as_u64().map_or(100, |d| d.min(100) as u8);
    let scene = pilot["sceneId"].as_u64().unwrap_or(0);
    let channel = |name: &str| pilot[name].as_u64().and_then(|c| u8::try_from(c).ok());
    let color = match (scene, pilot["temp"].as_u64(), channel("r"), channel("g"), channel("b")) {
        (0, Some(temp), ..) if temp > 0 => Some(Color {
            hex: None,
            kelvin: u16::try_from(temp).ok(),
        }),
        (0, _, Some(r), Some(g), Some(b)) => Some(Color {
            hex: Some(rgb_to_hex([r, g, b])),
            kelvin: None,
        }),
        _ => None,
    };
    Ok(Reported { on, level, color })
}

/* A capability command -> setPilot's params. (Already checked by
 * control.rs: the shape is right.) */
fn to_wiz(capability: &str, value: &Value) -> Result<Value, String> {
    match capability {
        "switch" => {
            let on = value["on"].as_bool().ok_or("switch needs {\"on\": true|false}")?;
            Ok(json!({ "state": on }))
        }
        "dimmer" => {
            let level = value["level"].as_u64().ok_or("dimmer needs {\"level\": 0-100}")?.min(100) as u8;
            Ok(if level == 0 {
                json!({ "state": false })
            } else {
                json!({ "state": true, "dimming": level.max(MIN_DIMMING) })
            })
        }
        "color" => {
            let color: Color = serde_json::from_value(value.clone()).map_err(|e| format!("invalid color: {e}"))?;
            match (color.hex, color.kelvin) {
                (Some(hex), _) => {
                    let [r, g, b] = hex_to_rgb(&hex).ok_or_else(|| format!("invalid color {hex:?}"))?;
                    Ok(json!({ "r": r, "g": g, "b": b }))
                }
                (None, Some(k)) => Ok(json!({ "temp": k.clamp(MIN_KELVIN, MAX_KELVIN) })),
                (None, None) => Err("color needs \"hex\" or \"kelvin\"".into()),
            }
        }
        other => Err(format!("WiZ lights have no capability {other:?}")),
    }
}

/* ------------------------------------------------------------------ */
/* Setup                                                               */
/* ------------------------------------------------------------------ */

/* The wizard's test step: a WiZ light at "host"? Its MAC (the template's
 * identity, the same form discovery fills) and model come back. */
async fn probe(values: &SetupValues) -> Result<Probe, SetupError> {
    let host = values
        .plain
        .get("host")
        .ok_or_else(|| SetupError::new(ErrorKind::Unsupported, "no address given"))?;
    let config = request(host, "getSystemConfig", json!({})).await?;
    let text = |key: &str| config[key].as_str().unwrap_or_default().to_string();
    if text("mac").is_empty() {
        return Err(SetupError::new(ErrorKind::Unsupported, format!("{host} answers, but isn't a WiZ light")));
    }
    let model = match text("moduleName") {
        m if m.is_empty() => "light".to_string(),
        m => m,
    };
    Ok(Probe {
        values: [("mac".to_string(), text("mac"))].into(),
        name: None,
        summary: format!("WiZ {model}, firmware {}", text("fwVersion")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_hub::TestHub;
    use crate::adapters::wiz_sim::Sim;
    use crate::device::{Capabilities, Dimmer, Switch};

    #[test]
    fn commands_become_wiz_params() {
        assert_eq!(to_wiz("switch", &json!({"on": false})).unwrap(), json!({"state": false}));
        assert_eq!(to_wiz("dimmer", &json!({"level": 0})).unwrap(), json!({"state": false}));
        assert_eq!(to_wiz("dimmer", &json!({"level": 3})).unwrap(), json!({"state": true, "dimming": 10}));
        assert_eq!(to_wiz("dimmer", &json!({"level": 70})).unwrap(), json!({"state": true, "dimming": 70}));
        assert_eq!(to_wiz("color", &json!({"hex": "#FF8800"})).unwrap(), json!({"r": 255, "g": 136, "b": 0}));
        assert_eq!(to_wiz("color", &json!({"kelvin": 2700})).unwrap(), json!({"temp": 2700}));
        assert_eq!(to_wiz("color", &json!({"kelvin": 1000})).unwrap(), json!({"temp": 2200}));
        assert!(to_wiz("media", &json!({})).is_err());
    }

    #[test]
    fn wiz_state_becomes_capabilities() {
        let rgb = from_wiz(&json!({"mac": "a8", "state": true, "sceneId": 0, "r": 255, "g": 136, "b": 0, "c": 0, "w": 0, "dimming": 80})).unwrap();
        assert!(rgb.on);
        assert_eq!(rgb.level, 80);
        assert_eq!(rgb.color.unwrap().hex.as_deref(), Some("#FF8800"));

        let white = from_wiz(&json!({"state": false, "sceneId": 0, "temp": 2700, "dimming": 10})).unwrap();
        assert_eq!(white.color.unwrap().kelvin, Some(2700));
        assert!(!white.on);

        /* A scene: on and dimming count, the colour doesn't. */
        let scene = from_wiz(&json!({"state": true, "sceneId": 5, "speed": 100, "dimming": 60})).unwrap();
        assert_eq!(scene, Reported { on: true, level: 60, color: None });

        assert!(from_wiz(&json!({"success": true})).is_err());
    }

    fn bulb(host: &str) -> Device {
        Device {
            id: "bulb".into(),
            name: "Bulb".into(),
            room: String::new(),
            template: "wiz".into(),
            source: crate::device::Source::new("wiz"),
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
        }
    }

    #[tokio::test]
    async fn follows_and_commands_a_simulated_bulb() {
        let sim = Sim::start().await;
        let mut hub = TestHub::start(bulb(&sim.host()), Box::new(Wiz)).await;

        /* The first poll reports the real state: on, 80 %, warm white. */
        let d = hub.until(|d| d.online == Some(Health::Online)).await;
        assert_eq!(d.capabilities.switch, Some(Switch { on: true }));
        assert_eq!(d.capabilities.dimmer, Some(Dimmer { level: 80 }));
        assert_eq!(d.capabilities.color.unwrap().kelvin, Some(2700));

        let d = hub.control.command("bulb", "dimmer", json!({"level": 30})).await.unwrap();
        assert_eq!(d.capabilities.dimmer, Some(Dimmer { level: 30 }));
        assert_eq!(sim.pilot()["dimming"], 30);

        let d = hub.control.command("bulb", "color", json!({"hex": "#0000FF"})).await.unwrap();
        assert_eq!(d.capabilities.color.unwrap().hex.as_deref(), Some("#0000FF"));

        let d = hub.control.command("bulb", "switch", json!({"on": false})).await.unwrap();
        assert_eq!(d.capabilities.switch, Some(Switch { on: false }));
        assert_eq!(sim.pilot()["state"], false);

        /* Changed in the WiZ app: seen at the next poll. */
        sim.change_from_outside(json!({"state": true, "temp": 4000}));
        let d = hub
            .until(|d| d.capabilities.color.as_ref().and_then(|c| c.kelvin) == Some(4000))
            .await;
        assert_eq!(d.capabilities.switch, Some(Switch { on: true }));

        /* Unplugged: offline, commands fail with a reason. */
        sim.stop();
        hub.until(|d| d.online == Some(Health::Offline)).await;
        let err = hub.control.command("bulb", "switch", json!({"on": true})).await.unwrap_err();
        assert!(err.contains("answer") || err.contains("reach"), "{err}");
        hub.registry.stop("bulb");
    }

    #[tokio::test]
    async fn lost_packets_are_retried() {
        let sim = Sim::start().await;
        /* Every other datagram is "lost": each request still succeeds. */
        sim.lose_every_other();
        let mut hub = TestHub::start(bulb(&sim.host()), Box::new(Wiz)).await;
        hub.until(|d| d.online == Some(Health::Online)).await;
        for level in [20, 40, 60] {
            let d = hub.control.command("bulb", "dimmer", json!({"level": level})).await.unwrap();
            assert_eq!(d.capabilities.dimmer, Some(Dimmer { level }));
        }
        hub.registry.stop("bulb");
    }

    #[tokio::test]
    async fn probe_recognises_a_wiz_light() {
        let sim = Sim::start().await;
        let values = SetupValues {
            plain: [("host".to_string(), sim.host())].into(),
            ..Default::default()
        };
        let found = Wiz.probe(&values).await.unwrap();
        assert_eq!(found.values["mac"], "a8bb50aabbcc");
        assert_eq!(found.summary, "WiZ ESP01_SHRGB1C_31, firmware 1.26.0");
        /* Gone: on the LAN that's a timeout; here, on localhost, the
         * kernel says at once that nothing listens (unreachable). */
        sim.stop();
        let kind = Wiz.probe(&values).await.unwrap_err().kind;
        assert!(matches!(kind, ErrorKind::Timeout | ErrorKind::Unreachable), "{kind:?}");
    }

    /* Against a REAL WiZ light (not run by default):
     *   WIZ_HOST=192.168.1.60 cargo test wiz::tests::live -- --ignored --nocapture
     * Switches it off and on, dims it, colours it -- then puts back the
     * state it found. */
    #[tokio::test]
    #[ignore]
    async fn live() {
        let host = std::env::var("WIZ_HOST").expect("set WIZ_HOST");
        let values = SetupValues {
            plain: [("host".to_string(), host.clone())].into(),
            ..Default::default()
        };
        println!("probe: {:?}", Wiz.probe(&values).await.unwrap());
        let mut hub = TestHub::start(bulb(&host), Box::new(Wiz)).await;
        let found = hub.until(|d| d.online == Some(Health::Online)).await;
        println!("found: {:?}", found.capabilities);
        for (capability, value) in [
            ("switch", json!({"on": false})),
            ("switch", json!({"on": true})),
            ("dimmer", json!({"level": 10})),
            ("dimmer", json!({"level": 100})),
            ("color", json!({"kelvin": 2700})),
            ("color", json!({"kelvin": 6500})),
            ("color", json!({"hex": "#0000FF"})),
            ("color", json!({"hex": "#FF8800"})),
        ] {
            let d = hub.control.command("bulb", capability, value.clone()).await.unwrap();
            println!("{capability} {value} -> {:?}", d.capabilities);
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        let caps = found.capabilities;
        if let Some(color) = caps.color {
            hub.control.command("bulb", "color", json!(color)).await.unwrap();
        }
        hub.control.command("bulb", "dimmer", json!(caps.dimmer.unwrap())).await.unwrap();
        hub.control.command("bulb", "switch", json!(caps.switch.unwrap())).await.unwrap();
        hub.registry.stop("bulb");
    }
}
