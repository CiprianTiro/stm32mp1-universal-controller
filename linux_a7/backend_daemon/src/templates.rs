/*
 * templates.rs -- device templates (issue #40): what KIND of device
 * something is, and how to set it up, described as data.
 *
 * A template is one JSON file per device type (templates/<id>.json in the
 * repo, /usr/share/universal-controller/templates on the hub). It says
 * which capabilities the type has, which ADAPTER (the Rust code for its
 * protocol) runs it, how to find it on the network, and which setup steps
 * the wizard shows. The format is documented on the wiki's
 * Device-Templates page -- this file is its exact definition.
 *
 * WHAT'S HERE: the format as Rust types (serde turns the JSON into them),
 * loading every template from a folder, and CHECKING each one. A template
 * with a mistake (a step asking for an input that doesn't exist, an
 * unknown capability, a typo in a field name) is refused at start with a
 * message saying what's wrong -- never a crash, never a wizard that breaks
 * halfway for the person adding a device. The other templates still load.
 *
 * Some fields are in the format already but used by later issues
 * (onboarding #71, hub services #72, more discovery #73, vendor logins
 * #74): they're parsed and checked now, so a template written today stays
 * valid.
 *
 * Pure (no network), unit-tested below -- including against the real
 * templates in the repo.
 */
// Many fields are only READ by the wizard, which comes later in #40 (they
// are parsed and checked already). Remove this once the wizard uses them.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/* The template FORMAT version this code understands. A template says
 * which one it's written in ("format": 1); a later format can add fields
 * and this number goes up. */
pub const FORMAT_VERSION: u32 = 1;

/* Where the image installs them (see backend-daemon.bb). */
pub const TEMPLATE_DIR: &str = "/usr/share/universal-controller/templates";

/* The wizard's first menu. */
pub const CATEGORIES: [&str; 11] = [
    "lighting", "plugs", "media", "climate", "covers", "cameras", "vacuums", "sensors", "locks", "energy", "other",
];

/* ------------------------------------------------------------------ */
/* The format                                                          */
/* ------------------------------------------------------------------ */

/* `deny_unknown_fields` everywhere: a misspelt field ("discovry") is an
 * error, not silently ignored. The only extra field allowed is
 * "$comment", for notes in the file itself. */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Template {
    #[serde(rename = "$comment", default)]
    pub comment: Option<String>,
    pub format: u32,
    /* Unique, [a-z0-9-]: stored in every device it creates. */
    pub id: String,
    /* This template's own version (not the format's). */
    pub version: u32,
    pub name: String,
    pub category: String,
    #[serde(default)]
    pub description: String,
    /* Which adapter (code) runs devices of this type. */
    pub adapter: String,
    /* Fixed settings handed to the adapter, e.g. {"protocol": "miio"}. */
    #[serde(default)]
    pub adapter_config: serde_json::Map<String, serde_json::Value>,
    /* Capability names (device.rs), e.g. ["switch", "dimmer"]. */
    pub capabilities: Vec<String>,
    /* Part of the hub itself: created automatically, never added or
     * removed through the wizard (the M4's LED, #41's relays). */
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub inputs: Vec<Input>,
    #[serde(default)]
    pub discovery: Vec<Discovery>,
    /* One or more variants ("guided", "advanced"), each a list of steps. */
    #[serde(default)]
    pub setup: Vec<Variant>,
    /* Plain-language messages replacing the built-in ones, per error kind. */
    #[serde(default)]
    pub errors: BTreeMap<ErrorKind, String>,
    /* What stays the same when the IP address changes, e.g. "{mac}":
     * filled from the values setup collected. Empty = no stable identity
     * (the device is only known by its address). */
    #[serde(default)]
    pub identity: String,
    #[serde(default)]
    pub connection: Connection,
    #[serde(default)]
    pub power: Power,
    /* For battery devices: how often they report (power = battery). */
    #[serde(default)]
    pub report_interval_s: Option<u32>,
    #[serde(default)]
    pub tls: Tls,
    #[serde(default)]
    pub wake: Wake,
    /* Adapter actions to repeat when access is revoked ("Pair again"). */
    #[serde(default)]
    pub reauth: Vec<String>,
    /* Issue #74 (pattern P5): the device is only reached through its
     * vendor's cloud -- it works only while the hub has internet. Screens
     * say so (wizard TemplateInfo, the device page). */
    #[serde(default)]
    pub cloud: bool,
    #[serde(default)]
    pub defaults: Defaults,
    /* For adapter "http" only (issue #75): the device's HTTP API, described
     * instead of programmed -- see HttpSpec. */
    #[serde(default)]
    pub http: Option<HttpSpec>,
    /* For adapter "mqtt" only (issue #72): the device's MQTT topics,
     * described instead of programmed -- see MqttSpec. */
    #[serde(default)]
    pub mqtt: Option<MqttSpec>,
}

/* One value setup collects. */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: InputKind,
    pub label: String,
    #[serde(default)]
    pub hint: String,
    #[serde(default = "yes")]
    pub required: bool,
    #[serde(default)]
    pub validate: Validate,
    /* For type "choice". */
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

fn yes() -> bool {
    true
}

/* Serialize too: the wizard sends it to clients (a field's "type"). */
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Text,
    Number,
    /* Stored apart from the device, never shown again, never logged. */
    Secret,
    Choice,
    Toggle,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

/* Checks on an input's value -- done by the backend, so the touchscreen
 * and the phone app get exactly the same ones. */
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Validate {
    #[serde(default)]
    pub pattern: Option<Pattern>,
    #[serde(default)]
    pub min_length: Option<usize>,
    #[serde(default)]
    pub max_length: Option<usize>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
}

/* Named patterns rather than regular expressions: every template gets the
 * same, tested rule for "an address", and no regex engine is needed. */
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Pattern {
    /* An IPv4 address or a host name. */
    Host,
    Ipv4,
    Email,
    Digits,
    Hex,
    /* A MAC address, 00:11:22:aa:bb:cc. */
    Mac,
}

/* How to find devices of this type on the LAN, and which values a found
 * device fills in. */
