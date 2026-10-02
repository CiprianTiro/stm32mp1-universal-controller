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
 * PUSHES (issue #72): a light the hub has "registered" with sends a
 * syncPilot message to the hub's UDP port 38900 whenever its state
 * changes -- in the WiZ app, by its own schedule, by a command. The
 * registration lapses after ~30 s, so each task renews it every
 * REGISTER_EVERY. One listener (Pushes) serves every light; a push is
 * matched to its light by MAC. While pushes arrive, polling slows to
 * POLL_WHILE_PUSHED -- only a safety net.
 *
 * THE DEVICE'S TASK: UDP has no connection to keep, so the task asks
 * getPilot every POLL_EVERY (changes made in the WiZ app show up within
 * that) and after every command (the confirmation). Each request is tried
 * a few times: UDP may lose a packet. A bulb that doesn't answer is
 * offline; asked again every RETRY..RETRY_MAX (switched off at the wall,
 * it can be for days).
 */
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

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
/* Pushes (see the header): the hub's port (fixed by WiZ), how often the
 * registration is renewed, how often to poll while pushes arrive. */
pub const PUSH_PORT: u16 = 38900;
const REGISTER_EVERY: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 20 });
const POLL_WHILE_PUSHED: Duration = Duration::from_secs(30);
/* What WiZ lights accept. */
const MIN_DIMMING: u8 = 10;
const MIN_KELVIN: u16 = 2200;
const MAX_KELVIN: u16 = 6500;

/* The one listener for every light's pushes: (MAC, params). */
#[derive(Clone)]
struct Pushes {
    tx: broadcast::Sender<(String, Value)>,
}

impl Pushes {
    /* Opens the port and starts reading it. None if it can't be opened
     * (then the lights are only polled, as before). */
    fn open(port: u16) -> Option<(Pushes, u16)> {
        let socket = std::net::UdpSocket::bind(("0.0.0.0", port))
            .and_then(|s| s.set_nonblocking(true).map(|()| s))
            .map_err(|e| println!("wiz: can't listen for pushes on UDP {port}: {e} (polling only)"))
            .ok()?;
        let bound = socket.local_addr().ok()?.port();
        let socket = tokio::net::UdpSocket::from_std(socket).ok()?;
        let (tx, _) = broadcast::channel(64);
        let pushes = Pushes { tx };
        let sender = pushes.tx.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((len, _)) = socket.recv_from(&mut buf).await {
                let Ok(message) = serde_json::from_slice::<Value>(&buf[..len]) else { continue };
                if message["method"] == "syncPilot" {
                    if let Some(mac) = message["params"]["mac"].as_str() {
                        let _ = sender.send((mac.to_lowercase(), message["params"].clone()));
                    }
                }
            }
        });
        println!("wiz: listening for pushes on UDP {bound}");
        Some((pushes, bound))
    }
}

pub struct Wiz {
    /* The port pushes are listened for (PUSH_PORT; tests: 0, any free). */
    push_port: u16,
    /* Opened at the first light (and the port really bound). */
    pushes: OnceLock<Option<(Pushes, u16)>>,
}

impl Wiz {
    pub fn new() -> Self {
        Wiz { push_port: PUSH_PORT, pushes: OnceLock::new() }
    }

    /* Tests: any free port; push_port() says which. */
    #[cfg(test)]
    fn for_tests() -> Self {
        Wiz { push_port: 0, pushes: OnceLock::new() }
    }

    fn pushes(&self) -> Option<(Pushes, u16)> {
        self.pushes.get_or_init(|| Pushes::open(self.push_port)).clone()
    }

    #[cfg(test)]
    fn push_port(&self) -> u16 {
        self.pushes().map_or(0, |(_, port)| port)
    }
}

impl Adapter for Wiz {
    fn id(&self) -> &'static str {
        "wiz"
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            host: device.config.get("host").cloned(),
            mac: device.config.get("mac").map(|m| m.replace(':', "").to_lowercase()),
            pushes: self.pushes().map(|(p, _)| p.tx.subscribe()),
            has_color: device.capabilities.color.is_some(),
            has_dimmer: device.capabilities.dimmer.is_some(),
            hub,
        };
        /* Issue #72: a webhook can ask for the state at once. */
        let refresh = Arc::new(tokio::sync::Notify::new());
        tokio::spawn(task.run(commands_rx, refresh.clone()));
        DeviceHandle::new(commands).with_refresh(refresh)
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(probe(values))
    }
}

/* One WiZ light's task (see the header). */
struct Task {
    id: String,
    host: Option<String>,
    /* Its MAC (setup's probe saved it): which pushes are its. */
    mac: Option<String>,
    pushes: Option<broadcast::Receiver<(String, Value)>>,
    /* Only the capabilities the device was created with are reported. */
    has_color: bool,
    has_dimmer: bool,
    hub: Hub,
}

