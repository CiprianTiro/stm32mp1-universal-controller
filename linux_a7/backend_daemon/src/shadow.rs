/* shadow.rs -- AWS IoT Device Shadow topics and documents (issue #31).
 *
 * Pure logic, no networking: topic names, recognising incoming topics,
 * building the JSON the board publishes, and working out which devices
 * changed since the last publish. mqtt.rs does the actual sending and
 * receiving; keeping this part separate means all of it can be unit-tested
 * without a broker.
 *
 * Layout in AWS (one Thing per hub):
 *
 *   Thing "dk2-01"
 *   ├── classic shadow        $aws/things/dk2-01/shadow/...
 *   │     the hub itself:     {"state":{"reported":{"system":{...}}}}
 *   ├── named shadow "ld7"    $aws/things/dk2-01/shadow/name/ld7/...
 *   │     one device:         {"state":{"reported":{"on":true}}}
 *   └── named shadow "lamp-1" ...
 *
 * Why one named shadow per device (instead of all devices in one map, as
 * in #26): AWS then knows each device as a separate item -- the console
 * lists them under Things -> dk2-01 -> Device Shadows, each one can be
 * deleted there, commands are per device, and a change to one lamp only
 * sends that lamp's few bytes instead of the whole hub's state.
 */

use serde_json::{json, Value};
use std::collections::HashMap;

use crate::state::{DeviceId, DeviceState};

/* All topic names for one Thing. Built once from the thing name. */
pub struct Topics {
    classic: String,
    named: String,
}

impl Topics {
    pub fn new(thing: &str) -> Self {
        Topics {
            classic: format!("$aws/things/{thing}/shadow"),
            named: format!("$aws/things/{thing}/shadow/name/"),
        }
    }

    /* The hub's own report (the `system` section) goes here. */
    pub fn classic_update(&self) -> String {
        format!("{}/update", self.classic)
    }

    /* Publishing a device's reported state here also CREATES its named
     * shadow if it doesn't exist yet -- there is no separate "create". */
    pub fn device_update(&self, id: &str) -> String {
        format!("{}{id}/update", self.named)
    }

    /* An empty message here asks AWS for the whole document; the answer
     * arrives on .../get/accepted (used to catch commands sent while the
     * board was offline). */
    pub fn device_get(&self, id: &str) -> String {
        format!("{}{id}/get", self.named)
    }

    /* An empty message here deletes the device's named shadow. */
    pub fn device_delete(&self, id: &str) -> String {
        format!("{}{id}/delete", self.named)
    }

    /* What the board listens to, for ALL its devices at once: `+` is MQTT's
     * single-level wildcard, so ".../name/+/update/delta" matches the delta
     * topic of every device. */
    pub fn subscriptions(&self) -> [String; 3] {
        [
            format!("{}+/update/delta", self.named),
            format!("{}+/get/accepted", self.named),
            format!("{}+/delete/accepted", self.named),
        ]
    }

    /* Recognises an incoming topic: which device, and what happened.
     * None for anything else (e.g. the classic shadow's own topics). */
    pub fn parse(&self, topic: &str) -> Option<(DeviceId, Event)> {
        let rest = topic.strip_prefix(&self.named)?;
        let (id, suffix) = rest.split_once('/')?;
        let event = match suffix {
            "update/delta" => Event::Delta,
            "get/accepted" => Event::GetAccepted,
            "delete/accepted" => Event::DeleteAccepted,
            _ => return None,
        };
        if !valid_name(id) {
            return None;
        }
        Some((id.to_string(), event))
    }
}

/* What an incoming message on a device's shadow means. */
#[derive(Debug, PartialEq)]
pub enum Event {
    /* The cloud wants something changed: payload has the differences. */
    Delta,
    /* Answer to our `get`: the whole document, maybe with a pending delta. */
    GetAccepted,
    /* Someone (the console, or we ourselves) deleted this device's shadow. */
    DeleteAccepted,
}

