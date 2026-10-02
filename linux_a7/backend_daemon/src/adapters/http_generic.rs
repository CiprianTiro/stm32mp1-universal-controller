/*
 * http_generic.rs -- the GENERIC HTTP adapter, "http" (issue #75).
 *
 * Many simple devices have a local HTTP API with JSON answers: a Shelly
 * plug (/rpc/Switch.Set?id=0&on=true), a Tasmota relay
 * (/cm?cmnd=Power%20ON), DIY ESP firmware. Writing Rust for each would be
 * the same code with different URLs. Instead, the device's TEMPLATE
 * describes its API in an "http" block (templates.rs, HttpSpec) and this
 * one adapter carries it out:
 *   - state: GET each "state" request, take the values at the given JSON
 *     paths ("output", "aenergy.total") -> the capabilities;
 *   - commands: one request per capability, its {placeholders} filled
 *     with the command's values ({on}, {level}, {r}...);
 *   - setup's test step: the "probe" request (is it really that device?
 *     what's its MAC?), or else just reading the state.
 * So supporting a new simple device = writing a JSON file. Devices that
 * push changes or speak anything more complex keep adapters of their own
 * (WLED, the LG TV): this adapter only POLLS, every "poll_s".
 *
 * Capabilities it can run: switch, dimmer, color (set and read), sensor
 * and energy (read). Templates are checked at load (templates.rs) so a template that
 * reads a capability it doesn't have, or uses a placeholder nobody fills,
 * never gets this far.
 *
 * The TEMPLATES are loaded after the adapters (checking them needs the
 * adapters' names), so this adapter gets them later, through `set_templates`
 * -- before any device starts (main.rs).
 */
use hyper::Method;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

use super::net;
use super::wled::{hex_to_rgb, kelvin_to_rgb, rgb_to_hex};
use super::{Adapter, BoxFuture, DeviceCmd, DeviceHandle, Hub, Probe, SetupError, SetupValues};
use crate::device::{Device, Health};
use crate::templates::{ErrorKind, HttpMethod, HttpRequest, HttpSpec, HttpValue, Templates, GENERIC_HTTP_ADAPTER};

/* While the device doesn't answer: tried again after RETRY_MIN, doubling
 * up to RETRY_MAX. */
const RETRY_MIN: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 5 });
const RETRY_MAX: Duration = Duration::from_secs(60);

/* Cheap to clone: main.rs keeps a clone to hand it the templates once
 * they're loaded, the registry owns the other. */
#[derive(Clone, Default)]
pub struct HttpGeneric {
    templates: Arc<OnceLock<Arc<Templates>>>,
}

impl HttpGeneric {
    pub fn new() -> Self {
        Self::default()
    }

    /* Once, when the templates are loaded (main.rs). */
    pub fn set_templates(&self, templates: Arc<Templates>) {
        let _ = self.templates.set(templates);
    }

    /* The "http" block of a template; None if there's no such template
     * (any more) or it has none. */
    fn spec(&self, template: &str) -> Option<HttpSpec> {
        self.templates.get()?.get(template)?.http.clone()
    }
}

impl Adapter for HttpGeneric {
    fn id(&self) -> &'static str {
        GENERIC_HTTP_ADAPTER
    }

    fn start(&self, device: &Device, hub: Hub) -> DeviceHandle {
        let (commands, commands_rx) = mpsc::channel(8);
        let task = Task {
            id: device.id.clone(),
            config: device.config.clone(),
            spec: self.spec(&device.template),
            hub,
        };
        /* Issue #72: a webhook can ask for the state at once. */
        let refresh = Arc::new(tokio::sync::Notify::new());
        tokio::spawn(task.run(commands_rx, refresh.clone()));
        DeviceHandle::new(commands).with_refresh(refresh)
    }

    fn probe<'a>(&'a self, values: &'a SetupValues) -> BoxFuture<'a, Result<Probe, SetupError>> {
        Box::pin(async move {
            let spec = self.spec(&values.template).ok_or_else(|| {
                SetupError::new(ErrorKind::Unsupported, format!("template {:?} has no http block", values.template))
            })?;
            probe(&spec, &values.plain).await
        })
    }
}

