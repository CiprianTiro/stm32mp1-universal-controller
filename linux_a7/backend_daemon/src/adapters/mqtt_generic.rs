/*
 * mqtt_generic.rs -- the GENERIC MQTT adapter, "mqtt" (issue #72).
 *
 * Devices that talk to the hub's own MQTT broker (broker.rs) instead of
 * being polled: Tasmota, WLED's MQTT mode, ESPHome. Like the generic HTTP
 * adapter (#75), the device's TEMPLATE describes its topics in an "mqtt"
 * block (templates.rs, MqttSpec) and this one adapter carries it out:
 *   - state: every message on one of the "state" topics is read (the whole
 *     payload, or values at JSON paths) and reported at once -- no polling,
 *     the device says when something changes;
 *   - online: its "last will" topic (the broker publishes "Offline" by
 *     itself when the device vanishes);
 *   - commands: one topic + payload per capability ({on}, {level}...), and
 *     the reply waits for the device's own state message (the confirmation);
 *   - setup: the action "mqtt_login" makes the device's own login on the
 *     broker (allowed only its own topics) and returns what the person types
 *     into the device: host, port, user, password. The test step then waits
 *     for the device to connect.
 *
 * BATTERY devices (template "power": "battery", "report_interval_s"):
 * they wake, report and sleep. Every message counts as "seen" (the card
 * shows "seen 3 min ago"); falling asleep (its last will says "offline")
 * is NOT offline; offline is only declared after MISSED_REPORTS reports
 * didn't come.
 *
 * The templates and the broker come after the adapters (main.rs), so this
 * adapter gets them later: set_templates, set_broker.
 */
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

use super::http_generic::{command_vars, convert, json_path};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::broker::{self, Broker};
use crate::device::{Device, Health};
use crate::secrets::Secret;
use crate::templates::{ErrorKind, HttpValue, MqttCommand, MqttSpec, Templates, GENERIC_MQTT_ADAPTER};

/* A command's reply waits this long for the device's own state message. */
const CONFIRM_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 5 });
/* The test step waits this long for the device to connect: the person may
 * still be typing the settings into it. */
const PROBE_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 90 });

/* A battery device is offline after this many reports didn't come. */
const MISSED_REPORTS: u32 = 3;

/* Cheap to clone (main.rs keeps one to hand it the templates and broker). */
#[derive(Clone, Default)]
pub struct MqttGeneric {
    templates: Arc<OnceLock<Arc<Templates>>>,
    broker: Arc<OnceLock<Arc<Broker>>>,
}

impl MqttGeneric {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_templates(&self, templates: Arc<Templates>) {
        let _ = self.templates.set(templates);
    }

    pub fn set_broker(&self, broker: Arc<Broker>) {
        let _ = self.broker.set(broker);
    }

    fn spec(&self, template: &str) -> Option<MqttSpec> {
        self.templates.get()?.get(template)?.mqtt.clone()
    }

    /* For a battery template: how often it reports. */
    fn report_interval(&self, template: &str) -> Option<Duration> {
        let t = self.templates.get()?.get(template)?;
        match t.power {
            crate::templates::Power::Battery => t.report_interval_s.map(|s| Duration::from_secs(s.into())),
            crate::templates::Power::Mains => None,
        }
    }
}

impl Adapter for MqttGeneric {
    fn id(&self) -> &'static str {
        GENERIC_MQTT_ADAPTER
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            config: device.config.clone(),
            spec: self.spec(&device.template),
            broker: self.broker.get().cloned(),
            hub,
            state: BTreeMap::new(),
            battery: self.report_interval(&device.template),
            heard: None,
        };
        tokio::spawn(task.run(commands_rx));
        DeviceHandle::new(commands)
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(async move {
            let (spec, broker) = self.parts(&values.template)?;
            probe(&spec, &broker, &values.plain).await
        })
    }

    fn action<'a>(&'a self, name: &'a str, values: &'a SetupValues) -> BoxFuture<'a, Result<SetupValues, SetupError>> {
        Box::pin(async move {
            if name != "mqtt_login" {
                return Err(SetupError::new(ErrorKind::Unsupported, format!("the mqtt adapter has no action {name:?}")));
            }
            let (spec, broker) = self.parts(&values.template)?;
            mqtt_login(&spec, &broker, &values.plain).await
        })
    }
}

