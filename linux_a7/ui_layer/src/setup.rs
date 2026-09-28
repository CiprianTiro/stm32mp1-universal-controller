// setup.rs -- adding and setting up devices on the touchscreen (issue #40):
// what the wizard pages show, and what a tap sends.
//
// The wizard runs in backend_daemon (its wizard.rs); this file keeps only
// what the SCREEN needs between two of its messages: the step on show, the
// fields it asks for and what has been typed into them so far (sent with
// "Next"), which field the keyboard types into, and where "Back" goes when
// there is no earlier step. Like the rest of ui_layer it decides nothing
// about devices -- it draws backend_daemon's step and passes answers on.

use serde_json::{json, Map, Value};
use slint::{ModelRc, VecModel};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::ws_client::{self, Request};
use crate::{AppWindow, ChoiceItem, FieldItem, FoundItem, TemplateItem, VariantItem};

/// The pages (app.slint's `page`).
pub const PAGE_DEVICES: i32 = 0;
pub const PAGE_ADD: i32 = 9;
pub const PAGE_WIZARD: i32 = 10;
pub const PAGE_DEVICE: i32 = 11;

/// One input of the current step.
#[derive(Clone, Debug)]
struct Field {
    id: String,
    label: String,
    hint: String,
    kind: String,
    required: bool,
    choices: Vec<(String, String)>,
}

#[derive(Default)]
pub struct Setup {
    /// What the "Add a device" page offers (list_templates / list_found).
    pub templates: Vec<ws_client::Template>,
    pub found: Vec<ws_client::Found>,
    /// The session backend_daemon runs for us ("" = none).
    session: String,
    template: String,
    variant: String,
    /// The current step's type ("form", "test", ...).
    step: String,
    number: i64,
    title: String,
    fields: Vec<Field>,
    /// What's in each field (by id): prefilled, then as typed.
    values: BTreeMap<String, String>,
    editing: Option<usize>,
    /// Where "Back" on the first step leads: the Add page, or the device's
    /// details (pair again / change settings).
    back_page: i32,
    /// Waiting for backend_daemon's answer: taps are ignored meanwhile.
    busy: bool,
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(items)).into()
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The category ids of backend_daemon's templates.rs, as the menu says them.
fn category_name(id: &str) -> &str {
    match id {
        "lighting" => "Lighting",
        "plugs" => "Plugs",
        "media" => "TV & media",
        "climate" => "Climate",
        "covers" => "Blinds & covers",
        "cameras" => "Cameras",
        "vacuums" => "Vacuums",
        "sensors" => "Sensors",
        "locks" => "Locks",
        "energy" => "Energy",
        _ => "Other",
    }
}

impl Setup {
    pub fn template(&self, id: &str) -> Option<&ws_client::Template> {
        self.templates.iter().find(|t| t.id == id)
    }

    fn template_name(&self, id: &str) -> String {
        self.template(id).map_or_else(|| id.to_string(), |t| t.name.clone())
    }

    /// New lists from backend_daemon: the Add page's rows (and the main
    /// screen's "found" banner, which shows `found`'s length).
    pub fn show_lists(&self, ui: &AppWindow) {
        let templates = self
            .templates
            .iter()
            .map(|t| TemplateItem {
                id: t.id.clone().into(),
                name: t.name.clone().into(),
                category: category_name(&t.category).into(),
                description: t.description.clone().into(),
            })
            .collect();
        ui.set_templates(model(templates));
        ui.set_found(model(self.found_items(None)));
    }

    /// The found devices (optionally only of one template) as rows.
    fn found_items(&self, template: Option<&str>) -> Vec<FoundItem> {
        self.found
            .iter()
            .filter(|f| template.is_none_or(|t| f.template == t))
            .map(|f| FoundItem {
                template: f.template.clone().into(),
                name: f.name.clone().into(),
                address: f.address.clone().into(),
                type_name: self.template_name(&f.template).into(),
            })
            .collect()
    }