/* One device's task. */
struct Task {
    id: String,
    /* The device's settings: "host", and whatever else its template's
     * requests use as {placeholders}. */
    config: BTreeMap<String, String>,
    spec: Option<HttpSpec>,
    hub: Hub,
}

impl Task {
    async fn run(self, mut commands: mpsc::Receiver<DeviceCmd>, refresh: Arc<tokio::sync::Notify>) {
        let (Some(spec), Some(host)) = (self.spec.clone(), self.config.get("host").cloned()) else {
            /* No template (removed from the hub) or no address: stay put
             * and say why. */
            let why = if self.spec.is_none() { "its template has no http block" } else { "no address set" };
            println!("http: {}: {why}", self.id);
            self.hub.set_online(&self.id, Health::Offline).await;
            while let Some(cmd) = commands.recv().await {
                cmd.refuse(format!("{}: {why}", self.id));
            }
            return;
        };
        /* (Every second in tests, which wait for polls.) */
        let poll_every = Duration::from_secs(if cfg!(test) { 1 } else { u64::from(spec.poll_s) });
        let mut wait = Duration::ZERO;
        let mut retry = RETRY_MIN;
        let mut online = None;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                /* "Read your state now" (a webhook): the poll comes early. */
                _ = refresh.notified() => {}
                cmd = commands.recv() => match cmd {
                    None => return,
                    Some(cmd) => {
                        self.handle(&spec, &host, cmd).await;
                        continue;
                    }
                },
            }
            match self.poll(&spec, &host).await {
                Ok(()) => {
                    if online != Some(true) {
                        println!("http: {} answers at {host}", self.id);
                    }
                    online = Some(true);
                    retry = RETRY_MIN;
                    wait = poll_every;
                }
                Err(e) => {
                    if online != Some(false) {
                        println!("http: {}: {e} (will keep trying)", self.id);
                    }
                    online = Some(false);
                    self.hub.set_online(&self.id, Health::Offline).await;
                    wait = retry;
                    retry = (retry * 2).min(RETRY_MAX);
                }
            }
        }
    }

    /* Every state request; then everything they said is reported. */
    async fn poll(&self, spec: &HttpSpec, host: &str) -> Result<(), String> {
        let state = read_state(spec, host, &self.config).await?;
        for (capability, value) in state {
            self.hub.report(&self.id, &capability, value).await?;
        }
        self.hub.set_online(&self.id, Health::Online).await;
        Ok(())
    }

    async fn handle(&self, spec: &HttpSpec, host: &str, cmd: DeviceCmd) {
        let DeviceCmd::Command { capability, value, reply } = cmd else {
            cmd.refuse(format!("{} has no actions", self.id));
            return;
        };
        let result = async {
            let request = spec
                .commands
                .get(&capability)
                .ok_or_else(|| format!("{} can't set {capability:?}", self.id))?;
            let vars = command_vars(&capability, &value)?;
            let (path, body) = build(request, &vars, &self.config);
            net::http_request(method(request.method), host, &path, body.as_ref()).await?;
            /* What the device now says is the confirmation. */
            self.poll(spec, host).await
        }
        .await;
        let _ = reply.send(result);
    }
}

fn method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
    }
}

/* ------------------------------------------------------------------ */
/* Reading the state                                                   */
/* ------------------------------------------------------------------ */

/* All state requests -> {capability: value}, ready to report. A value
 * the device didn't send (a path that isn't in the reply) is an error:
 * the template doesn't fit this device. */