impl MqttGeneric {
    fn parts(&self, template: &str) -> Result<(MqttSpec, Arc<Broker>), SetupError> {
        let spec = self
            .spec(template)
            .ok_or_else(|| SetupError::new(ErrorKind::Unsupported, format!("template {template:?} has no mqtt block")))?;
        let broker = self
            .broker
            .get()
            .cloned()
            .ok_or_else(|| SetupError::new(ErrorKind::Unreachable, "the hub's MQTT broker isn't running"))?;
        Ok((spec, broker))
    }
}

/* ------------------------------------------------------------------ */
/* Placeholders                                                        */
/* ------------------------------------------------------------------ */

/* {name} -> the device's setting (or a command's value). */
fn fill(text: &str, values: &BTreeMap<String, String>) -> String {
    crate::discovery::fill(text, values)
}

/* "tasmota-{topic}" with topic "Kitchen_1" -> "tasmota-kitchen_1": what
 * broker::valid_user accepts. */
fn login_name(spec: &MqttSpec, config: &BTreeMap<String, String>) -> String {
    fill(&spec.user, config)
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(64)
        .collect()
}

/* ------------------------------------------------------------------ */
/* Reading what a device sends                                         */
/* ------------------------------------------------------------------ */

/* The values one message gives, as "<capability>.<field>" -> value.
 * Path "$" = the whole payload: its JSON if it is JSON, else its text. */
fn read_message(reads: &BTreeMap<String, HttpValue>, payload: &[u8]) -> Vec<(String, Value)> {
    let text = String::from_utf8_lossy(payload).trim().to_string();
    let json: Option<Value> = serde_json::from_str(&text).ok();
    let mut out = Vec::new();
    for (target, how) in reads {
        let raw = match how.path() {
            "$" => json.clone().unwrap_or_else(|| Value::String(text.clone())),
            path => match json.as_ref().and_then(|j| json_path(j, path)) {
                Some(v) if !v.is_null() => v.clone(),
                _ => continue,
            },
        };
        let (capability, _) = target.split_once('.').unwrap_or((target, ""));
        if let Some(value) = convert(capability, &raw, how) {
            out.push((target.clone(), value));
        }
    }
    out
}

/* Folds read values into the capabilities' current values (a message
 * may carry only some fields: a sensor's one reading). Returns the
 * capabilities that changed. */
fn merge(state: &mut BTreeMap<String, Value>, values: Vec<(String, Value)>) -> Vec<String> {
    let mut changed = Vec::new();
    for (target, value) in values {
        let (capability, field) = target.split_once('.').unwrap_or((&target, ""));
        let entry = state.entry(capability.to_string()).or_insert_with(|| json!({}));
        let before = entry.clone();
        if capability == "sensor" {
            entry["readings"][field] = value;
        } else {
            entry[field] = value;
        }
        if *entry != before && !changed.iter().any(|c| c == capability) {
            changed.push(capability.to_string());
        }
    }
    changed
}

/* ------------------------------------------------------------------ */
/* The device's task                                                   */
/* ------------------------------------------------------------------ */

struct Task {
    id: String,
    config: BTreeMap<String, String>,
    spec: Option<MqttSpec>,
    broker: Option<Arc<Broker>>,
    hub: Hub,
    /* What's been reported, per capability (merged from messages). */
    state: BTreeMap<String, Value>,
    /* A battery device's report interval (None: mains-powered). */
    battery: Option<Duration>,
    /* When it last sent anything (battery devices). */
    heard: Option<tokio::time::Instant>,
}