impl Task {
    async fn run(mut self, mut commands: mpsc::Receiver<DeviceCmd>, refresh: Arc<tokio::sync::Notify>) {
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
        /* Pushes: renew the registration on this timer; when one came. */
        let mut register = tokio::time::interval(REGISTER_EVERY);
        let mut last_push: Option<Instant> = None;
        let mut pushes = self.pushes.take();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = register.tick(), if pushes.is_some() && self.mac.is_some() => {
                    self.register(&host).await;
                    continue;
                }
                push = next_push(&mut pushes) => {
                    let Some((mac, params)) = push else { continue };
                    if Some(&mac) == self.mac.as_ref() && self.report(&params).await.is_ok() {
                        last_push = Some(Instant::now());
                        online = Some(true);
                    }
                    continue;
                }
                /* "Read your state now" (a webhook): the poll comes early. */
                _ = refresh.notified() => {}
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
                    /* Pushes arriving: polling is only the safety net. */
                    let pushed = last_push.is_some_and(|t| t.elapsed() < REGISTER_EVERY * 3);
                    wait = if pushed { POLL_WHILE_PUSHED } else { POLL_EVERY };
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

    /* "Send me your changes" (see the header). Its answer isn't needed:
     * the pushes are the proof. */
    async fn register(&self, host: &str) {
        let params = json!({
            "phoneIp": crate::network::lan_address(),
            "phoneMac": crate::network::lan_mac(),
            "register": true,
            "id": "1",
        });
        let _ = request(host, "registration", params).await;
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

/* The next push, or never (no listener). */
async fn next_push(pushes: &mut Option<broadcast::Receiver<(String, Value)>>) -> Option<(String, Value)> {
    match pushes {
        Some(rx) => loop {
            match rx.recv().await {
                Ok(push) => return Some(push),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return std::future::pending().await,
            }
        },
        None => std::future::pending().await,
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
            ..Default::default()
        }),
        (0, _, Some(r), Some(g), Some(b)) => Some(Color {
            hex: Some(rgb_to_hex([r, g, b])),
            kelvin: None,
            ..Default::default()
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
            last_seen: None,
            favourite: false,
            capabilities: Capabilities {
                switch: Some(Switch { on: false }),
                dimmer: Some(Dimmer { level: 100 }),
                color: Some(Color {
                    hex: Some("#FFFFFF".into()),
                    kelvin: None,
                    ..Default::default()
                }),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn follows_and_commands_a_simulated_bulb() {
        let sim = Sim::start().await;
        let mut hub = TestHub::start(bulb(&sim.host()), Box::new(Wiz::for_tests())).await;

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

    /* Issue #72: registered, the light pushes its changes; polling slows
     * down to a safety net. */
    #[tokio::test]
    async fn pushes_are_followed() {
        let sim = Sim::start().await;
        let wiz = Wiz::for_tests();
        sim.push_to(format!("127.0.0.1:{}", wiz.push_port()).parse().unwrap());
        let mut light = bulb(&sim.host());
        light.config.insert("mac".into(), "a8bb50aabbcc".into());
        let mut hub = TestHub::start(light, Box::new(wiz)).await;
        hub.until(|d| d.online == Some(Health::Online)).await;
        for _ in 0..30 {
            if sim.registered() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(sim.registered(), "the hub registered for pushes");

        sim.change_from_outside(json!({"temp": 3000}));
        hub.until(|d| d.capabilities.color.as_ref().and_then(|c| c.kelvin) == Some(3000)).await;
        /* Pushes flow: the poll is now 30 s away -- this one can only come
         * as a push. */
        let started = Instant::now();
        sim.change_from_outside(json!({"dimming": 40}));
        hub.until(|d| d.capabilities.dimmer == Some(Dimmer { level: 40 })).await;
        assert!(started.elapsed() < Duration::from_millis(800), "{:?}", started.elapsed());
        hub.registry.stop("bulb");
    }

    #[tokio::test]
    async fn lost_packets_are_retried() {
        let sim = Sim::start().await;
        /* Every other datagram is "lost": each request still succeeds. */
        sim.lose_every_other();
        let mut hub = TestHub::start(bulb(&sim.host()), Box::new(Wiz::for_tests())).await;
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
        let found = Wiz::for_tests().probe(&values).await.unwrap();
        assert_eq!(found.values["mac"], "a8bb50aabbcc");
        assert_eq!(found.summary, "WiZ ESP01_SHRGB1C_31, firmware 1.26.0");
        /* Gone: on the LAN that's a timeout; here, on localhost, the
         * kernel says at once that nothing listens (unreachable). */
        sim.stop();
        let kind = Wiz::for_tests().probe(&values).await.unwrap_err().kind;
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
        println!("probe: {:?}", Wiz::for_tests().probe(&values).await.unwrap());
        let mut hub = TestHub::start(bulb(&host), Box::new(Wiz::for_tests())).await;
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