// `WsDiscovery` ends with the enum's name, which clippy flags -- but
// "WS-Discovery" IS that protocol's name.
#[allow(clippy::enum_variant_names)]
#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Discovery {
    Mdns {
        service: String,
        #[serde(default)]
        fill: BTreeMap<String, String>,
        #[serde(default, rename = "match")]
        matches: BTreeMap<String, String>,
    },
    Ssdp {
        search: String,
        #[serde(default)]
        fill: BTreeMap<String, String>,
        #[serde(default, rename = "match")]
        matches: BTreeMap<String, String>,
    },
    /* Issue #75: `probe` is sent to the broadcast address of every network
     * the hub is on, to UDP `port`; every answer is a device. */
    UdpBroadcast {
        port: u16,
        /* What to send, as text (the device's protocol decides). */
        probe: String,
        #[serde(default)]
        fill: BTreeMap<String, String>,
        #[serde(default, rename = "match")]
        matches: BTreeMap<String, String>,
    },
    /* Issue #75: devices that announce themselves to a multicast group
     * (Yeelight: 239.255.255.250:1982) -- the hub just listens. The port
     * must be opened in hub-firewall.nft. */
    UdpMulticast {
        group: std::net::Ipv4Addr,
        port: u16,
        #[serde(default)]
        fill: BTreeMap<String, String>,
        #[serde(default, rename = "match")]
        matches: BTreeMap<String, String>,
    },
    WsDiscovery {
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
    /* Issue #74: devices announcing themselves to the whole network on
     * UDP `port`, in a vendor's own packing that `decode` names (the hub
     * knows: "roborock" -> {duid}, {address}). The port must be opened in
     * hub-firewall.nft. */
    UdpListen {
        port: u16,
        decode: UdpDecoder,
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
    /* Issue #73 (netscan.rs): the hosts on the hub's own network whose
     * MAC address says one of these makers ("Espressif", see oui.rs).
     * Values: {address}, {mac}, {mac_hex}, {manufacturer}, {interface}.
     * Only searched when someone asks (the wizard's search). */
    NetworkScan {
        manufacturers: Vec<String>,
        #[serde(default)]
        fill: BTreeMap<String, String>,
        #[serde(default, rename = "match")]
        matches: BTreeMap<String, String>,
    },
    /* Issue #73: of those hosts (any maker if `manufacturers` is empty),
     * the ones answering on TCP `port`. With `get`, that page is fetched
     * there too and its JSON reply flattened into {json.<path>} values for
     * "match" and "fill" -- e.g. WLED: get "/json/info", match
     * {"json.brand": "WLED"}. */
    PortProbe {
        port: u16,
        #[serde(default)]
        manufacturers: Vec<String>,
        #[serde(default)]
        get: Option<String>,
        #[serde(default)]
        fill: BTreeMap<String, String>,
        #[serde(default, rename = "match")]
        matches: BTreeMap<String, String>,
    },
    /* (later, #72): the device connects to the hub by itself. */
    DeviceAnnounce {
        #[serde(default)]
        fill: BTreeMap<String, String>,
        /* Issue #72: e.g. {"client_kind": "DVES"} -- a Tasmota knocking on
         * the hub's MQTT broker without a login yet. */
        #[serde(default, rename = "match")]
        matches: BTreeMap<String, String>,
    },
    /* (later, #74): the devices of a vendor account. */
    CloudList {
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
}

/* The vendor packings udp_listen can read (discovery.rs). */
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum UdpDecoder {
    Roborock,
}

/* No conditions (the methods that have no "match"). */
static NO_MATCH: BTreeMap<String, String> = BTreeMap::new();

impl Discovery {
    pub fn fill(&self) -> &BTreeMap<String, String> {
        match self {
            Discovery::Mdns { fill, .. }
            | Discovery::Ssdp { fill, .. }
            | Discovery::UdpBroadcast { fill, .. }
            | Discovery::UdpMulticast { fill, .. }
            | Discovery::WsDiscovery { fill }
            | Discovery::UdpListen { fill, .. }
            | Discovery::NetworkScan { fill, .. }
            | Discovery::PortProbe { fill, .. }
            | Discovery::DeviceAnnounce { fill, .. }
            | Discovery::CloudList { fill } => fill,
        }
    }

    /* "match": values a sighting must have to be this template's device,
     * e.g. {"txt.app": "PlugSG3"} -- every Shelly answers the same mDNS
     * service, only the plug is a Shelly plug. Keys are placeholder names
     * ("txt.app", "json.method"), values compared as text. */
    pub fn matches(&self) -> &BTreeMap<String, String> {
        match self {
            Discovery::Mdns { matches, .. }
            | Discovery::Ssdp { matches, .. }
            | Discovery::UdpBroadcast { matches, .. }
            | Discovery::UdpMulticast { matches, .. }
            | Discovery::NetworkScan { matches, .. }
            | Discovery::PortProbe { matches, .. }
            | Discovery::DeviceAnnounce { matches, .. } => matches,
            _ => &NO_MATCH,
        }
    }

    /* Issue #73: the makers a network scan or port probe is limited to
     * (empty: any). Compared without case. */
    pub fn manufacturers(&self) -> &[String] {
        match self {
            Discovery::NetworkScan { manufacturers, .. } | Discovery::PortProbe { manufacturers, .. } => manufacturers,
            _ => &[],
        }
    }
}

/* ------------------------------------------------------------------ */
/* The generic HTTP adapter's description (issue #75)                  */
/* ------------------------------------------------------------------ */

/* A device whose local API is plain HTTP + JSON (Shelly, Tasmota, many
 * DIY ESP firmwares) needs no Rust of its own: the template says which
 * request sets each capability and where in which reply its state is.
 * adapters/http_generic.rs carries it out. Example (Shelly Gen2+):
 *   "http": {
 *     "poll_s": 10,
 *     "state": [ { "path": "/rpc/Switch.GetStatus?id=0",
 *                  "read": { "switch.on": "output",
 *                            "sensor.power": { "path": "apower", "unit": "W" } } } ],
 *     "commands": { "switch": { "path": "/rpc/Switch.Set?id=0&on={on}" } },
 *     "probe": { "path": "/shelly", "values": { "mac": "mac" }, "summary": "Shelly {model}" }
 *   }
 * Paths into a JSON reply are dot-separated keys and list indexes:
 * "aenergy.total", "lights.0.ison". */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HttpSpec {
    /* The state is read this often (s): changes made elsewhere (the
     * vendor's app, a button) show up at most this late. */
    #[serde(default = "default_poll_s")]
    pub poll_s: u32,
    /* The requests that read the state, each with where its values are. */
    pub state: Vec<HttpRead>,
    /* Per settable capability, the request that sets it. */
    #[serde(default)]
    pub commands: BTreeMap<String, HttpRequest>,
    /* The wizard's test step; without one, reading the state is the test. */
    #[serde(default)]
    pub probe: Option<HttpProbe>,
}

fn default_poll_s() -> u32 {
    10
}

/* One request. Placeholders in path and body are the command's values:
 *   switch  {on}               (true / false, or `bool`'s texts)
 *   dimmer  {level}            (0-100)
 *   color   {hex} {r} {g} {b}  ("FF8800" without "#", 0-255 each; a
 *                               white temperature arrives as its RGB)
 * plus any of the device's config values ({host}, a channel number...).
 * A body string that is exactly one placeholder ("{level}") becomes the
 * value itself -- a number or true/false -- not a text. */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HttpRequest {
    #[serde(default)]
    pub method: HttpMethod,
    pub path: String,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    /* How {on} is written: [text for off, text for on], e.g. ["OFF",
     * "ON"] for Tasmota. Default "false" / "true". */
    #[serde(default)]
    pub bool: Option<[String; 2]>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    #[default]
    Get,
    Post,
    Put,
}

/* A state request: GET `path`, then each "<capability>.<field>" from its
 * reply. Fields: switch.on, dimmer.level, color.hex, sensor.<reading>
 * (any name: "temperature"), and energy.power_w / energy_kwh / voltage_v /
 * current_a (#77; with "scale" for other units: Wh -> kWh is 0.001). */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HttpRead {
    pub path: String,
    pub read: BTreeMap<String, HttpValue>,
}

/* Where a value is: a path, or a path with a unit and a factor (a
 * device reporting mW, read as W: "scale": 0.001). */
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum HttpValue {
    Path(String),
    Full {
        path: String,
        #[serde(default)]
        unit: String,
        #[serde(default)]
        scale: Option<f64>,
    },
}

impl HttpValue {
    pub fn path(&self) -> &str {
        match self {
            HttpValue::Path(path) | HttpValue::Full { path, .. } => path,
        }
    }
}

/* The test step: GET `path`; every `require` path must hold that text
 * (it IS that kind of device); `values` are learned from the reply
 * ({"mac": "mac"} -- for the template's identity), `name` is the
 * device's own name, `summary` the "OK" line with {json.path}s. */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HttpProbe {
    pub path: String,
    #[serde(default)]
    pub require: BTreeMap<String, String>,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub summary: String,
}

/* The adapter that runs "http" blocks. */
pub const GENERIC_HTTP_ADAPTER: &str = "http";

/* What the http adapter can read into an energy capability (#77). */
const ENERGY_FIELDS: [&str; 4] = ["power_w", "energy_kwh", "voltage_v", "current_a"];

/* What each settable capability's command may use. */
fn command_placeholders(capability: &str) -> Option<&'static [&'static str]> {
    match capability {
        "switch" => Some(&["on"]),
        "dimmer" => Some(&["level", "level255"]),
        "color" => Some(&["hex", "r", "g", "b"]),
        _ => None,
    }
}

impl HttpSpec {
    fn check(&self, template: &Template) -> Result<(), String> {
        if !(2..=3600).contains(&self.poll_s) {
            return Err("http.poll_s must be 2-3600".into());
        }
        if self.state.is_empty() {
            return Err("http.state: at least one request reads the state".into());
        }
        let config_names: HashSet<&str> = template
            .inputs
            .iter()
            .map(|i| i.id.as_str())
            .chain(template.adapter_config.keys().map(String::as_str))
            .collect();
        // Which capabilities some request reads.
        let mut read = HashSet::new();
        for request in &self.state {
            check_path(&request.path, &config_names, &[]).map_err(|e| format!("http.state: {e}"))?;
            if request.read.is_empty() {
                return Err(format!("http.state {:?} reads nothing", request.path));
            }
            for (target, value) in &request.read {
                read.insert(check_read(template, target, value, "http")?);
            }
        }
        for capability in &template.capabilities {
            let settable = command_placeholders(capability).is_some();
            if !settable && !matches!(capability.as_str(), "sensor" | "energy") {
                return Err(format!("the http adapter can't run capability {capability:?}"));
            }
            if !read.contains(capability.as_str()) {
                return Err(format!("http.state never reads capability {capability:?}"));
            }
            if settable && !self.commands.contains_key(capability) {
                return Err(format!("http.commands has no request for {capability:?}"));
            }
        }
        for (capability, request) in &self.commands {
            let Some(allowed) = command_placeholders(capability) else {
                return Err(format!("http.commands {capability:?}: only switch, dimmer and color are set by commands"));
            };
            if !template.capabilities.contains(capability) {
                return Err(format!("http.commands {capability:?}: the template has no such capability"));
            }
            let what = format!("http.commands {capability:?}");
            check_path(&request.path, &config_names, allowed).map_err(|e| format!("{what}: {e}"))?;
            if let Some(body) = &request.body {
                check_body(body, &config_names, allowed).map_err(|e| format!("{what}: {e}"))?;
            }
            if request.bool.is_some() && capability != "switch" {
                return Err(format!("{what}: \"bool\" only goes with switch"));
            }
        }
        if let Some(probe) = &self.probe {
            check_path(&probe.path, &config_names, &[]).map_err(|e| format!("http.probe: {e}"))?;
            check_placeholders(&probe.summary).map_err(|e| format!("http.probe.summary: {e}"))?;
            for name in probe.values.keys() {
                if !valid_id_underscore(name) {
                    return Err(format!("http.probe learns {name:?}: use a-z, 0-9 and _"));
                }
            }
        }
        Ok(())
    }
}

/* ------------------------------------------------------------------ */
/* The generic MQTT adapter's description (issue #72)                  */
/* ------------------------------------------------------------------ */

/* A device that talks to the hub's own MQTT broker (broker.rs): Tasmota,
 * WLED's MQTT mode, ESPHome. Like the http block, the template says which
 * topics carry its state and how it's commanded; adapters/mqtt_generic.rs
 * carries it out. Example (Tasmota):
 *   "mqtt": {
 *     "user": "tasmota-{topic}",
 *     "acl": [ { "access": "write", "topic": "stat/{topic}/#" },
 *              { "access": "write", "topic": "tele/{topic}/#" },
 *              { "access": "read",  "topic": "cmnd/{topic}/#" } ],
 *     "online": { "topic": "tele/{topic}/LWT", "online": "Online", "offline": "Offline" },
 *     "state": [ { "topic": "stat/{topic}/RESULT", "read": { "switch.on": "POWER" } } ],
 *     "commands": { "switch": { "topic": "cmnd/{topic}/POWER", "payload": "{on}", "bool": ["OFF", "ON"] } },
 *     "refresh": { "topic": "cmnd/{topic}/STATE", "payload": "" }
 *   }
 * {placeholders} in topics are the device's settings (inputs: "topic").
 * A read's path "$" is the whole payload (Tasmota's stat/x/POWER is just
 * "ON"); any other path is into the payload's JSON. */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct MqttSpec {
    /* The device's login name on the broker, e.g. "tasmota-{topic}" --
     * lower-cased, and anything but a-z 0-9 - _ becomes "-". */
    pub user: String,
    /* Which topics its login may use (and nothing else). */
    pub acl: Vec<MqttRule>,
    /* Its "last will": online/offline as the broker knows it. */
    #[serde(default)]
    pub online: Option<MqttOnline>,
    pub state: Vec<MqttRead>,
    #[serde(default)]
    pub commands: BTreeMap<String, MqttCommand>,
    /* Sent when the hub starts following it: "tell me your state". */
    #[serde(default)]
    pub refresh: Option<MqttMessage>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct MqttRule {
    pub access: crate::broker::Access,
    pub topic: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct MqttOnline {
    pub topic: String,
    pub online: String,
    pub offline: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct MqttRead {
    pub topic: String,
    pub read: BTreeMap<String, HttpValue>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct MqttCommand {
    pub topic: String,
    /* Text with the command's placeholders ({on}, {level}, {hex}...). */
    pub payload: String,
    #[serde(default)]
    pub bool: Option<[String; 2]>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct MqttMessage {
    pub topic: String,
    #[serde(default)]
    pub payload: String,
}

/* The adapter that runs "mqtt" blocks. */
pub const GENERIC_MQTT_ADAPTER: &str = "mqtt";

impl MqttSpec {
    fn check(&self, template: &Template) -> Result<(), String> {
        let config_names: HashSet<&str> = template
            .inputs
            .iter()
            .map(|i| i.id.as_str())
            .chain(template.adapter_config.keys().map(String::as_str))
            .collect();
        /* A topic: placeholders known, and valid once they're filled in. */
        let topic = |text: &str, what: &str| -> Result<(), String> {
            check_names(text, &config_names, &[]).map_err(|e| format!("mqtt.{what}: {e}"))?;
            let sample: String = {
                let mut out = String::new();
                let mut rest = text;
                while let Some(open) = rest.find('{') {
                    out.push_str(&rest[..open]);
                    out.push('x');
                    rest = rest[open..].split_once('}').map_or("", |(_, r)| r);
                }
                out + rest
            };
            if crate::broker::valid_topic(&sample) {
                Ok(())
            } else {
                Err(format!("mqtt.{what}: {text:?} isn't a valid MQTT topic"))
            }
        };
        check_names(&self.user, &config_names, &[]).map_err(|e| format!("mqtt.user: {e}"))?;
        if self.acl.is_empty() {
            return Err("mqtt.acl: the device's login needs at least one topic".into());
        }
        for rule in &self.acl {
            topic(&rule.topic, "acl")?;
        }
        if let Some(online) = &self.online {
            topic(&online.topic, "online")?;
            if online.online.is_empty() || online.online == online.offline {
                return Err("mqtt.online: two different payloads for online and offline".into());
            }
        }
        if self.state.is_empty() {
            return Err("mqtt.state: at least one topic carries the state".into());
        }
        let mut read = HashSet::new();
        for message in &self.state {
            topic(&message.topic, "state")?;
            if message.read.is_empty() {
                return Err(format!("mqtt.state {:?} reads nothing", message.topic));
            }
            for (target, value) in &message.read {
                read.insert(check_read(template, target, value, "mqtt")?);
            }
        }
        for capability in &template.capabilities {
            let settable = command_placeholders(capability).is_some();
            if !settable && !matches!(capability.as_str(), "sensor" | "energy") {
                return Err(format!("the mqtt adapter can't run capability {capability:?}"));
            }
            if !read.contains(capability.as_str()) {
                return Err(format!("mqtt.state never reads capability {capability:?}"));
            }
            if settable && !self.commands.contains_key(capability) {
                return Err(format!("mqtt.commands has no message for {capability:?}"));
            }
        }
        for (capability, command) in &self.commands {
            let Some(allowed) = command_placeholders(capability) else {
                return Err(format!("mqtt.commands {capability:?}: only switch, dimmer and color are set by commands"));
            };
            if !template.capabilities.contains(capability) {
                return Err(format!("mqtt.commands {capability:?}: the template has no such capability"));
            }
            topic(&command.topic, "commands")?;
            check_names(&command.payload, &config_names, allowed).map_err(|e| format!("mqtt.commands {capability:?}: {e}"))?;
            if command.bool.is_some() && capability != "switch" {
                return Err(format!("mqtt.commands {capability:?}: \"bool\" only goes with switch"));
            }
        }
        if let Some(refresh) = &self.refresh {
            topic(&refresh.topic, "refresh")?;
            check_names(&refresh.payload, &config_names, &[]).map_err(|e| format!("mqtt.refresh: {e}"))?;
        }
        Ok(())
    }
}

/* One "<capability>.<field>" a state read fills (http and mqtt blocks):
 * a capability the template has, a field the generic adapters know.
 * Returns the capability. */
fn check_read<'t>(template: &Template, target: &'t str, value: &HttpValue, block: &str) -> Result<&'t str, String> {
    let Some((capability, field)) = target.split_once('.') else {
        return Err(format!("{block}.state reads {target:?}: write <capability>.<field>"));
    };
    if !template.capabilities.iter().any(|c| c == capability) {
        return Err(format!("{block}.state reads {target:?}, but the template has no capability {capability:?}"));
    }
    let ok = match capability {
        "switch" => field == "on",
        "dimmer" => field == "level",
        "color" => field == "hex",
        "sensor" => valid_id_underscore(field),
        "energy" => ENERGY_FIELDS.contains(&field),
        _ => false,
    };
    if !ok {
        return Err(format!("{block}.state reads {target:?}: the {block} adapter doesn't know that field"));
    }
    if value.path().is_empty() {
        return Err(format!("{block}.state reads {target:?} from an empty path"));
    }
    if matches!(value, HttpValue::Full { scale: Some(s), .. } if !s.is_finite() || *s == 0.0) {
        return Err(format!("{block}.state reads {target:?}: scale must be a non-zero number"));
    }
    Ok(capability)
}

/* A request path: starts with "/", and its placeholders are config values
 * or the command's own. */
fn check_path(path: &str, config: &HashSet<&str>, command: &[&str]) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("path {path:?} must start with /"));
    }
    check_names(path, config, command)
}

fn check_body(body: &serde_json::Value, config: &HashSet<&str>, command: &[&str]) -> Result<(), String> {
    match body {
        serde_json::Value::String(text) => check_names(text, config, command),
        serde_json::Value::Array(items) => items.iter().try_for_each(|v| check_body(v, config, command)),
        serde_json::Value::Object(map) => map.values().try_for_each(|v| check_body(v, config, command)),
        _ => Ok(()),
    }
}

fn check_names(text: &str, config: &HashSet<&str>, command: &[&str]) -> Result<(), String> {
    check_placeholders(text)?;
    for name in placeholder_names(text) {
        if !config.contains(name) && !command.contains(&name) {
            return Err(format!("{text:?} uses {{{name}}}, which is neither a setting nor one of {command:?}"));
        }
    }
    Ok(())
}

/* The names of a (checked) text's {placeholders}. */
fn placeholder_names(text: &str) -> impl Iterator<Item = &str> {
    text.split('{').skip(1).filter_map(|part| part.split_once('}').map(|(name, _)| name))
}

/* One way through setup ("guided" / "advanced"). */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub id: String,
    pub label: String,
    pub steps: Vec<Step>,
}