    /// Starting: adding a new device (from the Add page).
    pub fn start_new(&mut self, template: &str) {
        self.title = format!("Add: {}", self.template_name(template));
        self.back_page = PAGE_ADD;
        self.busy = true;
    }

    /// Starting: pair again / change settings for an existing device.
    pub fn start_existing(&mut self, title: String) {
        self.title = title;
        self.back_page = PAGE_DEVICE;
        self.busy = true;
    }

    /// A step from backend_daemon: shown, and -- for the steps that only
    /// wait (the test, the device's confirmation) -- answered at once:
    /// the page shows "Testing..." or the countdown until the next message.
    pub fn show_step(&mut self, ui: &AppWindow, tx: &Sender<Request>, step: &Map<String, Value>) {
        let get = |key: &str| step.get(key).cloned().unwrap_or(Value::Null);
        let kind = text(&get("step"));
        self.session = text(&get("session"));
        self.template = text(&get("template"));
        self.variant = text(&get("variant"));
        self.step = kind.clone();
        self.number = get("number").as_i64().unwrap_or(1);
        self.editing = None;
        self.busy = false;
        self.fields.clear();

        let mut body = String::new();
        let mut primary = "Next";
        match kind.as_str() {
            "info" => body = text(&get("text")),
            "form" => {
                if let Some(fields) = get("fields").as_array() {
                    self.fields = fields.iter().map(field_of).collect();
                }
            }
            "choice" | "code_from_device" => self.fields = vec![field_of(&get("field"))],
            "confirm_on_device" => body = text(&get("hint")),
            "name" => {
                body = text(&get("summary"));
                primary = "Add";
                self.fields = vec![
                    Field { id: "name".into(), label: "Name".into(), hint: String::new(), kind: "text".into(), required: true, choices: vec![] },
                    Field { id: "room".into(), label: "Room".into(), hint: "Where it is, e.g. Living room".into(), kind: "text".into(), required: false, choices: vec![] },
                ];
                self.values.insert("name".into(), text(&get("name")));
                self.values.insert("room".into(), text(&get("room")));
            }
            "save" => {
                body = text(&get("summary"));
                primary = "Save";
            }
            _ => {}
        }
        // Prefill from the step (an earlier answer, a found device, the
        // default) -- never for secrets, which never come back.
        for (field, value) in self.fields.iter().zip(field_values(&get, &kind)) {
            if let Some(value) = value {
                self.values.insert(field.id.clone(), value);
            }
        }

        ui.set_wizard_title(self.title.clone().into());
        ui.set_wizard_step(kind.clone().into());
        ui.set_wizard_number(self.number as i32);
        ui.set_wizard_text(body.into());
        ui.set_wizard_primary(primary.into());
        ui.set_wizard_error("".into());
        ui.set_wizard_detail("".into());
        let hints: Vec<slint::SharedString> =
            get("hints").as_array().map_or(vec![], |h| h.iter().map(|v| text(v).into()).collect());
        ui.set_wizard_hints(model(hints));
        ui.set_wizard_seconds(get("timeout_s").as_i64().unwrap_or(60) as i32);
        let found: Vec<FoundItem> = get("found").as_array().map_or(vec![], |list| {
            list.iter()
                .map(|f| FoundItem {
                    template: self.template.clone().into(),
                    name: text(&f["name"]).into(),
                    address: text(&f["address"]).into(),
                    type_name: "".into(),
                })
                .collect()
        });
        ui.set_wizard_found(model(found));
        // The template's other ways in, offered on the discover step.
        let variants: Vec<VariantItem> = self.template(&self.template).map_or(vec![], |t| {
            t.variants
                .iter()
                .filter(|v| v.id != self.variant)
                .map(|v| VariantItem { id: v.id.clone().into(), label: v.label.clone().into() })
                .collect()
        });
        ui.set_wizard_variants(model(variants));
        self.show_fields(ui);
        ui.set_page(PAGE_WIZARD);

        if kind == "test" || kind == "confirm_on_device" {
            self.answer(ui, tx, Map::new());
        }
        ui.set_wizard_busy(self.busy);
    }

