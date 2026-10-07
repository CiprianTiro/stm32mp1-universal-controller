/*
 * wizard.rs -- adding a device (issue #40): the setup wizard, run by the
 * BACKEND.
 *
 * WHY HERE AND NOT IN THE SCREENS. The template (templates.rs) says which
 * steps a device type needs; this file walks through them, checks every
 * answer, talks to the adapter (probe, pairing actions) and finally
 * creates the device. The touchscreen and the phone app only DRAW the
 * current step and send back what the person entered -- so the logic
 * exists once, both get exactly the same checks and error messages, and
 * the phone app (#48) needs no code per device type.
 *
 * A SESSION is one person adding one device. ws.rs keeps at most one per
 * connection (a second wizard_start replaces it) and drops it when the
 * connection closes or after IDLE_LIMIT without an answer.
 *
 *   start(template, variant?, found?)
 *     -> the first step
 *   answer(values)
 *     -> the next step, or an error for this one (it stays)
 *   back()
 *     -> the previous step (answers are kept, pre-filled)
 *   finish(name, room)                    once every step is done
 *     -> the new device, its adapter started
 *
 * The same session also works on a device that EXISTS (issue #40, same
 * id, name, room and cloud shadow afterwards):
 *   start_for(device, Reauth)       "Pair again": only the template's
 *                                   `reauth` steps (the TV's prompt), then
 *                                   the test -- for a device shown
 *                                   "unauthorized"
 *   start_for(device, Reconfigure)  "Change settings": the template's form
 *                                   fields, filled in with the current
 *                                   values (secrets: empty = keep), then
 *                                   the test
 * Their last step is "save" instead of "name"; finish() then updates the
 * device (config + secrets) and restarts its adapter.
 *
 * THE STEPS the template lists (templates::Step), as a client sees them:
 *   info              text; answer {} to go on
 *   discover          devices found on the network for this template;
 *                     answer {"found": "<address>"}, or {} to search again
 *   form              fields; answer {"<field>": value, ...}
 *   choice            one field; the answer picks which steps follow
 *   confirm_on_device a hint while the adapter waits for the person to
 *                     confirm on the device; answer {} to START waiting
 *   code_from_device  one field for the code the device shows
 *   test              answer {} to run the adapter's probe
 *   name              (after the last step) -> finish(name, room)
 * A client answers confirm_on_device and test right away and shows the
 * step ("Testing...") until the reply comes. The reply only comes when the
 * adapter is done: up to the step's timeout.
 *
 * "Found on your network" (discovery.rs's inbox) starts a wizard with
 * `found`: the device is already chosen, and the discover step is skipped.
 *
 *   provision_ble     (#42) a hint while the adapter sets the device's
 *                     WiFi over Bluetooth; answer {} to start, like
 *                     confirm_on_device. Inputs named wifi_* (the WiFi
 *                     name and password it sends) are forgotten right after
 *                     it: the device keeps them, the hub has no use for them.
 *
 *   vendor_login      (#74) a vendor account, in up to three screens of
 *                     the same step ("phase"):
 *                       account  the step's fields (email, password);
 *                       code     the one-time code the vendor sent
 *                                (skipped when the action answers
 *                                login_needs_code = "no");
 *                       pick     the account's devices: answer
 *                                {"device": "<id>"}
 *                     {"restart": true} goes back to "account" (another
 *                     email, a new code). A successful login SAVES the
 *                     account (accounts.rs: its session, never the
 *                     password); the account screen then also offers it --
 *                     {"account": "<id>"} -- straight to its devices.
 *                     A device already chosen on the network (same
 *                     identity), or the device being paired again, is
 *                     picked by itself. Then the password, the code and
 *                     every "login_*" value (the vendor session) are
 *                     forgotten: the hub keeps no account password.
 *
 * Not yet supported: the WiFi onboarding steps provision_softap /
 * smartconfig (#71). Templates using them aren't offered (list) and can't
 * be started.
 */
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::adapters::{Probe, SetupError, SetupValues};
use crate::control::Control;
use crate::device::{self, Device, Source};
use crate::discovery::{self, Discovery, Found};
use crate::secrets::Secret;
use crate::templates::{ErrorKind, Input, InputKind, Pattern, Step, Template, Templates};

/* A session nobody answered for this long is forgotten (the person
 * walked away): a new wizard_start begins again. */
pub const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);

/* Longest value accepted in a text field (a device's config values have
 * the same limit, device.rs). */
const MAX_TEXT: usize = 256;

/* What the wizard needs from the rest of the hub. */
pub struct Context<'a> {
    pub templates: &'a Templates,
    pub control: &'a Control,
    pub discovery: &'a Discovery,
    /* The WiFi the hub itself is on, if any: pre-filled as the network a
     * device set up over Bluetooth should join (input "wifi_ssid"). */
    pub hub_wifi: Option<String>,
}

/* How long a provision_ble step may take: finding the device, the
 * Bluetooth session, the device joining the WiFi, pairing over it. */
const PROVISION_LIMIT: Duration = Duration::from_secs(150);

/* ------------------------------------------------------------------ */
/* What clients see                                                    */
/* ------------------------------------------------------------------ */