fn default_otp_field() -> String {
    "code".into()
}

/* The step types -- each exists for a setup pattern (wiki Device-Catalog).
 * After the last step the wizard always asks for name and room; templates
 * don't repeat that. */
#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /* Text to read, then "Next". */
    Info {
        text: String,
        #[serde(default)]
        image: Option<String>,
    },
    /* A pick-list of devices found by the template's discovery. */
    Discover,
    /* Fields to fill in (input ids). */
    Form { fields: Vec<String> },
    /* Wait while the user confirms on the device (P3). */
    ConfirmOnDevice {
        action: String,
        hint: String,
        #[serde(default)]
        hints: Vec<String>,
        #[serde(default = "default_timeout")]
        timeout_s: u32,
    },
    /* A code the device shows, typed in (P3). */
    CodeFromDevice { action: String, field: String },
    /* Vendor account login (P4/P5, issue #74), in up to three screens:
     *   1. `fields` (e.g. email, password) -> the adapter's `action`;
     *   2. if `otp_action`: the one-time code the vendor sends, in input
     *      `otp_field` -> `otp_action` (skipped if `action` says the
     *      vendor needs none: plain value login_needs_code = "no");
     *   3. if `list_action`: the account's devices (cloud_list) to pick
     *      one from -- its values are the device's.
     * Afterwards the wizard forgets the account's password, the code and
     * every value named "login_*" (the session): only what the picked
     * device needs stays (wizard.rs). */
    VendorLogin {
        /* Shown: "Log in to your Roborock account". */
        vendor: String,
        fields: Vec<String>,
        action: String,
        #[serde(default)]
        otp_action: Option<String>,
        #[serde(default = "default_otp_field")]
        otp_field: String,
        #[serde(default)]
        list_action: Option<String>,
        /* Under the fields: what this login is for, where the code goes. */
        #[serde(default)]
        hint: String,
    },
    /* One choice changing what follows, e.g. two protocols. */
    Choice {
        field: String,
        then: BTreeMap<String, Vec<Step>>,
    },
    /* "Testing..." -- the adapter's probe. */
    Test,
    /* Onboarding a factory-fresh device (later, #71). */
    ProvisionSoftap { ssid_pattern: String, action: String },
    /* Implemented for the IR blaster (#42): the adapter's action sets the
     * device's WiFi over Bluetooth; `hint` is shown while it runs. */
    ProvisionBle {
        service_uuid: String,
        action: String,
        #[serde(default)]
        hint: String,
    },
    Smartconfig { flavour: String },
}

