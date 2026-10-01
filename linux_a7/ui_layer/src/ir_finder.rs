//! The IR code finder (issue #82): finding the codes of a device whose
//! remote is lost, on an IR device's remote page (ir_remote.slint, views
//! 5-8).
//!
//! The hub has a library of known remotes (backend_daemon's ir_library.rs).
//! The person picks the device's type and brand; the hub lists the remotes
//! to try, GROUPED by the code of their test button (Power): many remotes
//! share it, so each group is tried once. Then, one at a time:
//!
//!   view 7  the hub sends a group's Power -> "Did the device react?"
//!           No  -> the next group.   Yes -> view 8 (or add, if the
//!           group's remote has no second button).
//!   view 8  the hub sends a second button of one remote in the group
//!           (its CHECK button: a colour, Mute, ...), which tells the
//!           group's remotes apart -> "And now?"
//!           Yes -> its buttons are added to the device (use_set).
//!           No  -> the group's next remote. None left: the first one is
//!           added anyway (its Power did work), and the message says only
//!           Power was confirmed.
//!
//! This file keeps where we are and turns each answer into the next action
//! for the hub (`try`, `use_set`). The hub's answers come back through
//! main.rs (`Update::DeviceAction`) into `result`.

use serde_json::{json, Value};

use crate::{AppWindow, RemoteItem};

/// The finder's views on IrRemotePage.
pub const VIEW_TYPE: i32 = 5;
pub const VIEW_BRAND: i32 = 6;
pub const VIEW_TEST: i32 = 7;
pub const VIEW_CHECK: i32 = 8;

/// The id of the "Not listed / don't know" row in the brands list. The
/// hub's empty brand means "every brand" (ir_library.rs ANY_BRAND).
const ANY_BRAND: &str = "";

/// One remote of a group, as the hub's `finder` lists it.
struct Choice {
    id: String,
    name: String,
    /// Its test button (the group's code; names may differ: "POWER").
    button: String,
    /// Its second button, if it has one the hub can send.
    check: Option<String>,
}

struct Group {
    choices: Vec<Choice>,
}