async fn read_state(spec: &HttpSpec, host: &str, config: &BTreeMap<String, String>) -> Result<BTreeMap<String, Value>, String> {
    let mut state: BTreeMap<String, Value> = BTreeMap::new();
    for request in &spec.state {
        let path = fill(&request.path, &BTreeMap::new(), config);
        let reply = net::http_json(Method::GET, host, &path, None).await?;
        for (target, how) in &request.read {
            let (capability, field) = target.split_once('.').unwrap_or((target, ""));
            let found = json_path(&reply, how.path())
                .filter(|v| !v.is_null())
                .ok_or_else(|| format!("{host}{path}: no {:?} in the answer", how.path()))?;
            let value = convert(capability, found, how).ok_or_else(|| format!("{host}{path}: {:?} isn't a {target}: {found}", how.path()))?;
            let entry = state.entry(capability.to_string()).or_insert_with(|| json!({}));
            if capability == "sensor" {
                entry["readings"][field] = value;
            } else {
                entry[field] = value;
            }
        }
    }
    Ok(state)
}

/* "a.b.0.c" in a JSON value: object keys, list indexes. */
pub(crate) fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |v, key| match v {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

/* A device's value -> the capability field's. Lenient about types: an
 * "on" may be true, 1 or "ON"; a number may come as text. */
pub(crate) fn convert(capability: &str, value: &Value, how: &HttpValue) -> Option<Value> {
    let number = || match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match capability {
        "switch" => Some(json!(match value {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64()? != 0.0,
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "on" | "true" | "1" | "yes" => true,
                "off" | "false" | "0" | "no" => false,
                _ => return None,
            },
            _ => return None,
        })),
        /* "scale" for devices counting 0-255 (WLED over MQTT, #72):
         * 100/255 = 0.392157 turns their number into 0-100 %. */
        "dimmer" => {
            let scale = match how {
                HttpValue::Full { scale, .. } => scale.unwrap_or(1.0),
                HttpValue::Path(_) => 1.0,
            };
            Some(json!((number()? * scale).round().clamp(0.0, 100.0) as u8))
        }
        "color" => {
            let text = value.as_str()?;
            let hex = if text.starts_with('#') { text.to_string() } else { format!("#{text}") };
            Some(json!(rgb_to_hex(hex_to_rgb(&hex.to_ascii_uppercase())?)))
        }
        "sensor" => {
            let (unit, scale) = match how {
                HttpValue::Full { unit, scale, .. } => (unit.clone(), scale.unwrap_or(1.0)),
                HttpValue::Path(_) => (String::new(), 1.0),
            };
            Some(json!({ "value": number()? * scale, "unit": unit }))
        }
        /* Issue #77: one number per field; the field name says the unit. */
        "energy" => {
            let scale = match how {
                HttpValue::Full { scale, .. } => scale.unwrap_or(1.0),
                HttpValue::Path(_) => 1.0,
            };
            Some(json!(number()? * scale))
        }
        _ => None,
    }
}

/* ------------------------------------------------------------------ */
/* Commands                                                            */
/* ------------------------------------------------------------------ */

/* A command's values, as JSON (typed: true, 50, "FF8800"). */
pub(crate) fn command_vars(capability: &str, value: &Value) -> Result<Map<String, Value>, String> {
    let mut vars = Map::new();
    match capability {
        "switch" => {
            vars.insert("on".into(), json!(value["on"].as_bool().ok_or("switch needs {\"on\": true|false}")?));
        }
        "dimmer" => {
            let level = value["level"].as_u64().ok_or("dimmer needs {\"level\": 0-100}")?.min(100);
            vars.insert("level".into(), json!(level));
            /* The same as 0-255, for devices counting that way (#72). */
            vars.insert("level255".into(), json!((level * 255 + 50) / 100));
        }
        "color" => {
            /* A white temperature becomes its RGB: the generic adapter
             * only knows colours. */
            let rgb = match (value["hex"].as_str(), value["kelvin"].as_u64()) {
                (Some(hex), _) => hex_to_rgb(hex).ok_or_else(|| format!("invalid color {hex:?}"))?,
                (None, Some(k)) => kelvin_to_rgb(u16::try_from(k).unwrap_or(u16::MAX)),
                _ => return Err("color needs \"hex\" or \"kelvin\"".into()),
            };
            vars.insert("hex".into(), json!(rgb_to_hex(rgb).trim_start_matches('#')));
            for (name, channel) in ["r", "g", "b"].iter().zip(rgb) {
                vars.insert(name.to_string(), json!(channel));
            }
        }
        other => return Err(format!("the http adapter can't set {other:?}")),
    }
    Ok(vars)
}