/* Whether `id` can be used as a named shadow's name. AWS allows letters,
 * digits, `-`, `_` and `:`, up to 64 characters. Anything else would make
 * the topic invalid -- `/` would even change which topic it is, and `+`/`#`
 * are MQTT wildcards -- so such ids are refused before they reach MQTT. */
pub fn valid_name(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
}

/* The payload of a delta message: {"state": {"on": false}, "version": ...}.
 * Returns just the properties to change. */
pub fn parse_delta(payload: &[u8]) -> Result<DeviceState, String> {
    #[derive(serde::Deserialize)]
    struct Delta {
        state: DeviceState,
    }
    serde_json::from_slice::<Delta>(payload)
        .map(|d| d.state)
        .map_err(|e| e.to_string())
}

/* The payload of get/accepted: the whole document. Only its "delta" part
 * (desired-but-not-yet-reported, i.e. a command that arrived while the
 * board was offline) matters. Ok(None) = nothing pending. */
pub fn parse_get_accepted(payload: &[u8]) -> Result<Option<DeviceState>, String> {
    #[derive(serde::Deserialize)]
    struct Doc {
        state: DocState,
    }
    #[derive(serde::Deserialize)]
    struct DocState {
        #[serde(default)]
        delta: Option<DeviceState>,
    }
    serde_json::from_slice::<Doc>(payload)
        .map(|d| d.state.delta.filter(|delta| !delta.is_empty()))
        .map_err(|e| e.to_string())
}

/* A device's report. With `clear_desired`, the same message also deletes
 * the shadow's whole "desired" section: in a shadow update, null means
 * "delete". Why this matters (found on real AWS in #26): AWS keeps
 * "desired" until someone removes it and re-sends a delta whenever reported
 * differs from it, so an old cloud command would override every later
 * local change. The board clears it right after carrying a command out. */
pub fn device_report(properties: &DeviceState, clear_desired: bool) -> Vec<u8> {
    let mut doc = json!({ "state": { "reported": properties } });
    if clear_desired {
        doc["state"]["desired"] = Value::Null;
    }
    doc.to_string().into_bytes()
}

/* The hub's own report for the classic shadow. `migrate` additionally
 * deletes the `devices` map that #26/#29 kept there (in both reported and
 * desired), so no stale copy of the devices is left next to the new named
 * shadows. Sent once per connection; deleting what isn't there is a no-op. */
pub fn classic_report(system: &Value, migrate: bool) -> Vec<u8> {
    let mut doc = json!({ "state": { "reported": { "system": system } } });
    if migrate {
        doc["state"]["reported"]["devices"] = Value::Null;
        doc["state"]["desired"] = json!({ "devices": null });
    }
    doc.to_string().into_bytes()
}

/* What has to be published to bring the cloud from `published` (what we
 * last sent) to `current` (what the hub has now). */
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    /* new devices, or devices whose properties differ: publish a report */
    pub updated: Vec<DeviceId>,
    /* devices that no longer exist: delete their shadow */
    pub removed: Vec<DeviceId>,
}

