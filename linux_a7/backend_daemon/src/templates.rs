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
    #[serde(default)]
    pub defaults: Defaults,
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
    },
    Ssdp {
        search: String,
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
    UdpBroadcast {
        port: u16,
        /* What to send, as text (the adapter's protocol decides). */
        probe: String,
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
    WsDiscovery {
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
    /* (later, #73) */
    NetworkScan {
        manufacturers: Vec<String>,
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
    /* (later, #73) */
    PortProbe {
        port: u16,
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
    /* (later, #72): the device connects to the hub by itself. */
    DeviceAnnounce {
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
    /* (later, #74): the devices of a vendor account. */
    CloudList {
        #[serde(default)]
        fill: BTreeMap<String, String>,
    },
}

impl Discovery {
    pub fn fill(&self) -> &BTreeMap<String, String> {
        match self {
            Discovery::Mdns { fill, .. }
            | Discovery::Ssdp { fill, .. }
            | Discovery::UdpBroadcast { fill, .. }
            | Discovery::WsDiscovery { fill }
            | Discovery::NetworkScan { fill, .. }
            | Discovery::PortProbe { fill, .. }
            | Discovery::DeviceAnnounce { fill }
            | Discovery::CloudList { fill } => fill,
        }
    }
}

/* One way through setup ("guided" / "advanced"). */
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub id: String,
    pub label: String,
    pub steps: Vec<Step>,
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
    /* Vendor account login (P4/P5, later #74). */
    VendorLogin {
        action: String,
        #[serde(default)]
        otp_action: Option<String>,
        #[serde(default)]
        list_action: Option<String>,
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
    ProvisionBle { service_uuid: String, action: String },
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
                Step::VendorLogin { action, otp_action, list_action } => {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /* The hub's own list: a template using a new capability is checked
     * against what device.rs really has. */
    const ALL_CAPS: [&str; crate::device::CAPABILITY_NAMES.len()] = crate::device::CAPABILITY_NAMES;
    const ADAPTERS: [&str; 3] = ["m4-led", "wled", "lg-webos"];

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
        for id in ["m4-led", "wled", "lg-webos-tv"] {
            assert!(templates.get(id).is_some(), "{id} missing");
        }
        let tv = templates.get("lg-webos-tv").unwrap();
        assert_eq!(tv.tls, Tls::Tofu);
        assert_eq!(tv.wake, Wake::Wol);
        assert_eq!(tv.reauth, ["pair"]);
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