fn default_timeout() -> u32 {
    60
}

/* Why an adapter failed, so the wizard can say it in plain language. */
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Unreachable,
    Refused,
    Timeout,
    NotConfirmed,
    Unsupported,
    Vendor,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    /* The hub connects to the device (almost everything). */
    #[default]
    Hub,
    /* The device connects to the hub (later, #72). */
    DeviceMqtt,
    DeviceWebhook,
    /* The device broadcasts its state. */
    Multicast,
    /* Through the vendor's cloud (later, #74). */
    Cloud,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Power {
    #[default]
    Mains,
    /* Sleeps between reports: "last seen", never "offline" for sleeping. */
    Battery,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tls {
    #[default]
    None,
    /* Normal certificates, checked against the usual authorities. */
    Ca,
    /* Self-signed: trust the certificate seen at setup, refuse any other. */
    Tofu,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Wake {
    #[default]
    None,
    /* Wake-on-LAN to the device's MAC: devices gone when switched off. */
    Wol,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub room: String,
}

/* ------------------------------------------------------------------ */
/* Checking                                                            */
/* ------------------------------------------------------------------ */

/* What a template is checked against: the adapters and capabilities this
 * build of the daemon actually has. */
pub struct Known<'a> {
    pub adapters: &'a [&'a str],
    pub capabilities: &'a [&'a str],
}