/* One entry of the "Add device" menu. */
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct TemplateInfo {
    pub id: String,
    pub name: String,
    pub category: String,
    pub description: String,
    /* The ways in ("Find it automatically", "Enter the address"). */
    pub variants: Vec<VariantInfo>,
    /* Whether "pair again" / "change settings" exist for its devices
     * (Session::start_for would refuse otherwise): a screen only offers
     * what works. */
    pub can_reauth: bool,
    pub can_reconfigure: bool,
    /* Issue #74: works only through the vendor's cloud (needs internet). */
    pub cloud: bool,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct VariantInfo {
    pub id: String,
    pub label: String,
}

/* The step to show now. */
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct StepView {
    pub session: String,
    pub template: String,
    /* Which way in ("guided", "advanced"); "reauth" / "reconfigure" for an
     * existing device. A screen can offer the template's other variants
     * (e.g. "Enter the address instead" on the discover step). */
    pub variant: String,
    /* 1, 2, 3... (a choice can add steps, so no "of N"). */
    pub number: usize,
    #[serde(flatten)]
    pub step: StepKind,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum StepKind {
    Info {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        image: Option<String>,
    },
    Discover {
        found: Vec<FoundView>,
    },
    Form {
        fields: Vec<Field>,
    },
    Choice {
        field: Field,
    },
    ConfirmOnDevice {
        hint: String,
        hints: Vec<String>,
        timeout_s: u32,
    },
    ProvisionBle {
        hint: String,
        timeout_s: u32,
    },
    CodeFromDevice {
        field: Field,
    },
    /* Issue #74 (see the header). `devices` only in phase "pick";
     * `accounts`: the saved accounts of this vendor, phase "account"
     * (answer {"account": "<id>"} to use one). */
    VendorLogin {
        vendor: String,
        phase: LoginPhase,
        hint: String,
        fields: Vec<Field>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        accounts: Vec<AccountView>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        devices: Vec<CloudDeviceView>,
    },
    Test,
    /* After the last step: name and room, pre-filled. `summary`: what the
     * test step found ("WLED 16.0.1"), empty without one. */
    Name {
        name: String,
        room: String,
        summary: String,
    },
    /* After the last step of "pair again" / "change settings": confirm
     * with finish (name and room aren't asked: they stay). */
    Save {
        device: String,
        summary: String,
    },
}

/* Where a vendor_login step is (see the header). */
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LoginPhase {
    #[default]
    Account,
    Code,
    Pick,
}

/* A saved vendor account, offered on the login screen. */
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct AccountView {
    pub id: String,
    pub label: String,
}

/* One device of the account, as a pick list shows it. */
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CloudDeviceView {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub available: bool,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct FoundView {
    pub address: String,
    pub name: String,
}

/* An input as a client draws it. */
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Field {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: InputKind,
    pub label: String,
    pub hint: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<ChoiceView>,
    /* What's there already (an earlier answer, a found device, the
     * template's default). NEVER for secrets: those don't come back. */
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ChoiceView {
    pub value: String,
    pub label: String,
}

/* A refused answer: the sentence to show, which field it's about (if
 * one), and the technical detail (for a "more" line and the log). */
#[derive(Debug, Clone, PartialEq)]
pub struct WizardError {
    pub field: Option<String>,
    pub message: String,
    pub detail: String,
}

impl WizardError {
    fn plain(message: impl Into<String>) -> Self {
        WizardError {
            field: None,
            message: message.into(),
            detail: String::new(),
        }
    }

    fn field(field: &str, message: impl Into<String>) -> Self {
        WizardError {
            field: Some(field.to_string()),
            message: message.into(),
            detail: String::new(),
        }
    }
}

/* ------------------------------------------------------------------ */
/* The menu                                                            */
/* ------------------------------------------------------------------ */

/* The templates the wizard can add: not built in, and only using steps
 * this hub supports. Sorted by category, then name. */
pub fn list(templates: &Templates) -> Vec<TemplateInfo> {
    let mut list: Vec<TemplateInfo> = templates
        .all()
        .filter(|t| supported(t).is_ok())
        .map(|t| TemplateInfo {
            id: t.id.clone(),
            name: t.name.clone(),
            category: t.category.clone(),
            description: t.description.clone(),
            variants: t
                .setup
                .iter()
                .map(|v| VariantInfo {
                    id: v.id.clone(),
                    label: v.label.clone(),
                })
                .collect(),
            can_reauth: !action_steps(t, &t.reauth).is_empty(),
            can_reconfigure: !form_fields(t).is_empty(),
            cloud: t.cloud,
        })
        .collect();
    list.sort_by(|a, b| (&a.category, &a.name).cmp(&(&b.category, &b.name)));
    list
}

fn supported(template: &Template) -> Result<(), String> {
    if template.builtin {
        return Err(format!("{} is part of the hub: nothing to add", template.name));
    }
    fn check(steps: &[Step]) -> Result<(), &'static str> {
        for step in steps {
            match step {
                Step::ProvisionSoftap { .. } | Step::Smartconfig { .. } => return Err("setting up the device's WiFi"),
                Step::Choice { then, .. } => {
                    for branch in then.values() {
                        check(branch)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    for variant in &template.setup {
        check(&variant.steps)
            .map_err(|what| format!("{} needs {what}, which this hub can't do yet", template.name))?;
    }
    Ok(())
}

/* ------------------------------------------------------------------ */
/* A session                                                           */
/* ------------------------------------------------------------------ */

/* What a session is for (see the header). */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reauth,
    Reconfigure,
}

pub struct Session {
    id: String,
    template: Template,
    /* See StepView::variant. */
    variant: String,
    /* None: adding a new device. Some: working on this existing one. */
    existing: Option<(Device, Mode)>,
    /* The variant's steps; a choice inserts its branch after itself. */
    steps: Vec<Step>,
    /* The step shown now; steps.len() = all done (the name step). */
    pos: usize,
    /* Where "back" returns to: the earlier shown steps, and the step list
     * as it was then (a choice's branch disappears again). */
    history: Vec<(usize, Vec<Step>)>,
    values: SetupValues,
    /* The device picked from the network, if any. */
    found: Option<Found>,
    probe: Option<Probe>,
    /* Issue #74: the current vendor_login step's screen, and the name the
     * picked device has in the vendor's account (offered as its name). */
    login: LoginPhase,
    account_name: Option<String>,
    last_active: Instant,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn expired(&self) -> bool {
        self.last_active.elapsed() > IDLE_LIMIT
    }

    /* Starts adding a device of `template_id`. `variant`: which way in
     * (default: the first -- or, with `found`, the first that discovers).
     * `found`: the address of a device from the inbox. */
    pub async fn start(
        ctx: &Context<'_>,
        template_id: &str,
        variant: Option<&str>,
        found: Option<&str>,
    ) -> Result<(Session, StepView), WizardError> {
        let template = ctx
            .templates
            .get(template_id)
            .ok_or_else(|| WizardError::plain(format!("unknown device type {template_id:?}")))?;
        supported(template).map_err(WizardError::plain)?;

        let found = match found {
            Some(address) => Some(
                find(ctx, template, address)
                    .await
                    .ok_or_else(|| WizardError::plain("That device isn't on the network any more. Search again?"))?,
            ),
            None => None,
        };
        let chosen = match variant {
            Some(id) => template
                .setup
                .iter()
                .find(|v| v.id == id)
                .ok_or_else(|| WizardError::plain(format!("{} has no setup {id:?}", template.name)))?,
            None if found.is_some() => template
                .setup
                .iter()
                .find(|v| v.steps.iter().any(|s| matches!(s, Step::Discover)))
                .unwrap_or(&template.setup[0]),
            None => &template.setup[0],
        };

        let mut session = Session {
            id: new_session_id(),
            template: template.clone(),
            variant: chosen.id.clone(),
            existing: None,
            steps: chosen.steps.clone(),
            pos: 0,
            history: Vec::new(),
            values: SetupValues {
                template: template.id.clone(),
                ..Default::default()
            },
            found: None,
            probe: None,
            login: LoginPhase::Account,
            account_name: None,
            last_active: Instant::now(),
        };
        if let Some(found) = found {
            session.take_found(found);
        }
        /* The network a device set up over Bluetooth joins: the hub's own,
         * unless the person types another. */
        if let Some(ssid) = &ctx.hub_wifi {
            if session.template.inputs.iter().any(|i| i.id == "wifi_ssid") {
                session.values.plain.insert("wifi_ssid".into(), ssid.clone());
            }
        }
        session.skip_answered();
        let view = session.view(ctx).await;
        Ok((session, view))
    }

    /* "Pair again" or "change settings" for an existing device (see the
     * header). Starts with its current config and secrets. */
    pub async fn start_for(ctx: &Context<'_>, device_id: &str, mode: Mode) -> Result<(Session, StepView), WizardError> {
        let device = ctx
            .control
            .get(device_id)
            .await
            .map_err(WizardError::plain)?
            .ok_or_else(|| WizardError::plain(format!("unknown device {device_id:?}")))?;
        let template = ctx
            .templates
            .get(&device.template)
            .ok_or_else(|| WizardError::plain(format!("{} has no device type this hub knows", device.name)))?;
        supported(template).map_err(WizardError::plain)?;
        let steps = match mode {
            Mode::Reauth => {
                let steps = action_steps(template, &template.reauth);
                if steps.is_empty() {
                    return Err(WizardError::plain(format!("{} doesn't need pairing", device.name)));
                }
                steps
            }
            Mode::Reconfigure => {
                let fields = form_fields(template);
                if fields.is_empty() {
                    return Err(WizardError::plain(format!("{} has no settings to change", device.name)));
                }
                vec![Step::Form { fields }]
            }
        };
        let has_test = template.setup.iter().any(|v| v.steps.iter().any(|s| matches!(s, Step::Test)));
        let mut steps = steps;
        if has_test {
            steps.push(Step::Test);
        }
        let session = Session {
            id: new_session_id(),
            template: template.clone(),
            variant: match mode {
                Mode::Reauth => "reauth".into(),
                Mode::Reconfigure => "reconfigure".into(),
            },
            values: SetupValues {
                plain: device.config.clone(),
                secret: ctx.control.secrets().get(&device.id),
                template: template.id.clone(),
                ..Default::default()
            },
            existing: Some((device, mode)),
            steps,
            pos: 0,
            history: Vec::new(),
            found: None,
            probe: None,
            login: LoginPhase::Account,
            account_name: None,
            last_active: Instant::now(),
        };
        let view = session.view(ctx).await;
        Ok((session, view))
    }

    /* The answer to the current step. On an error the step stays. */
    pub async fn answer(&mut self, ctx: &Context<'_>, values: Map<String, Value>) -> Result<StepView, WizardError> {
        self.last_active = Instant::now();
        let Some(step) = self.steps.get(self.pos).cloned() else {
            return Err(WizardError::plain("all steps are done: finish with a name and room"));
        };
        let before = (self.pos, self.steps.clone());
        match step {
            Step::Info { .. } => {}
            Step::Discover => {
                let Some(address) = values.get("found").and_then(Value::as_str) else {
                    /* No pick: "search again". */
                    ctx.discovery.discover_now();
                    return Ok(self.view(ctx).await);
                };
                let found = find(ctx, &self.template, address)
                    .await
                    .ok_or_else(|| WizardError::plain("That device isn't on the network any more. Search again?"))?;
                self.take_found(found);
            }
            Step::Form { fields } => {
                for id in &fields {
                    self.take_input(id, values.get(id))?;
                }
            }
            Step::Choice { field, then } => {
                self.take_input(&field, values.get(&field))?;
                let picked = self.values.plain.get(&field).cloned().unwrap_or_default();
                if let Some(branch) = then.get(&picked) {
                    let at = self.pos + 1;
                    self.steps.splice(at..at, branch.iter().cloned());
                }
            }
            Step::ConfirmOnDevice { action, timeout_s, .. } => {
                /* Running out of time here means nobody confirmed. */
                let limit = Duration::from_secs(timeout_s.into());
                self.run_action(ctx, &action, limit, ErrorKind::NotConfirmed).await?;
            }
            Step::CodeFromDevice { action, field } => {
                self.take_input(&field, values.get(&field))?;
                self.run_action(ctx, &action, crate::adapters::net::HTTP_TIMEOUT * 6, ErrorKind::Timeout).await?;
            }
            Step::Test => self.test(ctx).await?,
            /* Refused at start (supported). */
            Step::ProvisionBle { service_uuid, action, .. } => {
                /* The adapter needs to know which Bluetooth service to look
                 * for; a value for this step only. */
                self.values.plain.insert("ble_service".into(), service_uuid);
                let result = self.run_action(ctx, &action, PROVISION_LIMIT, ErrorKind::Timeout).await;
                self.values.plain.remove("ble_service");
                result?;
                /* The device has the WiFi details now; the hub forgets them. */
                self.values.plain.retain(|k, _| !k.starts_with("wifi_"));
                self.values.secret.retain(|k, _| !k.starts_with("wifi_"));
            }
            Step::VendorLogin {
                fields,
                action,
                otp_action,
                otp_field,
                list_action,
                ..
            } => {
                let done = self
                    .vendor_login(ctx, &values, &fields, &action, otp_action.as_deref(), &otp_field, list_action.as_deref())
                    .await?;
                if !done {
                    /* The next screen of the same step. */
                    return Ok(self.view(ctx).await);
                }
                self.forget_login(&fields, &otp_field);
            }
            Step::ProvisionSoftap { .. } | Step::Smartconfig { .. } => {
                return Err(WizardError::plain("this step isn't supported yet"));
            }
        }
        self.login = LoginPhase::Account;
        self.history.push(before);
        self.pos += 1;
        self.skip_answered();
        Ok(self.view(ctx).await)
    }

    pub async fn back(&mut self, ctx: &Context<'_>) -> Result<StepView, WizardError> {
        self.last_active = Instant::now();
        let (pos, steps) = self
            .history
            .pop()
            .ok_or_else(|| WizardError::plain("this is the first step"))?;
        self.pos = pos;
        self.steps = steps;
        self.login = LoginPhase::Account;
        Ok(self.view(ctx).await)
    }

    /* The last word: creates the device and starts its adapter. */
    pub async fn finish(&mut self, ctx: &Context<'_>, name: &str, room: &str) -> Result<Device, WizardError> {
        self.last_active = Instant::now();
        if self.pos < self.steps.len() {
            return Err(WizardError::plain("not all steps are done yet"));
        }
        if let Some((device, _)) = &self.existing {
            /* Only the settings the template knows (a stray value picked
             * up on the way, like a found device's "name", isn't one). */
            let mut config = self.values.plain.clone();
            config.remove("name");
            return ctx
                .control
                .update_from_setup(&device.id, config, self.values.secret.clone())
                .await
                .map_err(WizardError::plain);
        }
        let name = name.trim();
        if name.is_empty() {
            return Err(WizardError::field("name", "Give the device a name."));
        }
        if let Some(existing) = self.already_added(ctx).await {
            return Err(WizardError::plain(format!("This device is already added, as \"{existing}\".")));
        }
        let devices = ctx.control.list().await.map_err(WizardError::plain)?;
        let id = unique_id(name, &self.template.id, &devices);

        /* Everything the adapter needs to run it: the plain values (minus
         * the found device's name, which isn't a setting) and the
         * template's fixed adapter settings. */
        let mut config: BTreeMap<String, String> = self.values.plain.clone();
        config.remove("name");
        for (key, value) in &self.template.adapter_config {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            config.insert(key.clone(), text);
        }
        let capabilities = device::Capabilities::with_defaults(&self.template.capabilities).map_err(WizardError::plain)?;
        let new = Device {
            id,
            name: name.to_string(),
            room: room.trim().to_string(),
            template: self.template.id.clone(),
            source: Source::new(&self.template.adapter),
            config,
            identity: self.identity(),
            online: None,
            last_seen: None,
            favourite: false,
            capabilities,
        };
        ctx.control
            .add_from_setup(new, self.values.secret.clone())
            .await
            .map_err(WizardError::plain)
    }

    /* ---- inside ---- */

    /* The step to show now, as a client sees it. */
    async fn view(&self, ctx: &Context<'_>) -> StepView {
        let summary = self.probe.as_ref().map(|p| p.summary.clone()).unwrap_or_default();
        let step = match self.steps.get(self.pos) {
            None if self.existing.is_some() => StepKind::Save {
                device: self.existing.as_ref().map(|(d, _)| d.name.clone()).unwrap_or_default(),
                summary,
            },
            None => StepKind::Name {
                name: self.default_name(),
                room: self.template.defaults.room.clone(),
                summary,
            },
            /* Issue #72: the text may show values setup collected so far,
             * {name} -- and {secret.name}: a device password the hub just
             * made (mqtt_login), shown ONCE to be typed into the device.
             * Templates come from the image, so only their authors decide
             * which secret a page shows. */
            Some(Step::Info { text, image }) => StepKind::Info {
                text: {
                    let mut values = self.values.plain.clone();
                    for (name, secret) in &self.values.secret {
                        values.insert(format!("secret.{name}"), secret.expose().to_string());
                    }
                    crate::discovery::fill(text, &values)
                },
                image: image.clone(),
            },
            Some(Step::Discover) => StepKind::Discover {
                found: inbox(ctx, &self.template)
                    .await
                    .into_iter()
                    .map(|f| FoundView {
                        address: f.address.to_string(),
                        name: f.name,
                    })
                    .collect(),
            },
            Some(Step::Form { fields }) => StepKind::Form {
                fields: fields.iter().filter_map(|id| self.field(id)).collect(),
            },
            Some(Step::Choice { field, .. }) => match self.field(field) {
                Some(field) => StepKind::Choice { field },
                None => StepKind::Test, /* can't happen: templates.rs checked it */
            },
            Some(Step::ConfirmOnDevice { hint, hints, timeout_s, .. }) => StepKind::ConfirmOnDevice {
                hint: hint.clone(),
                hints: hints.clone(),
                timeout_s: *timeout_s,
            },
            Some(Step::CodeFromDevice { field, .. }) => match self.field(field) {
                Some(field) => StepKind::CodeFromDevice { field },
                None => StepKind::Test,
            },
            Some(Step::VendorLogin {
                vendor,
                fields,
                otp_field,
                hint,
                ..
            }) => StepKind::VendorLogin {
                vendor: vendor.clone(),
                phase: self.login,
                hint: match self.login {
                    LoginPhase::Account => hint.clone(),
                    LoginPhase::Code => format!("{vendor} sent you a code. Type it here."),
                    LoginPhase::Pick => format!("Which device of your {vendor} account is it?"),
                },
                fields: match self.login {
                    LoginPhase::Account => fields.iter().filter_map(|id| self.field(id)).collect(),
                    LoginPhase::Code => self.field(otp_field).into_iter().collect(),
                    LoginPhase::Pick => Vec::new(),
                },
                accounts: match self.login {
                    LoginPhase::Account => crate::accounts::list(ctx.control.secrets(), Some(&self.template.adapter))
                        .into_iter()
                        .map(|a| AccountView { id: a.id(), label: a.account })
                        .collect(),
                    _ => Vec::new(),
                },
                devices: match self.login {
                    LoginPhase::Pick => self
                        .values
                        .cloud_devices
                        .iter()
                        .map(|d| CloudDeviceView {
                            id: d.id.clone(),
                            name: d.name.clone(),
                            detail: d.detail.clone(),
                            available: d.available,
                        })
                        .collect(),
                    _ => Vec::new(),
                },
            },
            Some(Step::Test) => StepKind::Test,
            Some(Step::ProvisionBle { hint, .. }) => StepKind::ProvisionBle {
                hint: if hint.is_empty() {
                    "Setting the device up over Bluetooth\u{2026}".into()
                } else {
                    hint.clone()
                },
                timeout_s: PROVISION_LIMIT.as_secs() as u32,
            },
            Some(_) => StepKind::Test, /* unsupported: refused at start */
        };
        StepView {
            session: self.id.clone(),
            template: self.template.id.clone(),
            variant: self.variant.clone(),
            number: self.history.len() + 1,
            step,
        }
    }

    fn input(&self, id: &str) -> Option<&Input> {
        self.template.inputs.iter().find(|i| i.id == id)
    }

    fn field(&self, id: &str) -> Option<Field> {
        let input = self.input(id)?;
        let value = if input.kind == InputKind::Secret {
            None
        } else {
            self.values
                .plain
                .get(id)
                .cloned()
                .or_else(|| input.default.as_ref().map(value_text))
        };
        Some(Field {
            id: input.id.clone(),
            kind: input.kind,
            label: input.label.clone(),
            hint: input.hint.clone(),
            required: input.required,
            choices: input
                .choices
                .iter()
                .map(|c| ChoiceView {
                    value: c.value.clone(),
                    label: c.label.clone(),
                })
                .collect(),
            value,
        })
    }

    /* One answered input: checked, then kept (plain or secret). */
    /* One answer to a vendor_login step (see the header); true when the
     * step is done. */
    #[allow(clippy::too_many_arguments)]
    async fn vendor_login(
        &mut self,
        ctx: &Context<'_>,
        values: &Map<String, Value>,
        fields: &[String],
        action: &str,
        otp_action: Option<&str>,
        otp_field: &str,
        list_action: Option<&str>,
    ) -> Result<bool, WizardError> {
        /* Talking to a vendor's cloud: several requests, each up to
         * CLOUD_TIMEOUT. */
        let limit = crate::adapters::cloud::CLOUD_TIMEOUT * 4;
        /* "Start over" (another email, a new code): back to the account
         * screen of the same step. */
        if values.get("restart").and_then(Value::as_bool) == Some(true) {
            self.login = LoginPhase::Account;
            self.values.cloud_devices.clear();
            return Ok(false);
        }
        match self.login {
            /* A saved account: its session, no password, no code. The
             * step's other choices (Roborock's map toggle) still count. */
            LoginPhase::Account if values.get("account").and_then(Value::as_str).is_some() => {
                let id = values["account"].as_str().unwrap_or_default();
                let account = crate::accounts::get(ctx.control.secrets(), id)
                    .filter(|a| a.vendor == self.template.adapter)
                    .ok_or_else(|| WizardError::plain("That account isn't saved any more: sign in."))?;
                for id in fields {
                    let optional = self.input(id).is_some_and(|i| matches!(i.kind, InputKind::Toggle | InputKind::Choice));
                    if optional {
                        self.take_input(id, values.get(id))?;
                    }
                }
                self.values.plain.insert("email".into(), account.account.clone());
                self.values.plain.insert("account".into(), account.id());
                self.values.secret.insert("login_session".into(), Secret::new(account.session));
            }
            LoginPhase::Account => {
                for id in fields {
                    self.take_input(id, values.get(id))?;
                }
                self.run_action(ctx, action, limit, ErrorKind::Timeout).await?;
                /* The code screen -- unless the vendor let us in without one
                 * (EZVIZ asks only when two-step verification is on): the
                 * action then says login_needs_code = "no". */
                let needs_code = self.values.plain.get("login_needs_code").map(String::as_str) != Some("no");
                if otp_action.is_some() && needs_code {
                    self.login = LoginPhase::Code;
                    return Ok(false);
                }
            }
            LoginPhase::Code => {
                self.take_input(otp_field, values.get(otp_field))?;
                if let Some(otp_action) = otp_action {
                    self.run_action(ctx, otp_action, limit, ErrorKind::Timeout).await?;
                }
            }
            LoginPhase::Pick => {
                let id = values
                    .get("device")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WizardError::plain("Pick one of the devices."))?;
                let id = id.to_string();
                return self.pick_and_locate(ctx, &id).map(|_| true);
            }
        }
        /* Logged in (with a password or a saved account): the account is
         * saved -- its session, never the password -- and the device will
         * refer to it (config "account"). */
        self.save_account(ctx);
        /* The account's devices, if the template lists them. */
        let Some(list_action) = list_action else { return Ok(true) };
        self.values.cloud_devices.clear();
        self.run_action(ctx, list_action, limit, ErrorKind::Timeout).await?;
        if self.values.cloud_devices.is_empty() {
            self.login = LoginPhase::Account;
            return Err(WizardError::plain(
                "This account has no devices of this kind. Add the device in the vendor's app first.",
            ));
        }
        /* Already known which one: the device picked on the network, or
         * the one being paired again (same identity). */
        let known = match (&self.existing, &self.found) {
            (Some((device, _)), _) => device.identity.clone(),
            (None, Some(found)) => found.identity.clone(),
            _ => String::new(),
        };
        if !known.is_empty() {
            let template_identity = self.template.identity.clone();
            let same = self.values.cloud_devices.iter().find(|d| {
                let mut values = self.values.plain.clone();
                values.extend(d.plain.clone());
                discovery::fill(&template_identity, &values) == known
            });
            if let Some(id) = same.map(|d| d.id.clone()) {
                if let Err(e) = self.pick_and_locate(ctx, &id) {
                    self.login = LoginPhase::Pick;
                    return Err(e);
                }
                return Ok(true);
            }
            if self.existing.is_some() {
                self.login = LoginPhase::Account;
                return Err(WizardError::plain("This device isn't in that account (any more). Log in with the account it belongs to."));
            }
        }
        self.login = LoginPhase::Pick;
        Ok(false)
    }

    /* Saves the account the person just signed in to (accounts.rs). */
    fn save_account(&mut self, ctx: &Context<'_>) {
        let (Some(session), Some(email)) = (self.values.secret.get("login_session"), self.values.plain.get("email")) else {
            return;
        };
        let account = crate::accounts::Account {
            vendor: self.template.adapter.clone(),
            account: email.clone(),
            session: session.expose().to_string(),
        };
        crate::accounts::save(ctx.control.secrets(), &account);
        self.values.plain.insert("account".into(), account.id());
    }

    /* pick_cloud_device, then its address: a vendor's list says which
     * device, not where it is on the LAN -- discovery saw it there (same
     * identity; a Roborock's broadcast). A device being paired again keeps
     * its address. */
    fn pick_and_locate(&mut self, ctx: &Context<'_>, id: &str) -> Result<(), WizardError> {
        let before = self.values.clone();
        self.pick_cloud_device(id)?;
        if self.values.plain.get("host").is_some_and(|h| !h.is_empty()) || self.template.discovery.is_empty() {
            return Ok(());
        }
        let found = ctx.discovery.lookup(&self.template.id, &self.identity());
        match found.and_then(|f| f.values.get("host").cloned().or(Some(f.address.to_string()))) {
            Some(host) => {
                self.values.plain.insert("host".into(), host);
                Ok(())
            }
            None => {
                let name = self.account_name.take().unwrap_or_default();
                self.values = before;
                Err(WizardError::plain(format!(
                    "{name} isn't on this network right now. Is it switched on, and on the same WiFi as the hub? \
                     It announces itself every few seconds: try again in a moment."
                )))
            }
        }
    }

    /* The person picked device `id` of the account: its values become the
     * device's. */
    fn pick_cloud_device(&mut self, id: &str) -> Result<(), WizardError> {
        let device = self
            .values
            .cloud_devices
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .ok_or_else(|| WizardError::plain("That device isn't in the list."))?;
        if !device.available {
            return Err(WizardError::plain(format!("{} can't be added: {}", device.name, device.detail)));
        }
        self.values.plain.extend(device.plain);
        self.values.secret.extend(device.secret);
        self.account_name = Some(device.name);
        self.values.cloud_devices.clear();
        Ok(())
    }

    /* After a vendor_login step: the account's secret fields (the
     * password), the code and the session ("login_*") go -- the hub keeps
     * no way into the account. Plain fields (the email) stay, to show
     * which account a device came from. */
    fn forget_login(&mut self, fields: &[String], otp_field: &str) {
        for id in fields.iter().map(String::as_str).chain([otp_field]) {
            if self.input(id).is_some_and(|i| i.kind == InputKind::Secret) || id == otp_field {
                self.values.secret.remove(id);
                self.values.plain.remove(id);
            }
        }
        self.values.plain.retain(|k, _| !k.starts_with("login_"));
        self.values.secret.retain(|k, _| !k.starts_with("login_"));
        self.values.cloud_devices.clear();
    }

    fn take_input(&mut self, id: &str, answer: Option<&Value>) -> Result<(), WizardError> {
        let input = self
            .input(id)
            .cloned()
            .ok_or_else(|| WizardError::plain(format!("unknown field {id:?}")))?;
        let text = answer.map(value_text).unwrap_or_default();
        let text = if input.kind == InputKind::Secret { text } else { text.trim().to_string() };
        if text.is_empty() {
            /* Changing settings: an empty secret field means "keep the
             * current one" (it's never shown, so it's always empty). */
            if input.kind == InputKind::Secret && self.existing.is_some() && self.values.secret.contains_key(id) {
                return Ok(());
            }
            if input.required {
                return Err(WizardError::field(id, format!("{} is needed.", input.label)));
            }
            self.values.plain.remove(id);
            self.values.secret.remove(id);
            return Ok(());
        }
        let text = check_input(&input, &text).map_err(|message| WizardError::field(id, message))?;
        if input.kind == InputKind::Secret {
            self.values.secret.insert(id.to_string(), Secret::new(text));
        } else {
            self.values.plain.insert(id.to_string(), text);
        }
        Ok(())
    }

    /* A device picked from the network: its values (the address...) are
     * taken over; they replace what an earlier pick filled in. */
    fn take_found(&mut self, found: Found) {
        for (key, value) in &found.values {
            self.values.plain.insert(key.clone(), value.clone());
        }
        self.found = Some(found);
    }

    /* Steps already answered by other means: a discover step when a
     * device was picked from the inbox. */
    fn skip_answered(&mut self) {
        while matches!(self.steps.get(self.pos), Some(Step::Discover)) && self.found.is_some() {
            self.pos += 1;
        }
    }

    /* `late`: the error kind when `limit` runs out. */
    async fn run_action(&mut self, ctx: &Context<'_>, action: &str, limit: Duration, late: ErrorKind) -> Result<(), WizardError> {
        let adapters = ctx.control.adapters();
        let adapter = adapters
            .get(&self.template.adapter)
            .ok_or_else(|| WizardError::plain("this device type's adapter is missing"))?;
        let result = match tokio::time::timeout(limit, adapter.action(action, &self.values)).await {
            Ok(result) => result,
            Err(_) => Err(SetupError::new(late, format!("{action} took longer than {} s", limit.as_secs()))),
        };
        let new = result.map_err(|e| self.explain(e))?;
        self.values.plain.extend(new.plain);
        self.values.secret.extend(new.secret);
        if !new.cloud_devices.is_empty() {
            self.values.cloud_devices = new.cloud_devices;
        }
        Ok(())
    }

    /* The test step: the adapter's probe, then "is it added already?". */
    async fn test(&mut self, ctx: &Context<'_>) -> Result<(), WizardError> {
        let adapters = ctx.control.adapters();
        let adapter = adapters
            .get(&self.template.adapter)
            .ok_or_else(|| WizardError::plain("this device type's adapter is missing"))?;
        /* Issue #74: a device of a saved account is tested with the
         * account's session, handed over as "account_session" for this
         * call only (it stays with the account, not the device). */
        let mut values = self.values.clone();
        if let Some(account) = values.plain.get("account").and_then(|a| crate::accounts::get(ctx.control.secrets(), a)) {
            values.secret.insert("account_session".into(), Secret::new(account.session));
        }
        let probe = adapter.probe(&values).await.map_err(|e| self.explain(e))?;
        if let Some((device, _)) = &self.existing {
            /* An existing device: what it says about itself now counts
             * (the stored values may be the old device's) -- and it must
             * still BE that device. */
            for (key, value) in &probe.values {
                self.values.plain.insert(key.clone(), value.clone());
            }
            let identity = self.identity();
            if !device.identity.is_empty() && !identity.is_empty() && identity != device.identity {
                return Err(WizardError::plain(format!(
                    "That's another device, not {}. To use it, add it as a new device.",
                    device.name
                )));
            }
        } else {
            /* New values only: never overrule what the person typed or
             * the network said. */
            for (key, value) in &probe.values {
                self.values.plain.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        self.probe = Some(probe);
        if let Some(existing) = self.already_added(ctx).await {
            return Err(WizardError::plain(format!("This device is already added, as \"{existing}\".")));
        }
        Ok(())
    }

    /* The template's identity, filled from the values ("{mac}"). */
    fn identity(&self) -> String {
        discovery::fill(&self.template.identity, &self.values.plain)
    }

    /* The name of an existing device that is this same device: same
     * identity, or (without one) same address. */
    async fn already_added(&self, ctx: &Context<'_>) -> Option<String> {
        let identity = self.identity();
        let host = self.values.plain.get("host");
        let devices = ctx.control.list().await.ok()?;
        let myself = self.existing.as_ref().map(|(d, _)| d.id.as_str());
        devices
            .into_iter()
            .find(|d| {
                Some(d.id.as_str()) != myself
                    && d.template == self.template.id
                    && if identity.is_empty() {
                        host.is_some() && d.config.get("host") == host
                    } else {
                        d.identity == identity
                    }
            })
            .map(|d| d.name)
    }

    /* What the device calls itself, else the template's default. A
     * machine-made network name ("wled-6b33ac") isn't offered: "LED strip"
     * reads better. */
    fn default_name(&self) -> String {
        self.probe
            .as_ref()
            .and_then(|p| p.name.clone())
            .or_else(|| self.account_name.clone())
            .or_else(|| self.found.as_ref().map(|f| f.name.clone()).filter(|n| !looks_generated(n)))
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| {
                if self.template.defaults.name.is_empty() {
                    self.template.name.clone()
                } else {
                    self.template.defaults.name.clone()
                }
            })
    }

    /* An adapter's failure, as a sentence (the template may have its own
     * for this kind). */
    fn explain(&self, error: SetupError) -> WizardError {
        println!("wizard: {} setup: {:?}: {}", self.template.id, error.kind, error.detail);
        let message = match self.template.errors.get(&error.kind) {
            Some(own) => own.clone(),
            None => default_message(error.kind).to_string(),
        };
        WizardError {
            field: None,
            message: message.replace("{message}", &error.detail),
            detail: error.detail,
        }
    }
}

/* The steps running these actions, from all of a template's variants
 * (the first of each), for "pair again". */
fn action_steps(template: &Template, actions: &[String]) -> Vec<Step> {
    fn walk(steps: &[Step], actions: &[String], found: &mut Vec<Step>) {
        for step in steps {
            let action = match step {
                Step::ConfirmOnDevice { action, .. } | Step::CodeFromDevice { action, .. } | Step::VendorLogin { action, .. } => {
                    Some(action)
                }
                _ => None,
            };
            if let Some(action) = action {
                let seen = found.iter().any(|s| {
                    matches!(s, Step::ConfirmOnDevice { action: a, .. } | Step::CodeFromDevice { action: a, .. }
                        | Step::VendorLogin { action: a, .. } if a == action)
                });
                if actions.contains(action) && !seen {
                    found.push(step.clone());
                }
            }
            if let Step::Choice { then, .. } = step {
                for branch in then.values() {
                    walk(branch, actions, found);
                }
            }
        }
    }
    let mut found = Vec::new();
    for variant in &template.setup {
        walk(&variant.steps, actions, &mut found);
    }
    found
}

/* Every field any of a template's forms asks for, in order, once: what
 * "change settings" shows. */
fn form_fields(template: &Template) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    /* Not from a way in that sets the device's WiFi up (#42): its fields
     * (the WiFi name and password, the code for that session) only mean
     * something together with that step, which "change settings" doesn't
     * run -- the password would be stored on the hub for nothing. */
    let onboarding = |steps: &[Step]| {
        steps
            .iter()
            .any(|s| matches!(s, Step::ProvisionBle { .. } | Step::ProvisionSoftap { .. } | Step::Smartconfig { .. }))
    };
    for variant in template.setup.iter().filter(|v| !onboarding(&v.steps)) {
        for step in &variant.steps {
            if let Step::Form { fields: these } = step {
                for field in these {
                    if !fields.contains(field) {
                        fields.push(field.clone());
                    }
                }
            }
        }
    }
    fields
}

/* "wled-6b33ac", "esp32-1a2b3c": lowercase, no spaces, ending in a
 * serial-like run of 4+ hex digits after a dash. */
fn looks_generated(name: &str) -> bool {
    match name.rsplit_once('-') {
        Some((stem, tail)) => {
            !stem.is_empty()
                && !name.contains(' ')
                && name == name.to_lowercase()
                && tail.len() >= 4
                && tail.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    }
}

/* The built-in sentences (wiki Device-Templates, "Errors in plain
 * language"). */
fn default_message(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Unreachable => "The hub can't reach the device. Is it on and on the same network?",
        ErrorKind::Refused => "The device refused the login. Check the password.",
        ErrorKind::Timeout => "No answer in time. Try again.",
        ErrorKind::NotConfirmed => "The device wasn't confirmed. Accept the prompt on it and try again.",
        ErrorKind::Unsupported => "This device (or its firmware) isn't supported by this device type.",
        ErrorKind::Vendor => "The vendor's service said: {message}",
    }
}

/* The inbox, for one template. */
async fn inbox(ctx: &Context<'_>, template: &Template) -> Vec<Found> {
    ctx.discovery
        .inbox(ctx.control)
        .await
        .into_iter()
        .filter(|f| f.template == template.id)
        .collect()
}

async fn find(ctx: &Context<'_>, template: &Template, address: &str) -> Option<Found> {
    inbox(ctx, template).await.into_iter().find(|f| f.address.to_string() == address)
}

/* A JSON answer as text: strings as they are, numbers and booleans
 * written out. */
fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/* One input's rules (templates::Validate, and its kind); returns the
 * value as stored (a MAC normalised to aa:bb:cc:dd:ee:ff). */
fn check_input(input: &Input, text: &str) -> Result<String, String> {
    let label = &input.label;
    let chars = text.chars().count();
    let rules = &input.validate;
    let too_short = rules.min_length.is_some_and(|min| chars < min);
    let too_long = chars > rules.max_length.unwrap_or(MAX_TEXT).min(MAX_TEXT);
    if too_short || too_long {
        let min = rules.min_length.unwrap_or(1);
        let max = rules.max_length.unwrap_or(MAX_TEXT).min(MAX_TEXT);
        return Err(format!("{label}: {min}-{max} characters, please."));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("{label} can't contain line breaks or control characters."));
    }
    let text = match input.kind {
        InputKind::Number => {
            let number: f64 = text.parse().map_err(|_| format!("{label} must be a number."))?;
            if rules.min.is_some_and(|min| number < min) || rules.max.is_some_and(|max| number > max) {
                return Err(format!(
                    "{label} must be between {} and {}.",
                    rules.min.map_or("-".into(), |m| m.to_string()),
                    rules.max.map_or("-".into(), |m| m.to_string())
                ));
            }
            text.to_string()
        }
        InputKind::Toggle => match text {
            "true" | "false" => text.to_string(),
            _ => return Err(format!("{label}: on or off.")),
        },
        InputKind::Choice => {
            if !input.choices.iter().any(|c| c.value == text) {
                return Err(format!("{label}: pick one of the choices."));
            }
            text.to_string()
        }
        InputKind::Text | InputKind::Secret => text.to_string(),
    };
    match rules.pattern {
        None => Ok(text),
        Some(pattern) => check_pattern(pattern, &text).ok_or_else(|| pattern_message(pattern, label)),
    }
}

/* The named patterns (templates::Pattern). Some(value as stored) if it
 * matches. */
fn check_pattern(pattern: Pattern, text: &str) -> Option<String> {
    let ok = match pattern {
        Pattern::Host => is_host(text),
        Pattern::Ipv4 => text.parse::<Ipv4Addr>().is_ok(),
        Pattern::Email => {
            let (user, domain) = text.split_once('@')?;
            !user.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !text.contains(' ')
        }
        Pattern::Digits => text.bytes().all(|b| b.is_ascii_digit()),
        Pattern::Hex => text.bytes().all(|b| b.is_ascii_hexdigit()),
        Pattern::Mac => return normalise_mac(text),
    };
    ok.then(|| text.to_string())
}

fn pattern_message(pattern: Pattern, label: &str) -> String {
    match pattern {
        Pattern::Host => format!("{label}: an IP address like 192.168.1.50, or a name like wled.local."),
        Pattern::Ipv4 => format!("{label}: an IP address like 192.168.1.50."),
        Pattern::Email => format!("{label}: an e-mail address."),
        Pattern::Digits => format!("{label}: digits only."),
        Pattern::Hex => format!("{label}: 0-9 and A-F only."),
        Pattern::Mac => format!("{label}: a MAC address like AA:BB:CC:DD:EE:FF."),
    }
}

/* An IPv4 address or a host name, optionally with ":port" (a device on a
 * non-standard port). A dotted-number text that isn't a valid address
 * ("192.168.1.300") is refused, not taken for a name. */
fn is_host(text: &str) -> bool {
    let host = match text.rsplit_once(':') {
        Some((host, port)) => {
            if port.parse::<u16>().map_or(true, |p| p == 0) {
                return false;
            }
            host
        }
        None => text,
    };
    if host.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return host.parse::<Ipv4Addr>().is_ok();
    }
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/* "AA-BB-CC-DD-EE-FF" / "aabbccddeeff" / "aa:bb:..." -> "aa:bb:cc:dd:ee:ff". */
fn normalise_mac(text: &str) -> Option<String> {
    let digits: String = text.chars().filter(|c| !matches!(c, ':' | '-')).collect();
    if digits.len() != 12 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digits = digits.to_ascii_lowercase();
    let pairs: Vec<&str> = (0..6).map(|i| &digits[i * 2..i * 2 + 2]).collect();
    Some(pairs.join(":"))
}

/* A device id from its name: "Kitchen strip" -> "kitchen-strip" (also its
 * AWS shadow's name, so only a-z, 0-9, -). Taken already: "-2", "-3"... */
fn unique_id(name: &str, fallback: &str, devices: &[Device]) -> String {
    let mut base = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c);
        } else if !base.ends_with('-') && !base.is_empty() {
            base.push('-');
        }
    }
    let mut base: String = base.trim_end_matches('-').chars().take(40).collect();
    base = base.trim_end_matches('-').to_string();
    if base.is_empty() {
        base = fallback.to_string();
    }
    let taken = |id: &str| devices.iter().any(|d| d.id == id);
    if !taken(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).expect("some number is free")
}

/* 16 random hex digits: a session's name, so a client can tell an old
 * session's reply from the current one's. */
fn new_session_id() -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 8];
    /* SystemRandom only fails if the OS has no randomness at all. */
    ring::rand::SystemRandom::new().fill(&mut bytes).expect("system randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::wled::Wled;
    use crate::adapters::wled_sim::Sim;
    use crate::adapters::Registry;
    use crate::device::Health;
    use crate::secrets::Secrets;
    use crate::state::{self, Outputs};
    use crate::templates::Known;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};

    fn text_input(validate: Value) -> Input {
        serde_json::from_value(json!({"id": "x", "type": "text", "label": "Address", "validate": validate})).unwrap()
    }

    #[test]
    fn inputs_are_checked() {
        let host = text_input(json!({"pattern": "host"}));
        for good in ["192.168.1.50", "wled-kitchen.local", "127.0.0.1:8080", "tv"] {
            assert!(check_input(&host, good).is_ok(), "{good}");
        }
        for bad in ["192.168.1.300", "has space", "-bad.local", "1.2.3.4:0", "a..b"] {
            assert!(check_input(&host, bad).is_err(), "{bad}");
        }
        let mac = text_input(json!({"pattern": "mac"}));
        assert_eq!(check_input(&mac, "AA-BB-CC-DD-EE-0F").unwrap(), "aa:bb:cc:dd:ee:0f");
        assert!(check_input(&mac, "AA-BB-CC").is_err());
        let short = text_input(json!({"min_length": 8}));
        assert!(check_input(&short, "1234567").is_err());
        let number: Input = serde_json::from_value(
            json!({"id": "n", "type": "number", "label": "Port", "validate": {"min": 1, "max": 65535}}),
        )
        .unwrap();
        assert!(check_input(&number, "80").is_ok());
        assert!(check_input(&number, "0").is_err());
        assert!(check_input(&number, "eighty").is_err());
    }

    #[test]
    fn generated_names_are_recognised() {
        assert!(looks_generated("wled-6b33ac"));
        assert!(looks_generated("esp32-1a2b"));
        assert!(!looks_generated("WLED-Kitchen"));
        assert!(!looks_generated("[LG] webOS TV OLED55C1"));
        assert!(!looks_generated("desk-lamp"));
    }

    #[test]
    fn ids_come_from_names() {
        let lamp = |id: &str| Device {
            id: id.into(),
            name: "x".into(),
            room: String::new(),
            template: String::new(),
            source: Source::default(),
            config: Default::default(),
            identity: String::new(),
            online: None,
            last_seen: None,
            favourite: false,
            capabilities: device::Capabilities::with_defaults(&["switch".to_string()]).unwrap(),
        };
        assert_eq!(unique_id("Kitchen strip", "wled", &[]), "kitchen-strip");
        assert_eq!(unique_id("  Łazienka! ", "wled", &[]), "azienka");
        assert_eq!(unique_id("!!!", "wled", &[]), "wled");
        let taken = [lamp("kitchen-strip"), lamp("kitchen-strip-2")];
        assert_eq!(unique_id("Kitchen strip", "wled", &taken), "kitchen-strip-3");
    }

    /* The real templates from the repo, the WLED adapter, a simulated
     * WLED: the whole "enter the address" way in. */
    struct Hub {
        templates: Templates,
        control: Control,
        discovery: Discovery,
    }

    impl Hub {
        fn ctx(&self) -> Context<'_> {
            Context {
                templates: &self.templates,
                control: &self.control,
                discovery: &self.discovery,
                hub_wifi: None,
            }
        }
    }

    async fn hub() -> Hub {
        let (state_tx, state_rx) = mpsc::channel(8);
        let outputs = Outputs {
            changed_tx: watch::channel(()).0,
            events_tx: tokio::sync::broadcast::channel(64).0,
            save_tx: watch::channel(Vec::new()).0,
        };
        tokio::spawn(state::run(state_rx, Default::default(), outputs));
        let registry = Arc::new(Registry::new(vec![Box::new(Wled), Box::new(crate::adapters::lg_webos::LgWebos)]));
        let secrets = Arc::new(Secrets::new(Default::default(), watch::channel(Vec::new()).0));
        let control = Control::new(state_tx, registry.clone(), secrets);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let (templates, _) = Templates::load(
            &dir,
            &Known {
                adapters: &registry.ids(),
                capabilities: &device::CAPABILITY_NAMES,
            },
        );
        Hub {
            templates,
            control,
            discovery: Discovery::new(),
        }
    }

    /* Issue #42: "change settings" of an IR blaster offers its address,
     * never the WiFi name/password or code of the Bluetooth setup. */
    #[test]
    fn change_settings_leaves_out_the_bluetooth_setup_fields() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let (templates, problems) = Templates::load(
            &dir,
            &Known {
                adapters: &["m4-led", "wled", "lg-webos", "ir-blaster", "wiz", "http", "mqtt", "roborock", "ezviz", "tapo", "camera"],
                capabilities: &device::CAPABILITY_NAMES,
            },
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(form_fields(templates.get("ir-blaster").unwrap()), vec!["host"]);
    }

    #[tokio::test]
    async fn menu_lists_addable_templates_only() {
        let hub = hub().await;
        let ids: Vec<String> = list(&hub.templates).into_iter().map(|t| t.id).collect();
        /* The LED is built in: not offered. By category: lighting, media. */
        assert_eq!(ids, ["wled", "lg-webos-tv"]);
    }

    #[tokio::test]
    async fn adds_a_wled_by_address() {
        let hub = hub().await;
        let sim = Sim::start(true).await;
        let ctx = hub.ctx();

        let (mut session, view) = Session::start(&ctx, "wled", Some("advanced"), None).await.unwrap();
        let StepKind::Form { fields } = &view.step else { panic!("{view:?}") };
        assert_eq!(fields[0].id, "host");

        /* A bad address stays on the form, pointing at the field. */
        let err = session.answer(&ctx, json!({"host": "192.168.1.300"}).as_object().unwrap().clone()).await.unwrap_err();
        assert_eq!(err.field.as_deref(), Some("host"));

        let view = session.answer(&ctx, json!({"host": sim.host()}).as_object().unwrap().clone()).await.unwrap();
        assert_eq!(view.step, StepKind::Test);
        assert_eq!(view.number, 2);

        /* Back keeps the answer. */
        let view = session.back(&ctx).await.unwrap();
        let StepKind::Form { fields } = &view.step else { panic!("{view:?}") };
        assert_eq!(fields[0].value.as_deref(), Some(sim.host().as_str()));
        session.answer(&ctx, json!({"host": sim.host()}).as_object().unwrap().clone()).await.unwrap();

        /* The test: the sim's name becomes the default name. */
        let view = session.answer(&ctx, Map::new()).await.unwrap();
        assert_eq!(
            view.step,
            StepKind::Name {
                name: "WLED Sim".into(),
                room: String::new(),
                summary: "WLED 0.0.0-sim".into()
            }
        );

        let device = session.finish(&ctx, "Kitchen strip", "Kitchen").await.unwrap();
        assert_eq!(device.id, "kitchen-strip");
        assert_eq!(device.source.as_str(), "wled");
        assert_eq!(device.identity, "aabbccddeeff");
        assert_eq!(device.config["host"], sim.host());
        assert!(device.capabilities.color.is_some());

        /* Its adapter runs: the real state arrives. */
        let mut online = false;
        for _ in 0..50 {
            let d = hub.control.get("kitchen-strip").await.unwrap().unwrap();
            if d.online == Some(Health::Online) {
                assert_eq!(d.capabilities.dimmer.unwrap().level, 50);
                online = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(online);

        /* The same device again: refused at the test step. */
        let (mut again, _) = Session::start(&ctx, "wled", Some("advanced"), None).await.unwrap();
        again.answer(&ctx, json!({"host": sim.host()}).as_object().unwrap().clone()).await.unwrap();
        let err = again.answer(&ctx, Map::new()).await.unwrap_err();
        assert!(err.message.contains("Kitchen strip"), "{err:?}");
    }

    /* Tapped in "Found on your network": the discover step is skipped,
     * the found name is offered. */
    #[tokio::test]
    async fn adds_a_found_wled() {
        let hub = hub().await;
        let sim = Sim::start(true).await;
        let ctx = hub.ctx();
        hub.discovery.record(Found {
            template: "wled".into(),
            name: "WLED-Desk".into(),
            address: "127.0.0.1".parse().unwrap(),
            values: [("host".to_string(), sim.host()), ("mac".to_string(), "aabbccddeeff".to_string())].into(),
            identity: "aabbccddeeff".into(),
        });
        let (mut session, view) = Session::start(&ctx, "wled", None, Some("127.0.0.1")).await.unwrap();
        assert_eq!(view.step, StepKind::Test, "discover skipped");
        let view = session.answer(&ctx, Map::new()).await.unwrap();
        /* The sim's own name wins over the mDNS name. */
        let StepKind::Name { name, .. } = view.step else { panic!() };
        assert_eq!(name, "WLED Sim");
        let device = session.finish(&ctx, &name, "Office").await.unwrap();
        assert_eq!(device.identity, "aabbccddeeff");
        /* Added: gone from the inbox. */
        assert!(hub.discovery.inbox(&hub.control).await.is_empty());
        assert!(Session::start(&ctx, "wled", None, Some("127.0.0.1")).await.is_err());
    }

    fn answers(json: Value) -> Map<String, Value> {
        json.as_object().unwrap().clone()
    }

    /* Adds a WLED at `host` through the wizard. */
    async fn add_wled(ctx: &Context<'_>, host: &str) -> Device {
        let (mut session, _) = Session::start(ctx, "wled", Some("advanced"), None).await.unwrap();
        session.answer(ctx, answers(json!({ "host": host }))).await.unwrap();
        session.answer(ctx, Map::new()).await.unwrap();
        session.finish(ctx, "Strip", "").await.unwrap()
    }

    /* "Change settings": the address, pre-filled; a wrong one is refused
     * by the test step and changes nothing; a right one is saved on the
     * SAME device, whose adapter restarts there. */
    #[tokio::test]
    async fn reconfigure_moves_a_device() {
        let hub = hub().await;
        let ctx = hub.ctx();
        let old = Sim::start(true).await;
        let device = add_wled(&ctx, &old.host()).await;

        let (mut session, view) = Session::start_for(&ctx, &device.id, Mode::Reconfigure).await.unwrap();
        let StepKind::Form { fields } = &view.step else { panic!("{view:?}") };
        assert_eq!(fields[0].value.as_deref(), Some(old.host().as_str()));

        session.answer(&ctx, answers(json!({"host": "127.0.0.1:9"}))).await.unwrap();
        let err = session.answer(&ctx, Map::new()).await.unwrap_err();
        assert_eq!(err.message, default_message(ErrorKind::Unreachable));
        session.back(&ctx).await.unwrap();

        /* The strip got a new address (the sim plays the same ESP). */
        let new = Sim::start(true).await;
        session.answer(&ctx, answers(json!({"host": new.host()}))).await.unwrap();
        let view = session.answer(&ctx, Map::new()).await.unwrap();
        assert!(matches!(view.step, StepKind::Save { .. }), "{view:?}");
        let saved = session.finish(&ctx, "", "").await.unwrap();
        assert_eq!(saved.id, device.id);
        assert_eq!(saved.name, "Strip");
        assert_eq!(saved.config["host"], new.host());
        assert_eq!(hub.control.list().await.unwrap().len(), 1);

        /* The adapter follows: a change on the NEW sim shows up. */
        new.change_from_outside(json!({"on": false}));
        let mut followed = false;
        for _ in 0..50 {
            let d = hub.control.get(&device.id).await.unwrap().unwrap();
            if d.capabilities.switch.as_ref().is_some_and(|s| !s.on) {
                followed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(followed, "the adapter didn't move to the new address");
    }

    /* "Pair again": a TV that no longer accepts our key (unauthorized) is
     * paired anew -- only the prompt step and the test -- and works
     * again, as the same device. */
    #[tokio::test]
    async fn reauth_repairs_an_unauthorized_tv() {
        use crate::adapters::lg_sim::TvSim;
        let hub = hub().await;
        let ctx = hub.ctx();
        let tv = TvSim::start().await;
        let device = Device {
            id: "tv".into(),
            name: "Living room TV".into(),
            room: "Living room".into(),
            template: "lg-webos-tv".into(),
            source: Source::new("lg-webos"),
            config: [("host".to_string(), tv.host()), ("cert_sha256".to_string(), tv.fingerprint())].into(),
            identity: String::new(),
            online: None,
            last_seen: None,
            favourite: false,
            capabilities: device::Capabilities::with_defaults(&["switch".into(), "media".into()]).unwrap(),
        };
        let old_key = [("client_key".to_string(), Secret::new("revoked-key"))].into();
        hub.control.add_from_setup(device, old_key).await.unwrap();
        let unauthorized = || async { hub.control.get("tv").await.unwrap().unwrap().online };
        for _ in 0..50 {
            if unauthorized().await == Some(Health::Unauthorized) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(unauthorized().await, Some(Health::Unauthorized));

        let (mut session, view) = Session::start_for(&ctx, "tv", Mode::Reauth).await.unwrap();
        assert!(matches!(view.step, StepKind::ConfirmOnDevice { .. }), "{view:?}");
        let view = session.answer(&ctx, Map::new()).await.unwrap(); /* the prompt, accepted */
        assert_eq!(view.step, StepKind::Test);
        let view = session.answer(&ctx, Map::new()).await.unwrap();
        assert_eq!(
            view.step,
            StepKind::Save {
                device: "Living room TV".into(),
                summary: "LG OLED55SIM".into()
            }
        );
        let saved = session.finish(&ctx, "", "").await.unwrap();
        assert_eq!((saved.id.as_str(), saved.name.as_str()), ("tv", "Living room TV"));
        assert_eq!(hub.control.secrets().get("tv")["client_key"].expose(), TvSim::KEY);

        let mut online = false;
        for _ in 0..50 {
            if unauthorized().await == Some(Health::Online) {
                online = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(online, "still {:?}", unauthorized().await);

        /* A WLED has nothing to pair. */
        let sim = Sim::start(true).await;
        let strip = add_wled(&ctx, &sim.host()).await;
        assert!(Session::start_for(&ctx, &strip.id, Mode::Reauth).await.is_err());
    }

    #[tokio::test]
    async fn failures_are_explained() {
        let hub = hub().await;
        let ctx = hub.ctx();
        let (mut session, _) = Session::start(&ctx, "wled", Some("advanced"), None).await.unwrap();
        session.answer(&ctx, json!({"host": "127.0.0.1:9"}).as_object().unwrap().clone()).await.unwrap();
        let err = session.answer(&ctx, Map::new()).await.unwrap_err();
        assert_eq!(err.message, default_message(ErrorKind::Unreachable));
        assert!(err.detail.contains("127.0.0.1:9"));
        /* Unfinished: no device. */
        assert!(session.finish(&ctx, "x", "").await.is_err());
        assert!(Session::start(&ctx, "m4-led", None, None).await.is_err());
        assert!(Session::start(&ctx, "nope", None, None).await.is_err());
    }

    /* Issue #74: a vendor's cloud, played by a fake "roborock" adapter --
     * the real roborock-vacuum template's vendor_login step. */
    struct FakeCloud;

    impl crate::adapters::Adapter for FakeCloud {
        fn id(&self) -> &'static str {
            "roborock"
        }
        fn start(&self, _: &Device, _: crate::adapters::Hub) -> crate::adapters::DeviceHandle {
            crate::adapters::DeviceHandle::new(mpsc::channel(1).0)
        }
        fn probe<'a>(&'a self, values: &'a SetupValues) -> crate::adapters::BoxFuture<'a, Result<Probe, SetupError>> {
            let ok = values.secret.get("local_key").is_some_and(|k| k.expose() == "key-of-d1");
            Box::pin(async move {
                if ok {
                    Ok(Probe { summary: "S7: charging".into(), ..Default::default() })
                } else {
                    Err(SetupError::new(ErrorKind::Refused, "wrong key"))
                }
            })
        }
        fn action<'a>(&'a self, name: &'a str, values: &'a SetupValues) -> crate::adapters::BoxFuture<'a, Result<SetupValues, SetupError>> {
            Box::pin(async move {
                let mut out = SetupValues::default();
                match name {
                    "send_code" => {
                        out.plain.insert("login_client".into(), "abc".into());
                        /* An account without two-step verification. */
                        if values.plain.get("email").is_some_and(|e| e.starts_with("nocode")) {
                            out.plain.insert("login_needs_code".into(), "no".into());
                            out.secret.insert("login_session".into(), Secret::new("session"));
                        }
                    }
                    "login" => {
                        if values.plain.get("code").map(String::as_str) != Some("123456") {
                            return Err(SetupError::new(ErrorKind::Vendor, "That code isn't right."));
                        }
                        out.secret.insert("login_session".into(), Secret::new("session"));
                    }
                    "list_devices" => {
                        assert!(values.secret.contains_key("login_session"));
                        out.cloud_devices = vec![
                            crate::adapters::CloudDevice {
                                id: "D1".into(),
                                name: "S7".into(),
                                detail: "Roborock S7 - online".into(),
                                available: true,
                                plain: [("duid".to_string(), "D1".to_string()), ("protocol".to_string(), "tcp".to_string())].into(),
                                secret: [("local_key".to_string(), Secret::new("key-of-d1"))].into(),
                            },
                            crate::adapters::CloudDevice {
                                id: "D2".into(),
                                name: "Q7".into(),
                                detail: "isn't supported yet".into(),
                                available: false,
                                ..Default::default()
                            },
                        ];
                    }
                    _ => unreachable!(),
                }
                Ok(out)
            })
        }
    }

    async fn cloud_hub() -> Hub {
        let (state_tx, state_rx) = mpsc::channel(8);
        let outputs = Outputs {
            changed_tx: watch::channel(()).0,
            events_tx: tokio::sync::broadcast::channel(64).0,
            save_tx: watch::channel(Vec::new()).0,
        };
        tokio::spawn(state::run(state_rx, Default::default(), outputs));
        let registry = Arc::new(Registry::new(vec![Box::new(FakeCloud)]));
        let secrets = Arc::new(Secrets::new(Default::default(), watch::channel(Vec::new()).0));
        let control = Control::new(state_tx, registry.clone(), secrets);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let (templates, _) = Templates::load(&dir, &Known { adapters: &registry.ids(), capabilities: &device::CAPABILITY_NAMES });
        Hub { templates, control, discovery: Discovery::new() }
    }

    fn answer(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn seen_vacuum(hub: &Hub) {
        hub.discovery.record(Found {
            template: "roborock-vacuum".into(),
            name: "Roborock vacuum".into(),
            address: "192.0.2.5".parse().unwrap(),
            values: [("host".to_string(), "192.0.2.5".to_string()), ("duid".to_string(), "D1".to_string())].into(),
            identity: "D1".into(),
        });
    }

    #[tokio::test]
    async fn a_vendor_login_picks_a_device_and_forgets_the_session() {
        let hub = cloud_hub().await;
        let ctx = hub.ctx();
        let (mut session, view) = Session::start(&ctx, "roborock-vacuum", None, None).await.unwrap();
        let StepKind::VendorLogin { phase, fields, .. } = &view.step else { panic!("{view:?}") };
        assert_eq!((*phase, fields[0].id.as_str()), (LoginPhase::Account, "email"));
        assert_eq!(session.answer(&ctx, answer(json!({"email": "not an email"}))).await.unwrap_err().field.as_deref(), Some("email"));

        /* Email -> the code screen of the same step. */
        let view = session.answer(&ctx, answer(json!({"email": "me@example.com"}))).await.unwrap();
        let StepKind::VendorLogin { phase, fields, .. } = &view.step else { panic!("{view:?}") };
        assert_eq!((*phase, fields[0].id.as_str()), (LoginPhase::Code, "code"));
        assert_eq!(view.number, 1);

        /* A wrong code: the vendor's sentence, still the code screen. */
        let err = session.answer(&ctx, answer(json!({"code": "000000"}))).await.unwrap_err();
        assert_eq!(err.message, "That code isn't right.");

        /* The right one -> the account's devices. */
        let view = session.answer(&ctx, answer(json!({"code": "123456"}))).await.unwrap();
        let StepKind::VendorLogin { phase, devices, .. } = &view.step else { panic!("{view:?}") };
        assert_eq!(*phase, LoginPhase::Pick);
        assert_eq!(devices.len(), 2);
        assert!(!devices[1].available);
        assert!(session.answer(&ctx, answer(json!({"device": "D2"}))).await.unwrap_err().message.contains("isn't supported"));

        /* D1 isn't on the network (yet): said so, the list stays. */
        let err = session.answer(&ctx, answer(json!({"device": "D1"}))).await.unwrap_err();
        assert!(err.message.contains("isn't on this network"), "{}", err.message);
        seen_vacuum(&hub);
        let view = session.answer(&ctx, answer(json!({"device": "D1"}))).await.unwrap();
        assert_eq!(view.step, StepKind::Test);

        /* What stays: the device's values. What doesn't: code, session. */
        assert_eq!(session.values.plain.get("host").map(String::as_str), Some("192.0.2.5"));
        assert_eq!(session.values.plain.get("duid").map(String::as_str), Some("D1"));
        assert_eq!(session.values.plain.get("email").map(String::as_str), Some("me@example.com"));
        assert!(session.values.secret.contains_key("local_key"));
        assert!(!session.values.plain.contains_key("code"));
        assert!(!session.values.plain.keys().chain(session.values.secret.keys()).any(|k| k.starts_with("login_")));

        let view = session.answer(&ctx, Map::new()).await.unwrap();
        let StepKind::Name { name, summary, .. } = &view.step else { panic!("{view:?}") };
        assert_eq!((name.as_str(), summary.as_str()), ("S7", "S7: charging"));
    }

    /* Issue #74: a login saves the account; the next device of it is
     * picked with a tap -- no email, no code -- and the device refers to
     * the account (never its own copy of the session). */
    #[tokio::test]
    async fn a_saved_account_skips_the_login() {
        let hub = cloud_hub().await;
        seen_vacuum(&hub);
        let ctx = hub.ctx();
        let (mut first, _) = Session::start(&ctx, "roborock-vacuum", None, None).await.unwrap();
        first.answer(&ctx, answer(json!({"email": "me@example.com"}))).await.unwrap();
        first.answer(&ctx, answer(json!({"code": "123456"}))).await.unwrap();
        let saved = crate::accounts::list(hub.control.secrets(), Some("roborock"));
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].session, "session");

        let (mut second, view) = Session::start(&ctx, "roborock-vacuum", None, None).await.unwrap();
        let StepKind::VendorLogin { accounts, .. } = &view.step else { panic!("{view:?}") };
        assert_eq!(accounts, &vec![AccountView { id: "roborock:me@example.com".into(), label: "me@example.com".into() }]);
        let view = second.answer(&ctx, answer(json!({"account": "roborock:me@example.com", "map": "true"}))).await.unwrap();
        assert!(matches!(view.step, StepKind::VendorLogin { phase: LoginPhase::Pick, .. }), "{view:?}");
        second.answer(&ctx, answer(json!({"device": "D1"}))).await.unwrap();
        assert_eq!(second.values.plain.get("account").map(String::as_str), Some("roborock:me@example.com"));
        assert_eq!(second.values.plain.get("map").map(String::as_str), Some("true"));
        assert!(!second.values.secret.keys().any(|k| k.starts_with("login_") || k == "cloud_session"));
        /* Another vendor's account isn't offered, and can't be used. */
        let (mut third, _) = Session::start(&ctx, "roborock-vacuum", None, None).await.unwrap();
        crate::accounts::save(hub.control.secrets(), &crate::accounts::Account { vendor: "ezviz".into(), account: "x".into(), session: "{}".into() });
        assert!(third.answer(&ctx, answer(json!({"account": "ezviz:x"}))).await.is_err());
    }

    /* Issue #74 (EZVIZ): no two-step verification -> no code screen. */
    #[tokio::test]
    async fn the_code_screen_is_skipped_when_not_needed() {
        let hub = cloud_hub().await;
        let ctx = hub.ctx();
        let (mut session, _) = Session::start(&ctx, "roborock-vacuum", None, None).await.unwrap();
        let view = session.answer(&ctx, answer(json!({"email": "nocode@example.com"}))).await.unwrap();
        assert!(matches!(view.step, StepKind::VendorLogin { phase: LoginPhase::Pick, .. }), "{view:?}");
    }

    /* Found on the network first: after the login, the same vacuum is
     * picked by itself. "Restart" goes back to the email. */
    #[tokio::test]
    async fn a_found_device_is_picked_by_itself() {
        let hub = cloud_hub().await;
        seen_vacuum(&hub);
        let ctx = hub.ctx();
        let (mut session, _) = Session::start(&ctx, "roborock-vacuum", None, Some("192.0.2.5")).await.unwrap();
        session.answer(&ctx, answer(json!({"email": "me@example.com"}))).await.unwrap();
        let view = session.answer(&ctx, answer(json!({"restart": true}))).await.unwrap();
        assert!(matches!(view.step, StepKind::VendorLogin { phase: LoginPhase::Account, .. }));
        session.answer(&ctx, answer(json!({"email": "me@example.com"}))).await.unwrap();
        let view = session.answer(&ctx, answer(json!({"code": "123456"}))).await.unwrap();
        assert_eq!(view.step, StepKind::Test);
        assert!(session.values.secret.contains_key("local_key"));
    }
}