/* The request's path and body with every placeholder filled. */
fn build(request: &HttpRequest, vars: &Map<String, Value>, config: &BTreeMap<String, String>) -> (String, Option<Value>) {
    /* {on} as the template wants it written: ["OFF", "ON"]. */
    let mut vars = vars.clone();
    if let (Some([off, on]), Some(b)) = (&request.bool, vars.get("on").and_then(Value::as_bool)) {
        vars.insert("on".into(), json!(if b { on } else { off }));
    }
    let texts: BTreeMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string)))
        .collect();
    let path = fill_path(&request.path, &texts, config);
    let body = request.body.as_ref().map(|b| fill_body(b, &vars, &texts, config));
    (path, body)
}

/* A body: a string that is one placeholder becomes the typed value; other
 * strings get text substituted; numbers etc. stay. */
fn fill_body(body: &Value, vars: &Map<String, Value>, texts: &BTreeMap<String, String>, config: &BTreeMap<String, String>) -> Value {
    match body {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
                if let Some(v) = vars.get(name) {
                    return v.clone();
                }
            }
            Value::String(fill(s, texts, config))
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| fill_body(v, vars, texts, config)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), fill_body(v, vars, texts, config))).collect()),
        other => other.clone(),
    }
}

/* {name} -> the command's value, else the config's. (Checked at load:
 * every name is one or the other.) */
fn fill(text: &str, vars: &BTreeMap<String, String>, config: &BTreeMap<String, String>) -> String {
    let mut all = config.clone();
    all.extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
    crate::discovery::fill(text, &all)
}

/* The same in a URL path: values are %-encoded, so a setting with a
 * space or "&" can't break the URL (or add a parameter). */
fn fill_path(path: &str, vars: &BTreeMap<String, String>, config: &BTreeMap<String, String>) -> String {
    let encode = |m: &BTreeMap<String, String>| m.iter().map(|(k, v)| (k.clone(), percent_encode(v))).collect();
    fill(path, &encode(vars), &encode(config))
}

fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/* ------------------------------------------------------------------ */
/* Setup                                                               */
/* ------------------------------------------------------------------ */