impl Template {
    /* Every rule, in one place. Returns the first problem, phrased for
     * whoever writes templates. */
    pub fn check(&self, known: &Known) -> Result<(), String> {
        if self.format != FORMAT_VERSION {
            return Err(format!(
                "written in format {}, this hub understands format {FORMAT_VERSION}",
                self.format
            ));
        }
        if !valid_id(&self.id) {
            return Err(format!("id {:?}: use 1-40 characters a-z, 0-9 and -", self.id));
        }
        if self.version == 0 {
            return Err("version starts at 1".into());
        }
        if self.name.trim().is_empty() {
            return Err("name is empty".into());
        }
        if !CATEGORIES.contains(&self.category.as_str()) {
            return Err(format!("unknown category {:?} (one of: {})", self.category, CATEGORIES.join(", ")));
        }
        if !known.adapters.contains(&self.adapter.as_str()) {
            return Err(format!("unknown adapter {:?} (this hub has: {})", self.adapter, known.adapters.join(", ")));
        }
        if self.capabilities.is_empty() {
            return Err("no capabilities: a device must be able to do something".into());
        }
        for cap in &self.capabilities {
            if !known.capabilities.contains(&cap.as_str()) {
                return Err(format!("unknown capability {cap:?} (this hub has: {})", known.capabilities.join(", ")));
            }
        }

        // Inputs: unique ids, and what each kind needs.
        let mut input_ids = HashSet::new();
        for input in &self.inputs {
            if !valid_id_underscore(&input.id) {
                return Err(format!("input id {:?}: use a-z, 0-9 and _", input.id));
            }
            if !input_ids.insert(input.id.as_str()) {
                return Err(format!("input {:?} is declared twice", input.id));
            }
            if input.kind == InputKind::Choice && input.choices.is_empty() {
                return Err(format!("input {:?} is a choice without choices", input.id));
            }
            if input.kind != InputKind::Choice && !input.choices.is_empty() {
                return Err(format!("input {:?} has choices but isn't a choice", input.id));
            }
        }

        // Built-in devices have no setup; everything else needs one.
        if self.builtin {
            if !self.setup.is_empty() || !self.inputs.is_empty() || !self.discovery.is_empty() {
                return Err("a builtin template has no inputs, discovery or setup".into());
            }
        } else if self.setup.is_empty() {
            return Err("no setup variant: how would anyone add this device?".into());
        }

        // Setup: unique variant ids; every step refers to things that exist.
        let mut variant_ids = HashSet::new();
        let mut actions = HashSet::new();
        for variant in &self.setup {
            if !variant_ids.insert(variant.id.as_str()) {
                return Err(format!("setup variant {:?} is declared twice", variant.id));
            }
            if variant.steps.is_empty() {
                return Err(format!("setup variant {:?} has no steps", variant.id));
            }
            self.check_steps(&variant.steps, &variant.id, &input_ids, &mut actions)?;
        }

        // Discovery fills either inputs or extra values (e.g. "mac").
        for d in &self.discovery {
            for (target, source) in d.fill() {
                if !valid_id_underscore(target) {
                    return Err(format!("discovery fills {target:?}: use a-z, 0-9 and _"));
                }
                check_placeholders(source).map_err(|e| format!("discovery fill {target:?}: {e}"))?;
            }
            for key in d.matches().keys() {
                check_placeholders(&format!("{{{key}}}")).map_err(|e| format!("discovery match: {e}"))?;
            }
            match d {
                Discovery::UdpBroadcast { port, probe, .. } if *port == 0 || probe.is_empty() => {
                    return Err("udp_broadcast needs a port and a probe to send".into());
                }
                Discovery::UdpMulticast { group, port, .. } if !group.is_multicast() || *port == 0 => {
                    return Err(format!("udp_multicast: {group}:{port} isn't a multicast group and port"));
                }
                _ => {}
            }
        }

        // The generic HTTP adapter's description: exactly with that adapter.
        match (&self.http, self.adapter == GENERIC_HTTP_ADAPTER) {
            (Some(http), true) => http.check(self)?,
            (None, true) => return Err(format!("adapter {GENERIC_HTTP_ADAPTER:?} needs an \"http\" block")),
            (Some(_), false) => return Err(format!("an \"http\" block goes with adapter {GENERIC_HTTP_ADAPTER:?} only")),
            (None, false) => {}
        }
        match (&self.mqtt, self.adapter == GENERIC_MQTT_ADAPTER) {
            (Some(mqtt), true) => mqtt.check(self)?,
            (None, true) => return Err(format!("adapter {GENERIC_MQTT_ADAPTER:?} needs an \"mqtt\" block")),
            (Some(_), false) => return Err(format!("an \"mqtt\" block goes with adapter {GENERIC_MQTT_ADAPTER:?} only")),
            (None, false) => {}
        }

        check_placeholders(&self.identity).map_err(|e| format!("identity: {e}"))?;
        for action in &self.reauth {
            if !actions.contains(action.as_str()) {
                return Err(format!("reauth names action {action:?}, which no setup step uses"));
            }
        }
        if (self.power == Power::Battery) != self.report_interval_s.is_some() {
            return Err("report_interval_s goes with power \"battery\" (and only with it)".into());
        }
        if self.wake == Wake::Wol && self.identity.is_empty() && !input_ids.contains("mac") {
            return Err("wake \"wol\" needs the device's MAC: an input \"mac\" or an identity".into());
        }
        Ok(())
    }