impl Task {
    async fn run(mut self, mut commands: mpsc::Receiver<DeviceCmd>) {
        let (Some(spec), Some(broker)) = (self.spec.clone(), self.broker.clone()) else {
            let why = if self.spec.is_none() { "its template has no mqtt block" } else { "the hub's MQTT broker isn't running" };
            println!("mqtt: {}: {why}", self.id);
            self.hub.set_online(&self.id, Health::Offline).await;
            while let Some(cmd) = commands.recv().await {
                cmd.refuse(format!("{}: {why}", self.id));
            }
            return;
        };
        let mut messages = broker.subscribe_messages();
        /* What arrived before this task started (a retained "Online"). */
        let mut topics: Vec<String> = spec.state.iter().map(|s| fill(&s.topic, &self.config)).collect();
        if let Some(online) = &spec.online {
            topics.push(fill(&online.topic, &self.config));
        }
        for (topic, payload) in broker.last_messages(&topics) {
            self.handle(&spec, &topic, &payload).await;
        }
        /* "Tell me your state" -- the broker connection may still be coming
         * up right after the hub starts: a few tries. */
        if let Some(refresh) = &spec.refresh {
            let topic = fill(&refresh.topic, &self.config);
            let payload = fill(&refresh.payload, &self.config).into_bytes();
            for _ in 0..5 {
                if broker.publish(&topic, payload.clone()).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        /* Battery devices: checked now and then for missed reports. */
        let mut watchdog = tokio::time::interval(Duration::from_secs(if cfg!(test) { 1 } else { 30 }));
        loop {
            tokio::select! {
                _ = watchdog.tick(), if self.battery.is_some() => self.check_missed().await,
                message = messages.recv() => match message {
                    Ok((topic, payload)) => {
                        self.handle(&spec, &topic, &payload).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => println!("mqtt: {}: missed {n} messages", self.id),
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                cmd = commands.recv() => match cmd {
                    None => return,
                    Some(cmd) => self.command(&spec, &broker, &mut messages, cmd).await,
                },
            }
        }
    }

    /* One message: online/offline, and/or state. Returns the capabilities
     * it REPORTED -- changed or not: "on" sent to a light that already
     * was on is answered with the same value, and that's a confirmation
     * too. */
    async fn handle(&mut self, spec: &MqttSpec, topic: &str, payload: &[u8]) -> Vec<String> {
        let mut changed = Vec::new();
        let mut reported: Vec<String> = Vec::new();
        /* Battery devices: anything on one of its topics is a sign of life. */
        let ours = spec.state.iter().any(|s| fill(&s.topic, &self.config) == topic)
            || spec.online.as_ref().is_some_and(|o| fill(&o.topic, &self.config) == topic);
        if self.battery.is_some() && ours {
            self.heard = Some(tokio::time::Instant::now());
            self.hub.seen(&self.id).await;
            self.hub.set_online(&self.id, Health::Online).await;
        }
        for read in spec.state.iter().filter(|s| fill(&s.topic, &self.config) == topic) {
            let values = read_message(&read.read, payload);
            for (target, _) in &values {
                let capability = target.split_once('.').map_or(target.as_str(), |(c, _)| c).to_string();
                if !reported.contains(&capability) {
                    reported.push(capability);
                }
            }
            changed.extend(merge(&mut self.state, values));
        }
        for capability in &changed {
            if let Err(e) = self.hub.report(&self.id, capability, self.state[capability].clone()).await {
                println!("mqtt: {}: {capability}: {e}", self.id);
            }
        }
        match &spec.online {
            /* A battery device saying goodbye is falling asleep, not gone
             * (check_missed decides that). */
            Some(online) if fill(&online.topic, &self.config) == topic && self.battery.is_none() => {
                let text = String::from_utf8_lossy(payload);
                let health = if text.trim() == online.online { Health::Online } else { Health::Offline };
                self.hub.set_online(&self.id, health).await;
            }
            /* No last will: hearing from it is the sign of life. */
            None if !reported.is_empty() => self.hub.set_online(&self.id, Health::Online).await,
            _ => {}
        }
        reported
    }

    /* A battery device that missed MISSED_REPORTS reports: offline. (Not
     * heard from at all since the hub started: not known -- left alone.) */
    async fn check_missed(&mut self) {
        let (Some(interval), Some(heard)) = (self.battery, self.heard) else { return };
        if heard.elapsed() > interval * MISSED_REPORTS {
            self.hub.set_online(&self.id, Health::Offline).await;
        }
    }

    /* A command: published, then answered once the device's own state
     * message for that capability arrives (or after CONFIRM_WAIT). */
    async fn command(&mut self, spec: &MqttSpec, broker: &Broker, messages: &mut broadcast::Receiver<(String, Vec<u8>)>, cmd: DeviceCmd) {
        let DeviceCmd::Command { capability, value, reply } = cmd else {
            cmd.refuse(format!("{} has no actions", self.id));
            return;
        };
        let result = async {
            let command = spec
                .commands
                .get(&capability)
                .ok_or_else(|| format!("{} can't set {capability:?}", self.id))?;
            let (topic, payload) = build(command, &command_vars(&capability, &value)?, &self.config);
            broker.publish(&topic, payload.into_bytes()).await?;
            /* Already in that state: some devices (WLED) then answer
             * nothing at all -- there's nothing to wait for. */
            let known = match self.state.get(&capability) {
                Some(now) => Some(now.clone()),
                /* Nothing heard since the hub started: what it saved. */
                None => self
                    .hub
                    .device(&self.id)
                    .await
                    .and_then(|d| serde_json::to_value(&d.capabilities).ok())
                    .and_then(|caps| caps.get(&capability).cloned()),
            };
            if known.is_some_and(|now| already(&value, &now)) {
                return Ok(());
            }
            let deadline = tokio::time::Instant::now() + CONFIRM_WAIT;
            loop {
                match tokio::time::timeout_at(deadline, messages.recv()).await {
                    Ok(Ok((topic, payload))) => {
                        if self.handle(spec, &topic, &payload).await.contains(&capability) {
                            return Ok(());
                        }
                    }
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                        return Err(format!("{} didn't confirm (no answer over MQTT within {} s)", self.id, CONFIRM_WAIT.as_secs()))
                    }
                }
            }
        }
        .await;
        let _ = reply.send(result);
    }
}

/* Does the device's known state already have every field the command
 * asks for? (Numbers as numbers, colours ignoring case.) */
fn already(wanted: &Value, now: &Value) -> bool {
    let Some(fields) = wanted.as_object() else { return false };
    !fields.is_empty()
        && fields.iter().all(|(key, want)| match (want, now.get(key)) {
            (_, None) => false,
            (Value::String(a), Some(Value::String(b))) => a.eq_ignore_ascii_case(b),
            (a, Some(b)) => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x == y,
                _ => a == b,
            },
        })
}

/* A command's topic and payload, placeholders filled ({on} as the
 * template's "bool" texts, e.g. ON/OFF for Tasmota). */
fn build(command: &MqttCommand, vars: &Map<String, Value>, config: &BTreeMap<String, String>) -> (String, String) {
    let mut values = config.clone();
    for (name, value) in vars {
        let text = match (name.as_str(), value, &command.bool) {
            ("on", Value::Bool(b), Some([off, on])) => if *b { on.clone() } else { off.clone() },
            (_, Value::String(s), _) => s.clone(),
            (_, other, _) => other.to_string(),
        };
        values.insert(name.clone(), text);
    }
    (fill(&command.topic, config), fill(&command.payload, &values))
}

/* ------------------------------------------------------------------ */
/* Setup                                                               */
/* ------------------------------------------------------------------ */

/* The action "mqtt_login": the device's own login (a new password if it
 * had one), allowed only its template's topics. */
async fn mqtt_login(spec: &MqttSpec, broker: &Broker, config: &BTreeMap<String, String>) -> Result<SetupValues, SetupError> {
    let user = login_name(spec, config);
    if !broker::valid_user(&user) {
        return Err(SetupError::new(ErrorKind::Unsupported, format!("can't make a login name from {:?}", spec.user)));
    }
    let rules: Vec<broker::Rule> = spec
        .acl
        .iter()
        .map(|r| broker::Rule { access: r.access, topic: fill(&r.topic, config) })
        .collect();
    let password = broker
        .add_device(&user, rules)
        .await
        .map_err(|e| SetupError::new(ErrorKind::Unsupported, e))?;
    let mut values = SetupValues::default();
    values.plain.insert("mqtt_user".into(), user);
    values.plain.insert("mqtt_host".into(), crate::network::lan_address());
    values.plain.insert("mqtt_port".into(), broker::PLAIN_PORT.to_string());
    values.secret.insert("mqtt_password".into(), Secret::new(password));
    Ok(values)
}

/* The test step: has the device connected? Its last will says "online",
 * or it sent something on a state topic -- now, or (retained) already. */
async fn probe(spec: &MqttSpec, broker: &Broker, config: &BTreeMap<String, String>) -> Result<Probe, SetupError> {
    let mut messages = broker.subscribe_messages();
    let online = spec.online.as_ref().map(|o| (fill(&o.topic, config), o.online.clone()));
    let state: Vec<String> = spec.state.iter().map(|s| fill(&s.topic, config)).collect();
    let alive = |topic: &str, payload: &[u8]| match &online {
        Some((t, word)) if t == topic => String::from_utf8_lossy(payload).trim() == word,
        _ => state.iter().any(|s| s == topic),
    };
    let mut topics = state.clone();
    topics.extend(online.iter().map(|(t, _)| t.clone()));
    if broker.last_messages(&topics).iter().any(|(t, p)| alive(t, p)) {
        return Ok(Probe { summary: "Connected to the hub".into(), ..Default::default() });
    }
    let deadline = tokio::time::Instant::now() + PROBE_WAIT;
    loop {
        match tokio::time::timeout_at(deadline, messages.recv()).await {
            Ok(Ok((topic, payload))) if alive(&topic, &payload) => {
                return Ok(Probe { summary: "Connected to the hub".into(), ..Default::default() });
            }
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => {
                return Err(SetupError::new(
                    ErrorKind::Timeout,
                    format!("the device hasn't connected to the hub's MQTT broker within {} s", PROBE_WAIT.as_secs()),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_hub::TestHub;
    use crate::device::{Capabilities, Switch};
    use crate::templates::{Known, Template};

    fn tasmota() -> Template {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/tasmota-switch.json");
        let t: Template = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        t.check(&Known { adapters: &["mqtt"], capabilities: &crate::device::CAPABILITY_NAMES }).unwrap();
        t
    }

    fn adapter(t: Template, broker: Arc<Broker>) -> MqttGeneric {
        let a = MqttGeneric::new();
        a.set_templates(Arc::new(Templates::from_list(vec![t])));
        a.set_broker(broker);
        a
    }

    fn device(t: &Template) -> Device {
        Device {
            id: "plug".into(),
            name: "Plug".into(),
            room: String::new(),
            template: t.id.clone(),
            source: crate::device::Source::new("mqtt"),
            config: [("topic".to_string(), "kitchen".to_string())].into(),
            identity: String::new(),
            online: None,
            last_seen: None,
            favourite: false,
            capabilities: Capabilities::with_defaults(&t.capabilities).unwrap(),
        }
    }

    #[test]
    fn messages_are_read() {
        let reads: BTreeMap<String, HttpValue> = [
            ("switch.on".to_string(), HttpValue::Path("$".into())),
        ]
        .into();
        assert_eq!(read_message(&reads, b"ON"), vec![("switch.on".to_string(), json!(true))]);
        let reads: BTreeMap<String, HttpValue> = [
            ("switch.on".to_string(), HttpValue::Path("POWER".into())),
            ("sensor.chip_temperature".to_string(), HttpValue::Full { path: "ESP32.Temperature".into(), unit: "°C".into(), scale: None }),
        ]
        .into();
        let got = read_message(&reads, br#"{"POWER":"OFF","ESP32":{"Temperature":41.5}}"#);
        assert!(got.contains(&("switch.on".to_string(), json!(false))));
        assert!(got.contains(&("sensor.chip_temperature".to_string(), json!({"value": 41.5, "unit": "°C"}))));
        /* Not this message's fields: nothing. */
        assert!(read_message(&reads, br#"{"Other":1}"#).is_empty());
    }

    #[test]
    fn login_names_are_safe() {
        let t = tasmota();
        let spec = t.mqtt.unwrap();
        let config = [("topic".to_string(), "Kitchen Plug/1".to_string())].into();
        assert_eq!(login_name(&spec, &config), "tasmota-kitchen-plug-1");
    }

    #[tokio::test]
    async fn follows_and_commands_a_tasmota() {
        let (broker, mut published) = Broker::for_tests();
        /* Retained before the task starts: still seen. */
        broker.inject("tele/kitchen/LWT", "Online");
        let t = tasmota();
        let mut hub = TestHub::start(device(&t), Box::new(adapter(t, broker.clone()))).await;
        hub.until(|d| d.online == Some(Health::Online)).await;
        /* It asked for the state at start. */
        let (topic, _) = published.recv().await.unwrap();
        assert_eq!(topic, "cmnd/kitchen/STATE");

        /* The device's own report. */
        broker.inject("tele/kitchen/STATE", r#"{"POWER":"ON"}"#);
        hub.until(|d| d.capabilities.switch == Some(Switch { on: true })).await;

        /* A command: published, confirmed by the device's answer. */
        let b = broker.clone();
        let answer = tokio::spawn(async move {
            let (topic, payload) = published.recv().await.unwrap();
            assert_eq!((topic.as_str(), payload.as_slice()), ("cmnd/kitchen/POWER", b"OFF".as_slice()));
            b.inject("stat/kitchen/POWER", "OFF");
            published
        });
        let d = hub.control.command("plug", "switch", json!({"on": false})).await.unwrap();
        assert_eq!(d.capabilities.switch, Some(Switch { on: false }));
        let mut published = answer.await.unwrap();

        /* Already off, switched off again: the device answers the same
         * value -- that confirms it too (found with WLED on the board). */
        let b = broker.clone();
        let answer = tokio::spawn(async move {
            let _ = published.recv().await.unwrap();
            b.inject("stat/kitchen/POWER", "OFF");
            published
        });
        hub.control.command("plug", "switch", json!({"on": false})).await.unwrap();
        let mut published = answer.await.unwrap();

        /* Already off, and the device answers nothing (WLED does that):
         * confirmed at once, still sent. */
        hub.control.command("plug", "switch", json!({"on": false})).await.unwrap();
        let (topic, _) = published.recv().await.unwrap();
        assert_eq!(topic, "cmnd/kitchen/POWER");

        /* No answer: an error, not a hang. */
        let err = hub.control.command("plug", "switch", json!({"on": true})).await.unwrap_err();
        assert!(err.contains("didn't confirm"), "{err}");
        let _ = published.recv().await;

        /* Gone (its last will): offline. */
        broker.inject("tele/kitchen/LWT", "Offline");
        hub.until(|d| d.online == Some(Health::Offline)).await;
        hub.registry.stop("plug");
    }

    /* WLED in MQTT mode: brightness 0-255 in and out, colour as #RRGGBB. */
    #[tokio::test]
    async fn follows_and_commands_a_wled() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/wled-mqtt.json");
        let t: Template = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        t.check(&Known { adapters: &["mqtt"], capabilities: &crate::device::CAPABILITY_NAMES }).unwrap();
        let (broker, mut published) = Broker::for_tests();
        let mut d = device(&t);
        d.config.insert("topic".into(), "strip".into());
        let mut hub = TestHub::start(d, Box::new(adapter(t, broker.clone()))).await;
        broker.inject("wled/strip/status", "online");
        broker.inject("wled/strip/g", "128");
        broker.inject("wled/strip/c", "#FF8800");
        let d = hub.until(|d| d.capabilities.color.as_ref().and_then(|c| c.hex.as_deref()) == Some("#FF8800")).await;
        assert_eq!(d.capabilities.switch, Some(Switch { on: true }));
        assert_eq!(d.capabilities.dimmer.unwrap().level, 50);
        /* Online comes after the state (the adapters' rule: never "online"
         * with old values), so it may arrive just after. */
        hub.until(|d| d.online == Some(Health::Online)).await;

        let b = broker.clone();
        let answer = tokio::spawn(async move {
            let (topic, payload) = published.recv().await.unwrap();
            assert_eq!((topic.as_str(), payload.as_slice()), ("wled/strip", b"51".as_slice()), "20 % = 51 of 255");
            b.inject("wled/strip/g", "51");
        });
        let d = hub.control.command("plug", "dimmer", json!({"level": 20})).await.unwrap();
        assert_eq!(d.capabilities.dimmer.unwrap().level, 20);
        answer.await.unwrap();
        hub.registry.stop("plug");
    }

    /* Battery devices (issue #72): "seen" with every report, asleep is not
     * offline, offline after MISSED_REPORTS missed reports. */
    #[tokio::test]
    async fn battery_devices_are_seen_not_offline_while_asleep() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/tasmota-sensor-battery.json");
        let mut json: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        json["report_interval_s"] = json!(1);
        let t: Template = serde_json::from_value(json).unwrap();
        t.check(&Known { adapters: &["mqtt"], capabilities: &crate::device::CAPABILITY_NAMES }).unwrap();
        let (broker, _published) = Broker::for_tests();
        let mut hub = TestHub::start(device(&t), Box::new(adapter(t, broker.clone()))).await;

        /* It wakes and reports. */
        broker.inject("tele/kitchen/LWT", "Online");
        broker.inject("tele/kitchen/SENSOR", r#"{"Time":"x","AM2301":{"Temperature":21.5,"Humidity":48.0}}"#);
        let d = hub.until(|d| d.capabilities.sensor.as_ref().is_some_and(|s| s.readings.contains_key("humidity"))).await;
        assert_eq!(d.capabilities.sensor.as_ref().unwrap().readings["temperature"].value, 21.5);
        let d = hub.until(|d| d.last_seen.is_some() && d.online == Some(Health::Online)).await;
        assert!(d.last_seen.unwrap() > 1_700_000_000);

        /* Falls asleep, saying goodbye: still online. */
        broker.inject("tele/kitchen/LWT", "Offline");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(hub.control.get("plug").await.unwrap().unwrap().online, Some(Health::Online));

        /* Three reports missed (interval 1 s here): offline. */
        let d = hub.until(|d| d.online == Some(Health::Offline)).await;
        assert!(d.last_seen.is_some(), "still says when it was last seen");
        /* It wakes again: online. */
        broker.inject("tele/kitchen/STATE", r#"{"Wifi":{"Signal":-61}}"#);
        hub.until(|d| d.online == Some(Health::Online)).await;
        hub.registry.stop("plug");
    }

    #[tokio::test]
    async fn setup_makes_a_login_and_waits_for_the_device() {
        let (broker, _published) = Broker::for_tests();
        let t = tasmota();
        let id = t.id.clone();
        let a = adapter(t, broker.clone());
        let mut values = SetupValues { template: id, ..Default::default() };
        values.plain.insert("topic".into(), "kitchen".into());
        let made = a.action("mqtt_login", &values).await.unwrap();
        assert_eq!(made.plain["mqtt_user"], "tasmota-kitchen");
        assert_eq!(made.plain["mqtt_port"], "1883");
        assert_eq!(made.secret["mqtt_password"].expose().len(), 20);
        assert!(broker.has_login("tasmota-kitchen"));

        /* Not connected yet: the test step says so. */
        assert_eq!(a.probe(&values).await.unwrap_err().kind, ErrorKind::Timeout);
        /* Connected (its last will says Online). */
        broker.inject("tele/kitchen/LWT", "Online");
        assert_eq!(a.probe(&values).await.unwrap().summary, "Connected to the hub");
    }
}