async fn probe(spec: &HttpSpec, values: &BTreeMap<String, String>) -> Result<Probe, SetupError> {
    let host = values
        .get("host")
        .ok_or_else(|| SetupError::new(ErrorKind::Unsupported, "no address given"))?;
    let Some(how) = &spec.probe else {
        /* No probe described: reading the state the way the task will is
         * the test. */
        read_state(spec, host, values)
            .await
            .map_err(|e| SetupError::new(ErrorKind::Unsupported, e))?;
        return Ok(Probe {
            summary: format!("Answers at {host}"),
            ..Default::default()
        });
    };
    let path = fill_path(&how.path, &BTreeMap::new(), values);
    let reply = net::http_json(Method::GET, host, &path, None).await?;
    let text = |path: &str| json_path(&reply, path).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string));
    for (path, wanted) in &how.require {
        if text(path).as_deref() != Some(wanted.as_str()) {
            return Err(SetupError::new(
                ErrorKind::Unsupported,
                format!("{host} answers, but {path} is {} (expected {wanted})", text(path).unwrap_or_else(|| "missing".into())),
            ));
        }
    }
    let learned = how
        .values
        .iter()
        .filter_map(|(name, path)| Some((name.clone(), text(path).filter(|t| t != "null")?)))
        .collect();
    let name = how.name.as_deref().and_then(text).filter(|n| !n.is_empty() && n != "null");
    /* The summary's {placeholders} are paths into the reply. */
    let mut summary = String::new();
    let mut rest = how.summary.as_str();
    while let Some(open) = rest.find('{') {
        summary.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}').unwrap_or(after.len());
        summary.push_str(&text(&after[..close]).unwrap_or_default());
        rest = after.get(close + 1..).unwrap_or("");
    }
    summary.push_str(rest);
    Ok(Probe {
        values: learned,
        name,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::http_sim::Sim;
    use crate::adapters::test_hub::TestHub;
    use crate::device::{Capabilities, Dimmer, Sensor, Switch};
    use crate::templates::{Known, Template};

    fn template(file: &str) -> Template {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates").join(file);
        let t: Template = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        t.check(&Known {
            adapters: &["http"],
            capabilities: &crate::device::CAPABILITY_NAMES,
        })
        .unwrap();
        t
    }

    /* A made-up DIY light with a POST API, to test bodies, dimmer and
     * colour (the Shelly plug only switches). */
    const DIY: &str = r#"{
        "format": 1, "id": "diy-light", "version": 1, "name": "DIY light", "category": "lighting",
        "adapter": "http", "capabilities": ["switch", "dimmer", "color"],
        "inputs": [ { "id": "host", "type": "text", "label": "Address" } ],
        "setup": [ { "id": "a", "label": "A", "steps": [ { "type": "form", "fields": ["host"] }, { "type": "test" } ] } ],
        "http": {
            "poll_s": 2,
            "state": [ { "path": "/diy/state", "read": { "switch.on": "power", "dimmer.level": "light.bri", "color.hex": "light.rgb" } } ],
            "commands": {
                "switch": { "method": "POST", "path": "/diy/set", "body": { "power": "{on}" }, "bool": ["OFF", "ON"] },
                "dimmer": { "method": "POST", "path": "/diy/set", "body": { "light": { "bri": "{level}" } } },
                "color":  { "method": "POST", "path": "/diy/set", "body": { "light": { "rgb": "{hex}", "parts": ["{r}", "{g}", "{b}"] } } }
            }
        }
    }"#;

    fn diy() -> Template {
        let t: Template = serde_json::from_str(DIY).unwrap();
        t.check(&Known {
            adapters: &["http"],
            capabilities: &crate::device::CAPABILITY_NAMES,
        })
        .unwrap();
        t
    }

    fn adapter(templates: Vec<Template>) -> HttpGeneric {
        let adapter = HttpGeneric::new();
        adapter.set_templates(Arc::new(Templates::from_list(templates)));
        adapter
    }

    fn device(template: &Template, host: &str) -> Device {
        Device {
            id: "dev".into(),
            name: "Dev".into(),
            room: String::new(),
            template: template.id.clone(),
            source: crate::device::Source::new("http"),
            config: [("host".to_string(), host.to_string())].into(),
            identity: String::new(),
            online: None,
            last_seen: None,
            capabilities: Capabilities::with_defaults(&template.capabilities).unwrap(),
        }
    }

    #[test]
    fn json_paths() {
        let v = json!({"a": {"b": [10, {"c": "x"}]}});
        assert_eq!(json_path(&v, "a.b.0"), Some(&json!(10)));
        assert_eq!(json_path(&v, "a.b.1.c"), Some(&json!("x")));
        assert_eq!(json_path(&v, "a.x"), None);
        assert_eq!(json_path(&v, "a.b.9"), None);
    }

    #[test]
    fn values_are_read_leniently() {
        let path = HttpValue::Path("x".into());
        assert_eq!(convert("switch", &json!("ON"), &path), Some(json!(true)));
        assert_eq!(convert("switch", &json!(0), &path), Some(json!(false)));
        assert_eq!(convert("switch", &json!("maybe"), &path), None);
        assert_eq!(convert("dimmer", &json!("42.6"), &path), Some(json!(43)));
        assert_eq!(convert("color", &json!("ff8800"), &path), Some(json!("#FF8800")));
        let mw = HttpValue::Full {
            path: "x".into(),
            unit: "W".into(),
            scale: Some(0.001),
        };
        assert_eq!(convert("sensor", &json!(1500), &mw), Some(json!({"value": 1.5, "unit": "W"})));
        assert_eq!(convert("energy", &json!("1500"), &mw), Some(json!(1.5)));
    }

    #[test]
    fn commands_fill_paths_and_bodies() {
        let t = diy();
        let http = t.http.unwrap();
        let config: BTreeMap<String, String> = [("host".to_string(), "h".to_string())].into();
        let (path, body) = build(&http.commands["switch"], &command_vars("switch", &json!({"on": true})).unwrap(), &config);
        assert_eq!(path, "/diy/set");
        assert_eq!(body.unwrap(), json!({"power": "ON"}));
        let (_, body) = build(&http.commands["dimmer"], &command_vars("dimmer", &json!({"level": 40})).unwrap(), &config);
        assert_eq!(body.unwrap(), json!({"light": {"bri": 40}}), "a whole-placeholder string becomes a number");
        let (_, body) = build(&http.commands["color"], &command_vars("color", &json!({"hex": "#FF8800"})).unwrap(), &config);
        assert_eq!(body.unwrap(), json!({"light": {"rgb": "FF8800", "parts": [255, 136, 0]}}));

        /* In a path, values are %-encoded. */
        let request = HttpRequest {
            method: HttpMethod::Get,
            path: "/cm?cmnd=Power%20{on}&user={user}".into(),
            body: None,
            bool: Some(["OFF".into(), "ON".into()]),
        };
        let config: BTreeMap<String, String> = [("user".to_string(), "a b&c".to_string())].into();
        let (path, _) = build(&request, &command_vars("switch", &json!({"on": false})).unwrap(), &config);
        assert_eq!(path, "/cm?cmnd=Power%20OFF&user=a%20b%26c");
    }

    #[tokio::test]
    async fn runs_a_shelly_plug_from_its_template_only() {
        let sim = Sim::start().await;
        let t = template("shelly-plug-gen3.json");
        let mut hub = TestHub::start(device(&t, &sim.host()), Box::new(adapter(vec![t]))).await;

        let d = hub.until(|d| d.online == Some(Health::Online)).await;
        assert_eq!(d.capabilities.switch, Some(Switch { on: false }));
        let energy = d.capabilities.energy.as_ref().unwrap();
        assert_eq!(energy.voltage_v, Some(231.4));
        /* 1234.567 Wh, read with "scale": 0.001. */
        assert!((energy.energy_kwh.unwrap() - 1.234567).abs() < 1e-9);
        assert_eq!(d.capabilities.sensor.as_ref().unwrap().readings["temperature"].unit, "°C");

        let d = hub.control.command("dev", "switch", json!({"on": true})).await.unwrap();
        assert_eq!(d.capabilities.switch, Some(Switch { on: true }));
        assert!(sim.shelly_on());
        /* The sim draws 40 W while on; the confirmation poll saw it. */
        assert_eq!(d.capabilities.energy.unwrap().power_w, 40.0);

        /* The button on the plug: seen at the next poll. */
        sim.press_button();
        hub.until(|d| d.capabilities.switch == Some(Switch { on: false })).await;

        sim.stop();
        hub.until(|d| d.online == Some(Health::Offline)).await;
        hub.registry.stop("dev");
    }

    #[tokio::test]
    async fn runs_a_light_with_post_bodies() {
        let sim = Sim::start().await;
        let t = diy();
        let mut hub = TestHub::start(device(&t, &sim.host()), Box::new(adapter(vec![t]))).await;
        hub.until(|d| d.online == Some(Health::Online)).await;
        let d = hub.control.command("dev", "dimmer", json!({"level": 25})).await.unwrap();
        assert_eq!(d.capabilities.dimmer, Some(Dimmer { level: 25 }));
        let d = hub.control.command("dev", "color", json!({"hex": "#00FF00"})).await.unwrap();
        assert_eq!(d.capabilities.color.unwrap().hex.as_deref(), Some("#00FF00"));
        let d = hub.control.command("dev", "switch", json!({"on": false})).await.unwrap();
        assert_eq!(d.capabilities.switch, Some(Switch { on: false }));
        assert_eq!(sim.diy()["power"], "OFF");
        hub.registry.stop("dev");
    }

    #[tokio::test]
    async fn probe_checks_it_is_that_device() {
        let sim = Sim::start().await;
        let t = template("shelly-plug-gen3.json");
        let id = t.id.clone();
        let adapter = adapter(vec![t, diy()]);
        let values = |template: &str| SetupValues {
            plain: [("host".to_string(), sim.host())].into(),
            template: template.into(),
            ..Default::default()
        };
        let found = adapter.probe(&values(&id)).await.unwrap();
        assert_eq!(found.values["shelly_id"], "shellyplugsg3-8cbfea9a1b2c");
        assert_eq!(found.values["mac"], "8CBFEA9A1B2C");
        assert_eq!(found.name, None, "a Shelly without a name set says null");
        assert_eq!(found.summary, "Shelly S3PL-00112EU, firmware 1.4.4");

        /* With a password set, the hub can't use it (yet): refused. */
        sim.set_auth(true);
        let err = adapter.probe(&values(&id)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unsupported);
        assert!(err.detail.contains("auth_en"), "{}", err.detail);

        /* No probe in the template: reading the state is the test. */
        assert_eq!(adapter.probe(&values("diy-light")).await.unwrap().summary, format!("Answers at {}", sim.host()));
    }

    #[test]
    fn sensors_from_several_paths_merge() {
        /* Two readings land in ONE sensor value. */
        let mut state: BTreeMap<String, Value> = BTreeMap::new();
        for (field, value) in [("power", 1.0), ("voltage", 2.0)] {
            let entry = state.entry("sensor".into()).or_insert_with(|| json!({}));
            entry["readings"][field] = json!({"value": value, "unit": ""});
        }
        let sensor: Sensor = serde_json::from_value(state["sensor"].clone()).unwrap();
        assert_eq!(sensor.readings.len(), 2);
    }

    /* Against a REAL Shelly Gen2+/Gen3 plug (not run by default):
     *   SHELLY_HOST=192.168.1.70 cargo test http_generic::tests::live -- --ignored --nocapture
     * Switches it off and on, then back to how it was. */
    #[tokio::test]
    #[ignore]
    async fn live() {
        let host = std::env::var("SHELLY_HOST").expect("set SHELLY_HOST");
        let t = template("shelly-plug-gen3.json");
        let id = t.id.clone();
        let adapter = adapter(vec![t.clone()]);
        let values = SetupValues {
            plain: [("host".to_string(), host.clone())].into(),
            template: id,
            ..Default::default()
        };
        println!("probe: {:?}", adapter.probe(&values).await.unwrap());
        let mut hub = TestHub::start(device(&t, &host), Box::new(adapter)).await;
        let found = hub.until(|d| d.online == Some(Health::Online)).await;
        println!("found: {:?}", found.capabilities);
        let was = found.capabilities.switch.unwrap().on;
        for on in [!was, was] {
            let d = hub.control.command("dev", "switch", json!({"on": on})).await.unwrap();
            println!("on={on} -> {:?}", d.capabilities);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        hub.registry.stop("dev");
    }
}