    /* A list of steps (a variant, or a choice's branch). */
    fn check_steps<'a>(
        &'a self,
        steps: &'a [Step],
        variant: &str,
        input_ids: &HashSet<&str>,
        actions: &mut HashSet<&'a str>,
    ) -> Result<(), String> {
        let input = |id: &str| -> Result<(), String> {
            if input_ids.contains(id) {
                Ok(())
            } else {
                Err(format!("variant {variant:?}: a step uses input {id:?}, which isn't declared"))
            }
        };
        for step in steps {
            match step {
                Step::Form { fields } => {
                    if fields.is_empty() {
                        return Err(format!("variant {variant:?}: a form without fields"));
                    }
                    for f in fields {
                        input(f)?;
                    }
                }
                Step::Discover => {
                    if self.discovery.is_empty() {
                        return Err(format!("variant {variant:?}: a discover step, but no discovery methods"));
                    }
                }
                Step::ConfirmOnDevice { action, timeout_s, .. } => {
                    if !(5..=600).contains(timeout_s) {
                        return Err(format!("variant {variant:?}: timeout_s must be 5-600"));
                    }
                    actions.insert(action);
                }
                Step::CodeFromDevice { action, field } => {
                    input(field)?;
                    actions.insert(action);
                }
                Step::VendorLogin {
                    action,
                    otp_action,
                    list_action,
                    fields,
                    otp_field,
                    ..
                } => {
                    if fields.is_empty() {
                        return Err(format!("variant {variant:?}: a vendor_login without fields"));
                    }
                    for f in fields {
                        input(f)?;
                    }
                    if otp_action.is_some() {
                        input(otp_field)?;
                    }
                    actions.insert(action);
                    actions.extend(otp_action.iter().map(String::as_str));
                    actions.extend(list_action.iter().map(String::as_str));
                }
                Step::Choice { field, then } => {
                    input(field)?;
                    let choices: HashSet<&str> = self
                        .inputs
                        .iter()
                        .find(|i| i.id == *field)
                        .map(|i| i.choices.iter().map(|c| c.value.as_str()).collect())
                        .unwrap_or_default();
                    for (value, branch) in then {
                        if !choices.contains(value.as_str()) {
                            return Err(format!("variant {variant:?}: choice {field:?} has no value {value:?}"));
                        }
                        self.check_steps(branch, variant, input_ids, actions)?;
                    }
                }
                Step::ProvisionSoftap { action, .. } | Step::ProvisionBle { action, .. } => {
                    actions.insert(action);
                }
                Step::Info { .. } | Step::Test | Step::Smartconfig { .. } => {}
            }
        }
        Ok(())
    }
}