    /// The fields as the page draws them.
    fn show_fields(&self, ui: &AppWindow) {
        let items = self
            .fields
            .iter()
            .map(|f| {
                let value = self.values.get(&f.id).cloned().unwrap_or_default();
                let shown = if f.kind == "secret" { "\u{2022}".repeat(value.chars().count()) } else { value.clone() };
                FieldItem {
                    id: f.id.clone().into(),
                    label: f.label.clone().into(),
                    hint: f.hint.clone().into(),
                    kind: f.kind.clone().into(),
                    shown: shown.into(),
                    value: value.into(),
                    required: f.required,
                    choices: model(
                        f.choices
                            .iter()
                            .map(|(value, label)| ChoiceItem { value: value.clone().into(), label: label.clone().into() })
                            .collect(),
                    ),
                }
            })
            .collect();
        ui.set_wizard_fields(model(items));
        ui.set_wizard_editing(self.editing.map_or(-1, |i| i as i32));
        let secret = self.editing.and_then(|i| self.fields.get(i)).is_some_and(|f| f.kind == "secret");
        ui.set_wizard_edit_secret(secret);
    }

    /// A refused answer: the step stays, with the reason. If it's about
    /// one field (a mistyped address), the keyboard opens on it.
    pub fn show_error(&mut self, ui: &AppWindow, field: Option<&str>, message: &str, detail: &str) {
        self.busy = false;
        ui.set_wizard_busy(false);
        if let Some(index) = field.and_then(|f| self.fields.iter().position(|x| x.id == f)) {
            self.edit(ui, index);
        }
        match ui.get_page() {
            PAGE_ADD => ui.set_add_message(message.into()),
            PAGE_DEVICE => ui.set_dev_message(message.into()),
            PAGE_WIZARD => {
                ui.set_wizard_error(message.into());
                ui.set_wizard_detail(detail.into());
            }
            _ => {}
        }
    }

    /// Done: the device added (or updated). Back to the device list.
    pub fn finished(&mut self, ui: &AppWindow, name: &str) {
        let adding = self.back_page == PAGE_ADD;
        *self = Setup {
            templates: std::mem::take(&mut self.templates),
            found: std::mem::take(&mut self.found),
            ..Default::default()
        };
        ui.set_wizard_busy(false);
        ui.set_page(PAGE_DEVICES);
        ui.set_device_message_ok(true);
        ui.set_device_message(if adding { format!("{name} added") } else { format!("{name} saved") }.into());
    }

    fn answer(&mut self, ui: &AppWindow, tx: &Sender<Request>, values: Map<String, Value>) {
        self.busy = true;
        ui.set_wizard_busy(true);
        let _ = tx.send(Request::WizardAnswer { session: self.session.clone(), values });
    }

    // ---- taps on the wizard page ----

    /// The main button: "Next" (the fields' values), "Add", "Save".
    pub fn next(&mut self, ui: &AppWindow, tx: &Sender<Request>) {
        if self.busy {
            return;
        }
        self.edit_done(ui);
        match self.step.as_str() {
            "name" | "save" => {
                let get = |id: &str| self.values.get(id).cloned().unwrap_or_default();
                self.busy = true;
                ui.set_wizard_busy(true);
                let _ = tx.send(Request::WizardFinish { session: self.session.clone(), name: get("name"), room: get("room") });
            }
            _ => {
                let values = self
                    .fields
                    .iter()
                    .map(|f| (f.id.clone(), json!(self.values.get(&f.id).cloned().unwrap_or_default())))
                    .collect();
                self.answer(ui, tx, values);
            }
        }
    }

    /// "Try again" after a failed test or pairing.
    pub fn retry(&mut self, ui: &AppWindow, tx: &Sender<Request>) {
        if !self.busy {
            ui.set_wizard_error("".into());
            ui.set_wizard_detail("".into());
            self.answer(ui, tx, Map::new());
        }
    }