/// An action to send for the device on the page: (name, args).
pub type Action = (&'static str, Value);

#[derive(Default)]
pub struct Finder {
    type_id: String,
    type_name: String,
    brand: String,
    groups: Vec<Group>,
    /// The group being tried, and (view 8) its remote.
    group: usize,
    choice: usize,
    /// The remote being added had its second button confirmed (or has
    /// none): false when it's taken on its Power alone.
    confirmed: bool,
}

impl Finder {
    /// "No remote? Find its codes": the list of device types.
    pub fn start(&mut self, ui: &AppWindow, items: &slint::VecModel<RemoteItem>) -> Action {
        *self = Finder::default();
        items.set_vec(Vec::new());
        ui.set_ir_message("".into());
        ui.set_ir_loading(true);
        ui.set_ir_view(VIEW_TYPE);
        ("library", json!({}))
    }

    /// A row of the type or brand list was tapped.
    pub fn pick(&mut self, ui: &AppWindow, items: &slint::VecModel<RemoteItem>, id: &str, label: &str) -> Option<Action> {
        ui.set_ir_message("".into());
        match ui.get_ir_view() {
            VIEW_TYPE => {
                self.type_id = id.to_string();
                self.type_name = label.to_string();
                items.set_vec(Vec::new());
                ui.set_ir_loading(true);
                ui.set_ir_view(VIEW_BRAND);
                Some(("library", json!({"type": self.type_id})))
            }
            VIEW_BRAND => {
                self.brand = id.to_string();
                ui.set_ir_loading(true);
                ui.set_ir_question("".into());
                ui.set_ir_detail("Looking up the codes\u{2026}".into());
                ui.set_ir_view(VIEW_TEST);
                Some(("finder", json!({"type": self.type_id, "brand": self.brand})))
            }
            _ => None,
        }
    }

    /// Yes / No on views 7 and 8.
    pub fn answer(&mut self, ui: &AppWindow, yes: bool) -> Option<Action> {
        ui.set_ir_message("".into());
        let view = ui.get_ir_view();
        let group = self.groups.get(self.group)?;
        match (view, yes) {
            (VIEW_TEST, true) => {
                // Power worked. A second button tells the group's remotes
                // apart; a remote without one is simply taken.
                self.choice = 0;
                match group.choices[0].check {
                    Some(_) => {
                        ui.set_ir_view(VIEW_CHECK);
                        Some(self.send(ui))
                    }
                    None => Some(self.use_choice(ui, 0, true)),
                }
            }
            (VIEW_TEST, false) => {
                self.group += 1;
                if self.group == self.groups.len() {
                    self.give_up(ui);
                    return None;
                }
                Some(self.send(ui))
            }
            (VIEW_CHECK, true) => Some(self.use_choice(ui, self.choice, true)),
            (VIEW_CHECK, false) => {
                // The group's next remote that has a second button.
                let next = (self.choice + 1..group.choices.len()).find(|&i| group.choices[i].check.is_some());
                match next {
                    Some(i) => {
                        self.choice = i;
                        Some(self.send(ui))
                    }
                    None => {
                        // Power did work: take the group's first remote and
                        // say only Power is sure.
                        Some(self.use_choice(ui, 0, false))
                    }
                }
            }
            _ => None,
        }
    }

    /// "Send again": the same button once more (missed, or too far).
    pub fn resend(&self, ui: &AppWindow) -> Option<Action> {
        self.groups.get(self.group)?;
        ui.set_ir_message("".into());
        Some(self.send(ui))
    }

    /// The hub's answer to one of the finder's actions. Returns the next
    /// action to send, if any.
    pub fn result(
        &mut self,
        ui: &AppWindow,
        items: &slint::VecModel<RemoteItem>,
        name: &str,
        args: &Value,
        result: &Result<Value, String>,
    ) -> Option<Action> {
        ui.set_ir_loading(false);
        let result = match result {
            Ok(result) => result,
            Err(why) => {
                // Shown where the person is; "Send again" retries a try.
                ui.set_ir_message_ok(false);
                ui.set_ir_message(why.as_str().into());
                return None;
            }
        };
        match name {
            "library" if args.get("type").is_none() => {
                items.set_vec(list(&result["types"], |t| (text(&t["id"]), text(&t["name"]), String::new())));
                None
            }
            "library" => {
                let mut rows = vec![RemoteItem {
                    id: ANY_BRAND.into(),
                    label: "Not listed / don't know".into(),
                    detail: "all brands".into(),
                }];
                rows.extend(list(&result["brands"], |b| {
                    let sets = b["sets"].as_u64().unwrap_or(0);
                    (text(&b["name"]), text(&b["name"]), if sets == 1 { "1 remote".into() } else { format!("{sets} remotes") })
                }));
                items.set_vec(rows);
                None
            }
            "finder" => {
                self.groups = groups(&result["candidates"]);
                self.group = 0;
                if self.groups.is_empty() {
                    self.give_up(ui);
                    return None;
                }
                Some(self.send(ui))
            }
            "use_set" => {
                let added = result["added"].as_u64().unwrap_or(0);
                let only_power = !self.confirmed;
                let choice = self.groups.get(self.group).and_then(|g| g.choices.get(self.choice));
                let from = choice.map_or_else(String::new, |c| c.name.clone());
                ui.set_ir_message_ok(true);
                ui.set_ir_message(
                    match (added, only_power) {
                        (0, _) => format!("The device already has the buttons of \u{201C}{from}\u{201D}."),
                        (_, false) => format!("Added {added} buttons from \u{201C}{from}\u{201D}. Tap one to try it."),
                        (_, true) => format!(
                            "Added {added} buttons from \u{201C}{from}\u{201D}. Only Power was confirmed: \
                             try the others, and delete the ones that do nothing."
                        ),
                    }
                    .into(),
                );
                ui.set_ir_view(0);
                None
            }
            // "try": sent. The question is already on screen.
            _ => None,
        }
    }

    /// Sends the current button (view 7: the group's test button; view 8:
    /// the current remote's check button) and says so on screen.
    fn send(&self, ui: &AppWindow) -> Action {
        let group = &self.groups[self.group];
        let checking = ui.get_ir_view() == VIEW_CHECK;
        let choice = &group.choices[if checking { self.choice } else { 0 }];
        let button = if checking { choice.check.clone().unwrap_or_default() } else { choice.button.clone() };
        if checking {
            ui.set_ir_question("Did it react again?".into());
            ui.set_ir_detail(
                format!(
                    "Sent \u{201C}{button}\u{201D} from \u{201C}{}\u{201D} (remote {} of {} that share this Power code).",
                    choice.name,
                    self.choice + 1,
                    group.choices.len()
                )
                .into(),
            );
        } else {
            ui.set_ir_question("Did the device react?".into());
            ui.set_ir_detail(
                format!(
                    "Sent \u{201C}{button}\u{201D}: code {} of {}. Point the IR blaster at the device, from close by.",
                    self.group + 1,
                    self.groups.len()
                )
                .into(),
            );
        }
        ("try", json!({"type": self.type_id, "set": choice.id, "button": button}))
    }

    fn use_choice(&mut self, ui: &AppWindow, choice: usize, confirmed: bool) -> Action {
        self.choice = choice;
        self.confirmed = confirmed;
        ui.set_ir_loading(true);
        let id = self.groups[self.group].choices[choice].id.clone();
        ("use_set", json!({"type": self.type_id, "set": id}))
    }

    /// Every code was tried, or there were none.
    fn give_up(&self, ui: &AppWindow) {
        let brand = if self.brand.is_empty() { String::new() } else { format!(" for {}", self.brand) };
        ui.set_ir_message_ok(false);
        ui.set_ir_message(
            format!(
                "No known code worked{brand}. You can still teach the buttons from a cheap replacement \
                 remote for this brand: tap \u{201C}+ Teach a button\u{201D}."
            )
            .into(),
        );
        ui.set_ir_view(0);
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn list(value: &Value, parts: impl Fn(&Value) -> (String, String, String)) -> Vec<RemoteItem> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let (id, label, detail) = parts(item);
                    RemoteItem { id: id.into(), label: label.into(), detail: detail.into() }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The hub's `candidates` -> groups.
fn groups(candidates: &Value) -> Vec<Group> {
    candidates
        .as_array()
        .map(|groups| {
            groups
                .iter()
                .map(|g| Group {
                    choices: g["sets"]
                        .as_array()
                        .map(|sets| {
                            sets.iter()
                                .map(|s| Choice {
                                    id: text(&s["id"]),
                                    name: text(&s["name"]),
                                    button: text(&s["button"]),
                                    check: s["check"].as_str().map(str::to_string),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .filter(|g| !g.choices.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