/* Template ids: 1-40 of a-z, 0-9, "-" (also used in device JSON). */
fn valid_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/* Input / value names: a-z, 0-9, "_" (they appear in placeholders). */
fn valid_id_underscore(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/* A text with {placeholders}: every { closed, each name non-empty and
 * made of a-z, 0-9, _ . - (e.g. {address}, {txt.mac},
 * {header.DLNADeviceName.lge.com} -- header names keep their case). */
fn check_placeholders(text: &str) -> Result<(), String> {
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(format!("{text:?}: a {{ without }}"));
        };
        let name = &after[..close];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
            return Err(format!("{text:?}: bad placeholder {{{name}}}"));
        }
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err(format!("{text:?}: a }} without {{"));
    }
    Ok(())
}

/* ------------------------------------------------------------------ */
/* Loading                                                             */
/* ------------------------------------------------------------------ */

/* All usable templates, by id. */
#[derive(Default)]
pub struct Templates {
    by_id: BTreeMap<String, Template>,
}

impl Templates {
    /* Loads every .json file in `dir`. A file that doesn't parse or fails a
     * check is skipped with a message (returned, and logged by the
     * caller); two files with the same id: the second is skipped. */
    pub fn load(dir: &Path, known: &Known) -> (Templates, Vec<String>) {
        let mut templates = Templates::default();
        let mut problems = Vec::new();
        let mut files: Vec<_> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect(),
            Err(e) => {
                problems.push(format!("{}: {e}", dir.display()));
                return (templates, problems);
            }
        };
        // Sorted, so which duplicate wins doesn't depend on the filesystem.
        files.sort();
        for path in files {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let result = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| serde_json::from_str::<Template>(&text).map_err(|e| e.to_string()))
                .and_then(|t| t.check(known).map(|()| t));
            match result {
                Ok(t) if templates.by_id.contains_key(&t.id) => {
                    problems.push(format!("{name}: id {:?} is already used by another template, skipped", t.id));
                }
                Ok(t) => {
                    templates.by_id.insert(t.id.clone(), t);
                }
                Err(e) => problems.push(format!("{name}: {e}")),
            }
        }
        (templates, problems)
    }

    pub fn get(&self, id: &str) -> Option<&Template> {
        self.by_id.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &Template> {
        self.by_id.values()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /* Tests: these templates (unchecked -- the caller did that). */
    #[cfg(test)]
    pub fn from_list(templates: Vec<Template>) -> Templates {
        Templates {
            by_id: templates.into_iter().map(|t| (t.id.clone(), t)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* The hub's own list: a template using a new capability is checked
     * against what device.rs really has. */
    const ALL_CAPS: [&str; crate::device::CAPABILITY_NAMES.len()] = crate::device::CAPABILITY_NAMES;
    const ADAPTERS: [&str; 11] = ["m4-led", "wled", "lg-webos", "ir-blaster", "wiz", "http", "mqtt", "roborock", "ezviz", "tapo", "camera"];

    fn known() -> Known<'static> {
        Known {
            adapters: &ADAPTERS,
            capabilities: &ALL_CAPS,
        }
    }

    fn parse(json: &str) -> Template {
        serde_json::from_str(json).unwrap()
    }

    /* The smallest valid template, to vary in the tests below. */
    const MINIMAL: &str = r#"{
        "format": 1, "id": "t", "version": 1, "name": "T", "category": "other",
        "adapter": "wled", "capabilities": ["switch"],
        "inputs": [ { "id": "host", "type": "text", "label": "Address" } ],
        "setup": [ { "id": "v", "label": "V", "steps": [ { "type": "form", "fields": ["host"] }, { "type": "test" } ] } ]
    }"#;

    fn with(change: impl FnOnce(&mut serde_json::Value)) -> Result<(), String> {
        let mut v: serde_json::Value = serde_json::from_str(MINIMAL).unwrap();
        change(&mut v);
        serde_json::from_value::<Template>(v).map_err(|e| e.to_string())?.check(&known())
    }

    #[test]
    fn the_repo_templates_all_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let (templates, problems) = Templates::load(&dir, &known());
        assert!(problems.is_empty(), "{problems:?}");
        for id in ["m4-led", "wled", "lg-webos-tv", "ir-blaster", "wiz", "shelly-plug-gen3"] {
            assert!(templates.get(id).is_some(), "{id} missing");
        }
        let tv = templates.get("lg-webos-tv").unwrap();
        assert_eq!(tv.tls, Tls::Tofu);
        assert_eq!(tv.wake, Wake::Wol);
        assert_eq!(tv.reauth, ["pair"]);
    }

    /* Issue #75: the generic HTTP adapter's block. */
    #[test]
    fn http_blocks_are_checked() {
        let http = |change: fn(&mut serde_json::Value)| {
            with(|v| {
                v["adapter"] = "http".into();
                v["capabilities"] = serde_json::json!(["switch", "sensor"]);
                v["http"] = serde_json::json!({
                    "state": [ { "path": "/status", "read": { "switch.on": "relay.on", "sensor.power": { "path": "w", "unit": "W" } } } ],
                    "commands": { "switch": { "path": "/relay?on={on}&key={host}" } }
                });
                change(v);
            })
        };
        assert_eq!(http(|_| {}), Ok(()));
        let err = |change| http(change).unwrap_err();
        assert!(err(|v| v["http"] = serde_json::Value::Null).contains("needs an \"http\" block"));
        assert!(err(|v| v["http"]["poll_s"] = 1.into()).contains("poll_s"));
        assert!(err(|v| v["http"]["commands"] = serde_json::json!({})).contains("no request for \"switch\""));
        assert!(err(|v| v["http"]["commands"]["switch"]["path"] = "/x?on={level}".into()).contains("{level}"));
        assert!(err(|v| v["http"]["commands"]["switch"]["path"] = "relay".into()).contains("start with /"));
        assert!(err(|v| v["http"]["state"][0]["read"]["dimmer.level"] = "bri".into()).contains("no capability \"dimmer\""));
        assert!(err(|v| v["http"]["state"][0]["read"]["switch.state"] = "x".into()).contains("doesn't know that field"));
        assert!(err(|v| v["capabilities"] = serde_json::json!(["switch", "sensor", "media"])).contains("can't run"));
        assert!(err(|v| v["http"]["commands"]["switch"]["typo"] = 1.into()).contains("unknown field"));
        /* A body's placeholders are checked too. */
        assert!(err(|v| v["http"]["commands"]["switch"]["body"] = serde_json::json!({"a": ["{nope}"]})).contains("{nope}"));
        /* ...and an http block with another adapter is a mistake. */
        let other = with(|v| {
            v["http"] = serde_json::json!({ "state": [] });
        });
        assert!(other.unwrap_err().contains("goes with adapter"));
    }

    #[test]
    fn udp_discovery_is_checked() {
        let udp = |d: serde_json::Value| with(|v| v["discovery"] = serde_json::json!([d]));
        assert_eq!(udp(serde_json::json!({"method": "udp_broadcast", "port": 38899, "probe": "x", "match": {"json.method": "x"}})), Ok(()));
        assert!(udp(serde_json::json!({"method": "udp_broadcast", "port": 38899, "probe": ""})).is_err());
        assert_eq!(udp(serde_json::json!({"method": "udp_multicast", "group": "239.255.255.250", "port": 1982})), Ok(()));
        assert!(udp(serde_json::json!({"method": "udp_multicast", "group": "192.168.1.1", "port": 1982})).is_err());
        assert!(udp(serde_json::json!({"method": "mdns", "service": "_x._tcp", "match": {"bad name": "x"}})).is_err());
    }

    #[test]
    fn minimal_is_valid() {
        assert_eq!(with(|_| {}), Ok(()));
    }

    #[test]
    fn typos_and_unknown_names_are_refused() {
        assert!(with(|v| v["discovry"] = serde_json::json!([])).unwrap_err().contains("unknown field"));
        assert!(with(|v| v["capabilities"] = serde_json::json!(["swich"])).unwrap_err().contains("unknown capability"));
        assert!(with(|v| v["adapter"] = serde_json::json!("hue")).unwrap_err().contains("unknown adapter"));
        assert!(with(|v| v["category"] = serde_json::json!("toys")).unwrap_err().contains("unknown category"));
        assert!(with(|v| v["format"] = serde_json::json!(2)).unwrap_err().contains("format"));
        assert!(with(|v| v["id"] = serde_json::json!("My Lamp")).unwrap_err().contains("id"));
    }

    #[test]
    fn steps_must_refer_to_declared_things() {
        let err = with(|v| v["setup"][0]["steps"][0]["fields"] = serde_json::json!(["password"])).unwrap_err();
        assert!(err.contains("\"password\""), "{err}");
        let err = with(|v| v["setup"][0]["steps"][0] = serde_json::json!({"type": "discover"})).unwrap_err();
        assert!(err.contains("no discovery"), "{err}");
        let err = with(|v| v["reauth"] = serde_json::json!(["pair"])).unwrap_err();
        assert!(err.contains("reauth"), "{err}");
        let err = with(|v| v["setup"] = serde_json::json!([])).unwrap_err();
        assert!(err.contains("no setup"), "{err}");
    }

    #[test]
    fn inputs_are_consistent() {
        let err = with(|v| {
            let first = v["inputs"][0].clone();
            v["inputs"].as_array_mut().unwrap().push(first)
        }).unwrap_err();
        assert!(err.contains("twice"), "{err}");
        let err = with(|v| v["inputs"][0]["type"] = serde_json::json!("choice")).unwrap_err();
        assert!(err.contains("without choices"), "{err}");
        let err = with(|v| v["inputs"][0]["validate"] = serde_json::json!({"pattern": "regex"})).unwrap_err();
        assert!(err.contains("unknown variant"), "{err}");
    }

    #[test]
    fn network_behaviour_fields_go_together() {
        let err = with(|v| v["power"] = serde_json::json!("battery")).unwrap_err();
        assert!(err.contains("report_interval_s"), "{err}");
        assert_eq!(
            with(|v| {
                v["power"] = serde_json::json!("battery");
                v["report_interval_s"] = serde_json::json!(300);
            }),
            Ok(())
        );
        let err = with(|v| v["wake"] = serde_json::json!("wol")).unwrap_err();
        assert!(err.contains("MAC"), "{err}");
        assert!(with(|v| v["identity"] = serde_json::json!("{mac")).is_err());
    }

    #[test]
    fn choice_branches_are_checked() {
        let good = with(|v| {
            v["inputs"].as_array_mut().unwrap().push(serde_json::json!({"id": "protocol", "type": "choice", "label": "App",
                "choices": [{"value": "miio", "label": "Xiaomi"}, {"value": "tcp", "label": "Roborock"}]}));
            v["setup"][0]["steps"][0] = serde_json::json!({"type": "choice", "field": "protocol",
                "then": {"miio": [{"type": "form", "fields": ["host"]}], "tcp": [{"type": "test"}]}});
        });
        assert_eq!(good, Ok(()));
        let bad = with(|v| {
            v["inputs"].as_array_mut().unwrap().push(serde_json::json!({"id": "protocol", "type": "choice", "label": "App",
                "choices": [{"value": "miio", "label": "Xiaomi"}]}));
            v["setup"][0]["steps"][0] = serde_json::json!({"type": "choice", "field": "protocol",
                "then": {"zigbee": [{"type": "test"}]}});
        });
        assert!(bad.unwrap_err().contains("zigbee"));
    }

    #[test]
    fn builtin_templates_have_no_setup() {
        assert!(parse(r#"{"format":1,"id":"b","version":1,"name":"B","category":"lighting","adapter":"m4-led","capabilities":["switch"],"builtin":true}"#)
            .check(&known())
            .is_ok());
        let err = with(|v| v["builtin"] = serde_json::json!(true)).unwrap_err();
        assert!(err.contains("builtin"), "{err}");
    }

    #[test]
    fn a_broken_file_does_not_stop_the_others() {
        let dir = std::env::temp_dir().join(format!("uc-templates-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), MINIMAL).unwrap();
        std::fs::write(dir.join("b.json"), "{ not json").unwrap();
        std::fs::write(dir.join("c.json"), MINIMAL).unwrap(); // same id "t"
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let (templates, problems) = Templates::load(&dir, &known());
        assert_eq!(templates.len(), 1);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].starts_with("b.json"));
        assert!(problems[1].contains("already used"));
    }
}