pub fn changes(
    published: &HashMap<DeviceId, DeviceState>,
    current: &HashMap<DeviceId, DeviceState>,
) -> Changes {
    let mut c = Changes::default();
    for (id, properties) in current {
        if published.get(id) != Some(properties) {
            c.updated.push(id.clone());
        }
    }
    for id in published.keys() {
        if !current.contains_key(id) {
            c.removed.push(id.clone());
        }
    }
    /* HashMap order is random; sorting makes behaviour (and tests)
     * repeatable. */
    c.updated.sort();
    c.removed.sort();
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(on: bool) -> DeviceState {
        HashMap::from([("on".to_string(), json!(on))])
    }

    #[test]
    fn topics_have_aws_names() {
        let t = Topics::new("dk2-01");
        assert_eq!(t.classic_update(), "$aws/things/dk2-01/shadow/update");
        assert_eq!(t.device_update("ld7"), "$aws/things/dk2-01/shadow/name/ld7/update");
        assert_eq!(t.device_delete("lamp-1"), "$aws/things/dk2-01/shadow/name/lamp-1/delete");
        assert_eq!(t.subscriptions()[0], "$aws/things/dk2-01/shadow/name/+/update/delta");
    }

    #[test]
    fn parse_recognises_device_events() {
        let t = Topics::new("dk2-01");
        assert_eq!(
            t.parse("$aws/things/dk2-01/shadow/name/lamp-1/update/delta"),
            Some(("lamp-1".to_string(), Event::Delta))
        );
        assert_eq!(
            t.parse("$aws/things/dk2-01/shadow/name/ld7/delete/accepted"),
            Some(("ld7".to_string(), Event::DeleteAccepted))
        );
        assert_eq!(
            t.parse("$aws/things/dk2-01/shadow/name/ld7/get/accepted"),
            Some(("ld7".to_string(), Event::GetAccepted))
        );
    }

    #[test]
    fn parse_ignores_other_topics() {
        let t = Topics::new("dk2-01");
        /* classic shadow, another thing, an event we don't handle */
        assert_eq!(t.parse("$aws/things/dk2-01/shadow/update/delta"), None);
        assert_eq!(t.parse("$aws/things/dk2-02/shadow/name/ld7/update/delta"), None);
        assert_eq!(t.parse("$aws/things/dk2-01/shadow/name/ld7/update/accepted"), None);
    }

    #[test]
    fn valid_names_follow_aws_rules() {
        assert!(valid_name("lamp-1"));
        assert!(valid_name("zone:kitchen_2"));
        assert!(!valid_name(""));
        assert!(!valid_name("living room"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("lamp+"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[test]
    fn delta_and_get_accepted_payloads() {
        let delta = parse_delta(br#"{"version":3,"state":{"on":false}}"#).unwrap();
        assert_eq!(delta, props(false));

        let pending = br#"{"state":{"desired":{"on":true},"reported":{"on":false},"delta":{"on":true}}}"#;
        assert_eq!(parse_get_accepted(pending).unwrap(), Some(props(true)));

        let in_sync = br#"{"state":{"desired":{"on":true},"reported":{"on":true}}}"#;
        assert_eq!(parse_get_accepted(in_sync).unwrap(), None);

        assert!(parse_delta(b"not json").is_err());
    }

    #[test]
    fn device_report_optionally_clears_desired() {
        let plain: Value = serde_json::from_slice(&device_report(&props(true), false)).unwrap();
        assert_eq!(plain, json!({"state":{"reported":{"on":true}}}));
        let cleared: Value = serde_json::from_slice(&device_report(&props(true), true)).unwrap();
        assert_eq!(cleared, json!({"state":{"reported":{"on":true},"desired":null}}));
    }

    #[test]
    fn classic_report_migration_removes_devices_map() {
        let sys = json!({"uptime_s": 5});
        let doc: Value = serde_json::from_slice(&classic_report(&sys, true)).unwrap();
        assert_eq!(
            doc,
            json!({"state":{"reported":{"system":{"uptime_s":5},"devices":null},"desired":{"devices":null}}})
        );
        let plain: Value = serde_json::from_slice(&classic_report(&sys, false)).unwrap();
        assert_eq!(plain, json!({"state":{"reported":{"system":{"uptime_s":5}}}}));
    }

    #[test]
    fn changes_finds_new_changed_and_removed_devices() {
        let published = HashMap::from([
            ("ld7".to_string(), props(false)),
            ("lamp-1".to_string(), props(true)),
            ("old".to_string(), props(true)),
        ]);
        let current = HashMap::from([
            ("ld7".to_string(), props(true)),    /* changed */
            ("lamp-1".to_string(), props(true)), /* unchanged */
            ("new".to_string(), props(false)),   /* new */
        ]);
        assert_eq!(
            changes(&published, &current),
            Changes {
                updated: vec!["ld7".to_string(), "new".to_string()],
                removed: vec!["old".to_string()],
            }
        );
    }
}