    /// A device from the discover step's list, or "search again" (None).
    pub fn pick_found(&mut self, ui: &AppWindow, tx: &Sender<Request>, address: Option<String>) {
        if self.busy {
            return;
        }
        let mut values = Map::new();
        if let Some(address) = address {
            values.insert("found".into(), json!(address));
        }
        self.answer(ui, tx, values);
    }

    /// The found list changed while the discover step shows: refresh it.
    pub fn found_changed(&mut self, ui: &AppWindow, tx: &Sender<Request>) {
        if ui.get_page() == PAGE_WIZARD && self.step == "discover" && !self.busy {
            self.answer(ui, tx, Map::new());
        }
    }

    /// Tapped a field's box: the keyboard types into it.
    pub fn edit(&mut self, ui: &AppWindow, index: usize) {
        self.edit_done(ui);
        if let Some(field) = self.fields.get(index) {
            self.editing = Some(index);
            ui.set_wizard_edit_text(self.values.get(&field.id).cloned().unwrap_or_default().into());
        }
        self.show_fields(ui);
    }

    /// The keyboard's "Done" (or another field tapped): keep what's typed.
    pub fn edit_done(&mut self, ui: &AppWindow) {
        if let Some(field) = self.editing.and_then(|i| self.fields.get(i)) {
            self.values.insert(field.id.clone(), ui.get_wizard_edit_text().to_string());
        }
        self.editing = None;
        self.show_fields(ui);
    }

    /// A choice (or toggle) tapped. A choice STEP goes on at once.
    pub fn pick(&mut self, ui: &AppWindow, tx: &Sender<Request>, index: usize, value: String) {
        let Some(field) = self.fields.get(index).cloned() else { return };
        self.values.insert(field.id.clone(), value.clone());
        self.show_fields(ui);
        if self.step == "choice" && !self.busy {
            let mut values = Map::new();
            values.insert(field.id, json!(value));
            self.answer(ui, tx, values);
        }
    }

    /// "Back": the previous step, or -- on the first -- leave the wizard.
    pub fn back(&mut self, ui: &AppWindow, tx: &Sender<Request>) {
        if self.editing.is_some() {
            self.editing = None;
            self.show_fields(ui);
            return;
        }
        if self.number <= 1 || self.busy {
            let _ = tx.send(Request::WizardCancel { session: self.session.clone() });
            self.busy = false;
            ui.set_wizard_busy(false);
            ui.set_page(if self.back_page == 0 { PAGE_ADD } else { self.back_page });
            return;
        }
        self.busy = true;
        ui.set_wizard_busy(true);
        let _ = tx.send(Request::WizardBack { session: self.session.clone() });
    }

    /// Another way in (e.g. "Enter the address" instead of searching).
    pub fn switch_variant(&mut self, tx: &Sender<Request>, variant: String) {
        let _ = tx.send(Request::WizardCancel { session: self.session.clone() });
        self.busy = true;
        let _ = tx.send(Request::WizardStart { template: self.template.clone(), variant: Some(variant), found: None });
    }
}

/// A step's field JSON (wizard.rs's Field) -> Field.
fn field_of(value: &Value) -> Field {
    Field {
        id: text(&value["id"]),
        label: text(&value["label"]),
        hint: text(&value["hint"]),
        kind: text(&value["type"]),
        required: value["required"].as_bool().unwrap_or(true),
        choices: value["choices"].as_array().map_or(vec![], |c| {
            c.iter().map(|c| (text(&c["value"]), text(&c["label"]))).collect()
        }),
    }
}

/// The prefilled values of a step's fields, in order.
fn field_values(get: &dyn Fn(&str) -> Value, kind: &str) -> Vec<Option<String>> {
    let one = |f: &Value| f.get("value").map(text);
    match kind {
        "form" => get("fields").as_array().map_or(vec![], |f| f.iter().map(one).collect()),
        "choice" | "code_from_device" => vec![one(&get("field"))],
        _ => vec![],
    }
}
