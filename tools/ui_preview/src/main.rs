// ui-preview -- renders every page of the hub UI to PNG files, at the
// screen sizes the hub runs at (issue #38), with made-up sample data.
//
//     cargo run --release --manifest-path tools/ui_preview/Cargo.toml -- <folder> [WxH ...] [--preset mode,accent,density]
//
// --preset (issue #39): the design preset to render, e.g.
// light,emerald,compact; default: tokens.json's defaults. The file names
// then end in the preset, e.g. 1280x720-devices-light-emerald-compact.png.
//
// Default sizes: 480x800 (the DK2's touchscreen, portrait), 1280x720 (an
// HDMI monitor) and 800x600 (a monitor's fallback mode). Files are named
// <size>-<page>.png, e.g. 1280x720-devices.png.
//
// How it works: instead of a display, the UI gets a "window" that only
// exists in memory (MinimalSoftwareWindow); Slint's software renderer draws
// each frame into a pixel buffer, which is saved as a PNG.

use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{PhysicalSize, VecModel};
use std::rc::Rc;

slint::include_modules!();

// The UI's own preset code, so the preview applies presets exactly as the
// hub does.
#[path = "../../../linux_a7/ui_layer/src/theme.rs"]
mod theme;
// The UI's time zone lists (issue #39). Their unit tests (and theme.rs's)
// run here with `cargo test`: the UI crate itself can't link on the PC
// without libinput's development files.
#[path = "../../../linux_a7/ui_layer/src/zones.rs"]
mod zones;
// The IR remote's layout (issue #82), as the hub's UI makes it.
#[path = "../../../linux_a7/ui_layer/src/ir_layout.rs"]
mod ir_layout;
// Automations in words, and the simple editor's JSON (issue #47): its
// unit tests run here (`cargo test`).
#[path = "../../../linux_a7/ui_layer/src/automation_text.rs"]
#[allow(dead_code)]
mod automation_text;
// The Devices page's tiles, chips and groups (issue #95), on the devices
// as backend_daemon reports them (ws_client.rs's types): its unit tests
// run here too.
#[path = "../../../linux_a7/ui_layer/src/ws_client.rs"]
#[allow(dead_code)]
mod ws_client;
// Issue #74: the vacuum map renderer.
#[path = "../../../linux_a7/ui_layer/src/vacuum_map.rs"]
mod vacuum_map;
#[path = "../../../linux_a7/ui_layer/src/tiles.rs"]
mod tiles;

thread_local! {
    /// The sample devices (issue #95: as backend_daemon sends them).
    static DEVICES: std::cell::RefCell<Vec<ws_client::Device>> = Default::default();
}

/// Sets the window size.
fn set_size(window: &MinimalSoftwareWindow, width: u32, height: u32) {
    window.set_size(PhysicalSize::new(width, height));
}

/// The Devices page's chips and tiles for `filter`, at the current size --
/// built by tiles.rs exactly as ui_layer's main.rs does.
fn set_tiles(ui: &AppWindow, filter: &str) {
    let columns = ui.get_tile_columns().max(1) as usize;
    let accent = ui.global::<Theme>().get_accent();
    DEVICES.with(|d| {
        let devices = d.borrow();
        let all: Vec<&ws_client::Device> = devices.iter().collect();
        ui.set_filter_chips(Rc::new(VecModel::from(tiles::chips(&all, &[]))).into());
        ui.set_device_filter(filter.into());
        let mut rows = Vec::new();
        for (heading, group) in tiles::groups(&all, filter, &[], accent, 1_790_000_000) {
            if !heading.is_empty() {
                rows.push(TileRow { header: heading.into(), tiles: Default::default() });
            }
            for chunk in group.chunks(columns) {
                rows.push(TileRow { header: "".into(), tiles: Rc::new(VecModel::from(chunk.to_vec())).into() });
            }
        }
        ui.set_tile_rows(Rc::new(VecModel::from(rows)).into());
    });
}

/// Issue #95: the Devices page with each kind of chip selected, and one
/// device's controls page.
const DEVICE_VIEWS: [(&str, &str); 4] =
    [("devices", "all"), ("devices-favourites", "fav"), ("devices-lights", "lights"), ("devices-room", "room:Living room")];

/// The pages worth looking at, with the number app.slint uses for each.
/// None: the welcome screen (shown until the backend first answers).
const PAGES: [(&str, Option<i32>); 18] = [
    ("welcome", None),
    // Issue #95: the device's controls page (the Devices page itself is
    // in DEVICE_VIEWS).
    ("controls", Some(20)),
    ("network", Some(1)),
    ("password", Some(2)),
    ("country", Some(3)),
    ("settings", Some(4)),
    ("clients", Some(5)),
    ("pairing", Some(6)),
    ("appearance", Some(7)),
    ("timezone", Some(8)),
    ("add-device", Some(9)),
    ("device", Some(11)),
    // Issue #47: the scenes, and "Save current state" (its first view).
    ("scenes", Some(14)),
    ("new-scene", Some(15)),
    // Automations: the list, the log, the editor (its first view), the
    // location.
    ("automations", Some(16)),
    ("log", Some(17)),
    ("new-automation", Some(18)),
    ("location", Some(19)),
];

/// The wizard page (10) in each kind of step (issue #40): the step's
/// sample data is set just before its render.
const WIZARD_STEPS: [(&str, fn(&AppWindow)); 35] = [
    // The remote (issue #44), page 12, in its views.
    ("remote", |ui| {
        ui.set_page(12);
        ui.set_remote_view(0);
    }),
    ("remote-channels", |ui| {
        ui.set_page(12);
        ui.set_remote_view(2);
        let items: Vec<RemoteItem> = [("1", "TVR 1"), ("2", "TVR 2"), ("5", "Pro TV"), ("7", "Antena 1"), ("12", "Digi 24")]
            .iter()
            .map(|&(n, name)| RemoteItem { id: n.into(), label: n.into(), detail: name.into() })
            .collect();
        ui.set_remote_items(Rc::new(VecModel::from(items)).into());
        ui.set_remote_total(412);
    }),
    ("remote-search", |ui| {
        ui.set_page(12);
        ui.set_remote_view(2);
        ui.set_remote_query("pro".into());
        let items: Vec<RemoteItem> = [("8", "Pro TV"), ("70", "Pro Cinema"), ("71", "Pro Arena")]
            .iter()
            .map(|&(n, name)| RemoteItem { id: n.into(), label: n.into(), detail: name.into() })
            .collect();
        ui.set_remote_items(Rc::new(VecModel::from(items)).into());
        ui.set_remote_total(3);
    }),
    ("remote-keyboard", |ui| {
        ui.set_page(12);
        ui.set_remote_view(3);
        ui.set_remote_text("stranger thi".into());
    }),
    // An IR device's remote (issue #42), page 13, in its views.
    ("ir-remote-empty", |ui| {
        ir_remote(ui, &[], 0);
    }),
    ("ir-remote", |ui| {
        ir_remote(ui, &["Power", "Brighter", "Dimmer", "Red", "Green", "Blue", "White", "Flash", "Smooth", "Speed +"], 0);
        ui.set_ir_message_ok(true);
        ui.set_ir_message("Learned \u{201C}Speed +\u{201D}. Tap it to try it.".into());
    }),
    ("ir-remote-offline", |ui| {
        ir_remote(ui, &["Power", "Red"], 0);
        ui.set_ir_status("The IR blaster is offline: buttons can't be sent or taught right now. Is it plugged in and on the WiFi?".into());
    }),
    // Issue #85: "Use as a light", with the strip's colour order.
    ("ir-light", |ui| {
        ir_remote(ui, &["power", "red", "green", "blue", "Brighter", "Dimmer"], 9);
        ui.set_ir_light(true);
        ui.set_ir_order("GRB".into());
        ui.set_ir_message_ok(true);
        ui.set_ir_message("On its card: on/off, 3 colours, brightness \u{2212} / +.".into());
    }),
    ("ir-edit", |ui| {
        ir_remote(ui, &["Power", "Brighter", "Dimmer", "Red"], 1);
    }),
    ("ir-name", |ui| {
        ir_remote(ui, &["Power"], 2);
        ui.set_ir_text("Brigh".into());
    }),
    ("ir-waiting", |ui| {
        ir_remote(ui, &["Power"], 3);
        ui.set_ir_selected("Brighter".into());
    }),
    // The code finder for a lost remote (issue #82), views 5-8.
    ("ir-find-type", |ui| {
        ir_remote(ui, &[], 5);
        ir_items(ui, &[("led_lighting", "LED lights and strips", ""), ("tv", "TVs", ""), ("projector", "Projectors", ""), ("fan", "Fans", ""), ("heater", "Heaters", ""), ("soundbar", "Soundbars", "")]);
    }),
    ("ir-find-brand", |ui| {
        ir_remote(ui, &[], 6);
        ir_items(ui, &[
            ("", "Not listed / don't know", "all brands"),
            ("Generic (no brand)", "Generic (no brand)", "34 remotes"),
            ("Govee", "Govee", "4 remotes"),
            ("Philips", "Philips", "3 remotes"),
            ("Sylvania", "Sylvania", "1 remote"),
        ]);
    }),
    ("ir-find-test", |ui| {
        ir_remote(ui, &[], 7);
        ui.set_ir_question("Did the device react?".into());
        ui.set_ir_detail("Sent \u{201C}POWER\u{201D}: code 1 of 88. Point the IR blaster at the device, from close by.".into());
    }),
    ("ir-find-check", |ui| {
        ir_remote(ui, &[], 8);
        ui.set_ir_question("Did it react again?".into());
        ui.set_ir_detail("Sent \u{201C}Red\u{201D} from \u{201C}LED 44Key\u{201D} (remote 1 of 7 that share this Power code).".into());
    }),
    ("ir-tv-layout", |ui| {
        let buttons = [
            "Power", "HOME", "BACK", "MENU", "UP", "LEFT", "OK", "RIGHT", "DOWN", "VOLUME_DOWN", "MUTE", "VOLUME_UP",
            "CHANNEL_DOWN", "CHANNEL_UP", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "Netflix", "Input", "Subtitle",
        ];
        ir_remote(ui, &buttons, 0);
        ui.set_remote_name("Living room TV".into());
    }),
    ("ir-find-done", |ui| {
        ir_remote(ui, &["POWER", "Light UP", "Light DOWN", "Play / Pause", "Red", "Green", "Blue", "White", "Flash", "Fade 3", "Jump 7", "Quick"], 0);
        ui.set_ir_message_ok(true);
        ui.set_ir_message("Added 43 buttons from \u{201C}LED 44Key\u{201D}. Tap one to try it.".into());
    }),
    // The IR blaster's pairing code (issue #42): a secret field, hidden,
    // then shown while typing.
    ("wizard-code", |ui| {
        wizard(ui, "Add: IR remote device", "code_from_device", 3, "", false);
        ui.set_wizard_fields(secret_field("5MCN-WM19-04CH-Q2GT"));
        ui.set_wizard_reveal(false);
    }),
    ("wizard-code-shown", |ui| {
        wizard(ui, "Add: IR remote device", "code_from_device", 3, "", false);
        ui.set_wizard_fields(secret_field(""));
        ui.set_wizard_editing(0);
        ui.set_wizard_edit_secret(true);
        ui.set_wizard_edit_text("5MCN-WM19-04C".into());
        ui.set_wizard_reveal(true);
    }),
    // A new IR blaster being set up over Bluetooth (issue #42).
    ("wizard-provision", |ui| {
        wizard(ui, "Add: IR remote device", "provision_ble", 3,
               "Setting up the blaster: WiFi over Bluetooth, then pairing. This takes up to a minute.", true);
        ui.set_wizard_seconds(150);
    }),
    ("wizard-discover", |ui| {
        wizard(ui, "Add: WLED light", "discover", 1, "", false);
        ui.set_wizard_found(Rc::new(VecModel::from(vec![
            FoundItem { name: "WLED-Desk".into(), address: "192.168.1.139".into(), ..Default::default() },
            FoundItem { name: "wled-kitchen".into(), address: "192.168.1.61".into(), ..Default::default() },
        ])).into());
        ui.set_wizard_variants(Rc::new(VecModel::from(vec![
            VariantItem { id: "advanced".into(), label: "Enter the address".into() },
        ])).into());
    }),
    ("wizard-form", |ui| {
        wizard(ui, "Add: LG TV (webOS)", "form", 1, "", false);
        ui.set_wizard_fields(fields(&[
            ("host", "TV address", "Settings > Network on the TV shows it, e.g. 192.168.1.40", "192.168.1.140", true),
            ("mac", "TV MAC address", "Needed to switch the TV on; read from the TV after pairing if left empty", "", false),
        ]));
    }),
    ("wizard-typing", |ui| {
        wizard(ui, "Add: WLED light", "form", 1, "", false);
        ui.set_wizard_fields(fields(&[("host", "Address", "The IP address shown in the WLED app, e.g. 192.168.1.50", "", true)]));
        ui.set_wizard_editing(0);
        ui.set_wizard_edit_text("192.168.1.13".into());
        ui.set_wizard_error("Address: an IP address like 192.168.1.50, or a name like wled.local.".into());
    }),
    // Issue #74: four fields and the keyboard: only the one typed into.
    ("wizard-typing-many", |ui| {
        wizard(ui, "Settings: Tapo camera", "form", 1, "", false);
        ui.set_wizard_fields(fields(&[
            ("host", "Camera address", "The Tapo app shows it under the camera's Device Info", "192.168.1.133", true),
            ("password", "Tapo account password", "The password of the Tapo app account", "", true),
            ("stream_user", "Camera Account user name (for video)", "Optional: set in the Tapo app", "", false),
            ("stream_password", "Camera Account password", "Optional, with the user name above", "", false),
        ]));
        ui.set_wizard_editing(2);
        ui.set_wizard_edit_text("hubcam".into());
    }),
    ("wizard-confirm", |ui| {
        wizard(ui, "Add: LG TV (webOS)", "confirm_on_device", 2, "A prompt appears on the TV: accept it with the remote.", true);
        ui.set_wizard_hints(Rc::new(VecModel::from(vec![slint::SharedString::from(
            "Can't see it? The TV may be showing another input: press Home.",
        )])).into());
        ui.set_wizard_seconds(60);
    }),
    ("wizard-failed", |ui| {
        wizard(ui, "Add: WLED light", "test", 2, "", false);
        ui.set_wizard_error("The hub can't reach the device. Is it on and on the same network?".into());
        ui.set_wizard_detail("can't reach 192.168.1.250: No route to host (os error 113)".into());
    }),
    // Issue #74: a vendor account login, its three screens.
    ("wizard-login", |ui| {
        wizard(ui, "Add: Roborock vacuum", "login", 1,
               "Roborock emails you a code. The hub uses the account once, to fetch the vacuum's key; after that it controls the vacuum on your network only, also without internet. Your password is never asked.", false);
        ui.set_wizard_fields(fields(&[("email", "Roborock account email", "The email you log in to the Roborock app with", "ciprian@example.com", true)]));
    }),
    ("wizard-login-saved", |ui| {
        wizard(ui, "Add: EZVIZ smart plug", "login", 1,
               "EZVIZ plugs only work through EZVIZ's cloud: this plug will need internet. The hub keeps a login session, never your password.", false);
        ui.set_wizard_found(Rc::new(VecModel::from(vec![
            FoundItem { name: "ciprian@example.com".into(), address: "ezviz:ciprian@example.com".into(), type_name: "Signed in: use it".into(), ..Default::default() },
        ])).into());
        ui.set_wizard_fields(fields(&[
            ("email", "EZVIZ account email (or phone)", "What you log in to the EZVIZ app with", "", true),
            ("password", "EZVIZ password", "Used once to log in; the hub doesn't keep it", "", true),
        ]));
    }),
    ("accounts", |ui| {
        ui.set_page(21);
        ui.set_accounts(Rc::new(VecModel::from(vec![
            AccountItem { id: "roborock:c".into(), vendor: "Roborock".into(), account: "ciprian@example.com".into(), devices_text: "Used by: S7".into() },
            AccountItem { id: "ezviz:c".into(), vendor: "EZVIZ".into(), account: "ciprian@example.com".into(), devices_text: "No devices use it".into() },
        ])).into());
    }),
    ("wizard-login-code", |ui| {
        wizard(ui, "Add: Roborock vacuum", "login_code", 1, "Roborock sent you a code. Type it here.", false);
        ui.set_wizard_fields(fields(&[("code", "Code from the email", "6 digits, from Roborock's email", "", true)]));
        ui.set_wizard_error("That code isn't right (or it expired). Check the email, or ask for a new code.".into());
    }),
    ("wizard-login-pick", |ui| {
        wizard(ui, "Add: Roborock vacuum", "login_pick", 1, "Which device of your Roborock account is it?", false);
        ui.set_wizard_found(Rc::new(VecModel::from(vec![
            FoundItem { name: "S7".into(), address: "1tV5069KevcrlKa4Snsyyq".into(), type_name: "Roborock S7 - online".into(), ..Default::default() },
            FoundItem { name: "Q7 Max".into(), address: "x".into(), type_name: "Roborock Q7 Max: its protocol (B01) isn't supported yet".into(), ..Default::default() },
        ])).into());
    }),
    // Issue #74: a vacuum's controls page.
    ("controls-ezviz", |ui| {
        ui.set_page(20);
        let plug: ws_client::Device = serde_json::from_value(serde_json::json!(
            {"id": "priza", "name": "Priza EzViz", "room": "Office", "template": "ezviz-plug", "online": "online",
             "capabilities": {"switch": {"on": true}, "energy": {"power_w": 8.4, "today_kwh": 0.12, "energy_kwh": 12.3}}})).unwrap();
        ui.set_controls_tile(tiles::tile(&plug, &[], ui.global::<Theme>().get_accent(), 1_790_000_000));
        ui.set_controls_item(DeviceItem {
            id: "priza".into(), name: "Priza EzViz".into(), room: "Office".into(), has_switch: true, on: true,
            energy_text: "8.4 W \u{2022} today 0.12 kWh \u{2022} 12.30 kWh".into(),
            options: Rc::new(VecModel::from(vec![
                OptionItem { id: "status_light".into(), name: "Status light".into(), on: true, ..Default::default() },
                OptionItem { id: "power_recovery".into(), name: "Restore after a power cut".into(), on: false, ..Default::default() },
            ])).into(),
            ..Default::default()
        });
    }),
    ("controls-camera", |ui| {
        ui.set_page(20);
        let camera: ws_client::Device = serde_json::from_value(serde_json::json!(
            {"id": "cam", "name": "Living room camera", "room": "Living room", "template": "tapo-camera", "online": "online",
             "capabilities": {"switch": {"on": true}}})).unwrap();
        let templates: Vec<ws_client::Template> = vec![serde_json::from_value(serde_json::json!(
            {"id": "tapo-camera", "name": "Tapo camera", "category": "cameras", "variants": []})).unwrap()];
        ui.set_controls_tile(tiles::tile(&camera, &templates, ui.global::<Theme>().get_accent(), 1_790_000_000));
        let modes = |values: &[&str]| -> slint::ModelRc<ModeItem> {
            Rc::new(VecModel::from(values.iter().map(|v| ModeItem { value: (*v).into(), label: (v[..1].to_uppercase() + &v[1..]).into() }).collect::<Vec<_>>())).into()
        };
        let toggle = |id: &str, name: &str, on: bool| OptionItem { id: id.into(), name: name.into(), on, ..Default::default() };
        // A picture, if one is given (CAMERA_JPEG: a JPEG file).
        match std::env::var("CAMERA_JPEG").ok().and_then(|f| std::fs::read(f).ok()) {
            Some(jpeg) => {
                use base64::Engine;
                ui.set_controls_map(vacuum_map::picture(&base64::engine::general_purpose::STANDARD.encode(jpeg)).expect("a JPEG"));
                ui.set_controls_map_state("".into());
                ui.set_controls_live(true);
            }
            None => ui.set_controls_map_state("Loading the picture\u{2026}".into()),
        }
        ui.set_controls_item(DeviceItem {
            id: "cam".into(), name: "Living room camera".into(), room: "Living room".into(), has_switch: true, on: true, has_camera: true,
            camera_ptz: true,
            camera_presets: Rc::new(VecModel::from(vec![ModeItem { value: "1".into(), label: "Door".into() }, ModeItem { value: "2".into(), label: "Window".into() }])).into(),
            options: Rc::new(VecModel::from(vec![
                toggle("motion_detection", "Motion detection", true),
                toggle("alarm", "Alarm on motion (siren and light)", false),
                toggle("status_led", "Status light", true),
                OptionItem { id: "night_vision".into(), name: "Night vision".into(), value: "auto".into(), choices: modes(&["auto", "on", "off"]), ..Default::default() },
            ])).into(),
            ..Default::default()
        });
    }),
    ("controls-vacuum", |ui| {
        ui.set_page(20);
        let vacuum: ws_client::Device = serde_json::from_value(serde_json::json!(
            {"id": "s7", "name": "S7", "room": "Living room", "template": "roborock-vacuum", "online": "online",
             "capabilities": {"vacuum": {"state": "cleaning", "battery": 84, "detail": "room cleaning",
                "fan": "balanced", "fans": ["off", "quiet", "balanced", "turbo", "max"],
                "water": "medium", "waters": ["off", "low", "medium", "high"], "mop": "standard", "mops": ["standard", "deep"],
                "area_m2": 14.5, "minutes": 21,
                "rooms": [{"id": 16, "name": "Cameră de zi"}, {"id": 17, "name": "Kitchen"}],
                "parts": [{"id": "main_brush", "name": "Main brush", "left": 60}, {"id": "side_brush", "name": "Side brush", "left": 41},
                          {"id": "filter", "name": "Filter", "left": 21}, {"id": "sensors", "name": "Sensors (clean them)", "left": 0}],
                "totals": {"cleanings": 272, "area_m2": 6977.2, "hours": 117.7}, "quiet_hours": "22:00-08:00"}}})).unwrap();
        ui.set_controls_tile(tiles::tile(&vacuum, &[], ui.global::<Theme>().get_accent(), 1_790_000_000));
        let v = vacuum.capabilities.vacuum.as_ref().unwrap();
        let modes = |values: &[&str], labels: &[&str]| -> slint::ModelRc<ModeItem> {
            Rc::new(VecModel::from(values.iter().zip(labels).map(|(v, l)| ModeItem { value: (*v).into(), label: (*l).into() }).collect::<Vec<_>>())).into()
        };
        ui.set_controls_item(DeviceItem {
            id: "s7".into(), name: "S7".into(), room: "Living room".into(),
            has_vacuum: true, vacuum_state: "cleaning".into(), vacuum_text: tiles::vacuum_line(v).into(),
            vacuum_fan: "balanced".into(), vacuum_fans: modes(&["off", "quiet", "balanced", "turbo", "max"], &["Off", "Quiet", "Balanced", "Turbo", "Max"]),
            vacuum_water: "medium".into(), vacuum_waters: modes(&["off", "low", "medium", "high"], &["Off", "Low", "Medium", "High"]),
            vacuum_mop: "standard".into(), vacuum_mops: modes(&["standard", "deep"], &["Standard", "Deep"]),
            vacuum_run_text: tiles::vacuum_run(v).into(),
            vacuum_rooms: Rc::new(VecModel::from(vec![VacuumRoomItem { id: 16, name: "Cameră de zi".into() }, VacuumRoomItem { id: 17, name: "Kitchen".into() }])).into(),
            vacuum_parts: Rc::new(VecModel::from(v.parts.iter().map(|p| VacuumPartItem { id: p.id.clone().into(), name: p.name.clone().into(), left: p.left as i32 }).collect::<Vec<_>>())).into(),
            vacuum_totals_text: "272 cleanings \u{2022} 6977 m² \u{2022} 118 h in all".into(),
            vacuum_quiet_text: "Do not disturb 22:00-08:00".into(),
            vacuum_has_map: true,
            ..Default::default()
        });
        // A real map, if one is given (VACUUM_MAP_JSON: a "map" action's
        // answer saved to a file -- never committed, it's someone's home).
        match std::env::var("VACUUM_MAP_JSON").ok().and_then(|f| std::fs::read_to_string(f).ok()) {
            Some(text) => {
                let map: serde_json::Value = serde_json::from_str(&text).unwrap();
                let theme = ui.global::<Theme>();
                let colors = vacuum_map::Colors {
                    floor: theme.get_surface_pressed(),
                    wall: theme.get_text_muted(),
                    path: theme.get_text(),
                    robot: theme.get_accent(),
                    dock: theme.get_ok(),
                    no_go: theme.get_error(),
                };
                ui.set_controls_map(vacuum_map::render(&map, &colors).expect("a map"));
                ui.set_controls_map_state("".into());
            }
            None => ui.set_controls_map_state("Loading the map\u{2026}".into()),
        }
    }),
    ("wizard-name", |ui| {
        wizard(ui, "Add: WLED light", "name", 3, "WLED 16.0.1", false);
        ui.set_wizard_primary("Add".into());
        ui.set_wizard_fields(fields(&[
            ("name", "Name", "", "LED strip", true),
            ("room", "Room", "Where it is, e.g. Living room", "Office", false),
        ]));
    }),
];

/// The IR remote page (issue #42) with `buttons`, in `view`.
fn ir_remote(ui: &AppWindow, buttons: &[&str], view: i32) {
    ui.set_page(13);
    ui.set_remote_name("Astronaut".into());
    let buttons: Vec<slint::SharedString> = buttons.iter().map(|&b| b.into()).collect();
    let names: Vec<String> = buttons.iter().map(|b| b.to_string()).collect();
    let (rows, others) = ir_layout::layout(&names);
    ui.set_ir_layout(Rc::new(VecModel::from(rows)).into());
    let others: Vec<slint::SharedString> = others.into_iter().map(Into::into).collect();
let labelled: Vec<IrKeyItem> =
        names.iter().map(|n| IrKeyItem { name: n.as_str().into(), label: ir_layout::label(n).into() }).collect();
    ui.set_ir_labelled(std::rc::Rc::new(slint::VecModel::from(labelled)).into());
        ui.set_ir_others(Rc::new(VecModel::from(others)).into());
    ui.set_ir_buttons(Rc::new(VecModel::from(buttons)).into());
    ui.set_ir_status("".into());
    ui.set_ir_message("".into());
    ui.set_ir_text("".into());
    ui.set_ir_loading(false);
    ui.set_ir_view(view);
}

/// The finder's type or brand list (issue #82): (id, label, detail).
fn ir_items(ui: &AppWindow, rows: &[(&str, &str, &str)]) {
    let rows: Vec<RemoteItem> =
        rows.iter().map(|(id, label, detail)| RemoteItem { id: (*id).into(), label: (*label).into(), detail: (*detail).into() }).collect();
    ui.set_ir_items(Rc::new(VecModel::from(rows)).into());
    ui.set_ir_loading(false);
}

/// Resets the wizard page to one step.
fn wizard(ui: &AppWindow, title: &str, step: &str, number: i32, text: &str, busy: bool) {
    ui.set_wizard_title(title.into());
    ui.set_wizard_step(step.into());
    ui.set_wizard_number(number);
    ui.set_wizard_text(text.into());
    ui.set_wizard_busy(busy);
    ui.set_wizard_error("".into());
    ui.set_wizard_detail("".into());
    ui.set_wizard_editing(-1);
    ui.set_wizard_primary("Next".into());
    ui.set_wizard_fields(Rc::new(VecModel::from(Vec::<FieldItem>::new())).into());
}

/// The IR blaster's pairing code field (a secret) holding `value`.
fn secret_field(value: &str) -> slint::ModelRc<FieldItem> {
    let item = FieldItem {
        id: "pairing_code".into(),
        label: "Pairing code".into(),
        hint: "On the blaster's label, or type `pairing` in its console: XXXX-XXXX-XXXX-XXXX (small letters are fine)".into(),
        kind: "secret".into(),
        shown: "\u{2022}".repeat(value.chars().count()).into(),
        value: value.into(),
        required: true,
        ..Default::default()
    };
    Rc::new(VecModel::from(vec![item])).into()
}

/// Text fields: (id, label, hint, value, required).
fn fields(list: &[(&str, &str, &str, &str, bool)]) -> slint::ModelRc<FieldItem> {
    let items: Vec<FieldItem> = list
        .iter()
        .map(|&(id, label, hint, value, required)| FieldItem {
            id: id.into(),
            label: label.into(),
            hint: hint.into(),
            kind: "text".into(),
            shown: value.into(),
            value: value.into(),
            required,
            ..Default::default()
        })
        .collect();
    Rc::new(VecModel::from(items)).into()
}

/// Slint needs a Platform before any window exists; this one hands out the
/// in-memory window. The event loop is never run (frames are drawn by hand).
struct PreviewPlatform(Rc<MinimalSoftwareWindow>);

impl Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut appearance = theme::Appearance::default();
    let mut suffix = String::new();
    if let Some(i) = args.iter().position(|a| a == "--preset") {
        let spec = args.get(i + 1).expect("--preset needs mode,accent,density").clone();
        let parts: Vec<&str> = spec.split(',').collect();
        assert_eq!(parts.len(), 3, "--preset looks like light,emerald,compact");
        appearance = theme::Appearance { mode: parts[0].into(), accent: parts[1].into(), density: parts[2].into() };
        suffix = format!("-{}", parts.join("-"));
        args.drain(i..i + 2);
    }
    let Some(out_dir) = args.first() else {
        eprintln!("usage: ui-preview <folder> [WxH ...]");
        std::process::exit(2);
    };
    let sizes: Vec<(u32, u32)> = if args.len() > 1 {
        args[1..].iter().map(|s| parse_size(s)).collect()
    } else {
        vec![(480, 800), (1280, 720), (800, 600)]
    };
    std::fs::create_dir_all(out_dir).expect("can't create the output folder");

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PreviewPlatform(window.clone()))).unwrap();
    let ui = AppWindow::new().unwrap();
    // "auto" is rendered as it looks at noon.
    theme::apply(&ui, &appearance, theme::is_dark(&appearance.mode, 12));
    fill_sample_data(&ui, &appearance);
    ui.show().unwrap();

    for (width, height) in sizes {
        set_size(&window, width, height);
        ui.set_ever_connected(true);
        ui.set_page(0);
        for (name, filter) in DEVICE_VIEWS {
            set_tiles(&ui, filter);
            let path = format!("{out_dir}/{width}x{height}-{name}{suffix}.png");
            save_png(&window, width, height, &path);
            println!("{path}");
        }
        set_tiles(&ui, "all");
        for (name, page) in PAGES {
            ui.set_ever_connected(page.is_some());
            ui.set_page(page.unwrap_or(0));
            let path = format!("{out_dir}/{width}x{height}-{name}{suffix}.png");
            save_png(&window, width, height, &path);
            println!("{path}");
        }
        ui.set_ever_connected(true);
        ui.set_page(10);
        for (name, setup) in WIZARD_STEPS {
            ui.set_page(10);
            setup(&ui);
            let path = format!("{out_dir}/{width}x{height}-{name}{suffix}.png");
            save_png(&window, width, height, &path);
            println!("{path}");
        }
    }
}

fn parse_size(text: &str) -> (u32, u32) {
    let (w, h) = text.split_once('x').expect("sizes look like 1280x720");
    (w.parse().expect("bad width"), h.parse().expect("bad height"))
}

/// Draws the current state into a fresh buffer and saves it.
fn save_png(window: &MinimalSoftwareWindow, width: u32, height: u32, path: &str) {
    // Let layouts, bindings and (finished) animations settle first.
    slint::platform::update_timers_and_animations();
    window.request_redraw();
    let mut pixels = vec![PremultipliedRgbaColor::default(); (width * height) as usize];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width as usize);
    });
    let file = std::fs::File::create(path).expect("can't write the PNG");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let rgb: Vec<u8> = pixels.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
    encoder.write_header().unwrap().write_image_data(&rgb).unwrap();
}

/// Believable content: a handful of devices in rooms, a WiFi list, two
/// paired phones -- enough to see how each page fills up.
/// A list of texts for a sample device (a climate's modes).
fn strings(items: &[&str]) -> slint::ModelRc<slint::SharedString> {
    Rc::new(VecModel::from(items.iter().map(|&i| i.into()).collect::<Vec<slint::SharedString>>())).into()
}

fn fill_sample_data(ui: &AppWindow, appearance: &theme::Appearance) {
    ui.set_connected(true);
    let device = |id: &str, name: &str, room: &str| DeviceItem {
        id: id.into(),
        name: name.into(),
        room: room.into(),
        ..Default::default()
    };
    let devices = vec![
        DeviceItem { has_switch: true, on: true, ..device("ld7", "Board LED (LD7)", "Hub") },
        // Issue #85: an IR LED strip used as a light: its colours as chips,
        // brightness - / +.
        {
            let chip = |hex: &str, r, g, b| ColorChip { hex: hex.into(), color: slint::Color::from_rgb_u8(r, g, b) };
            DeviceItem { has_switch: true, on: true, has_color: true, color: slint::Color::from_rgb_u8(255, 0, 0),
                         color_text: "#FF0000".into(),
                         palette: Rc::new(VecModel::from(vec![
                             chip("#FF0000", 255, 0, 0), chip("#00FF00", 0, 255, 0), chip("#0000FF", 0, 0, 255),
                             chip("#FFFFFF", 255, 255, 255), chip("#FF8000", 255, 128, 0), chip("#FFFF00", 255, 255, 0),
                             chip("#00FFFF", 0, 255, 255), chip("#8000FF", 128, 0, 255), chip("#FF00FF", 255, 0, 255),
                             chip("#FF60A0", 255, 96, 160),
                         ])).into(),
                         brightness_up: "Brighter".into(), brightness_down: "Dimmer".into(),
                         has_remote: true, remote_learns: true,
                         ..device("led-strip", "Shelf LED strip", "Living room") }
        },
        DeviceItem { has_switch: true, on: true, has_dimmer: true, level: 42, has_color: true,
                     color: slint::Color::from_rgb_u8(0, 255, 136), color_text: "#00FF88".into(),
                     ..device("bulb", "Hall bulb", "Hall") },
        DeviceItem { has_switch: true, on: false, has_dimmer: true, level: 73, ..device("lamp-1", "Desk lamp", "Office") },
        DeviceItem { has_switch: true, on: true, has_media: true, volume: 12, input: "TV".into(),
                     inputs: Rc::new(VecModel::from(vec![
                         MediaInputItem { id: "TV".into(), label: "Live TV".into() },
                         MediaInputItem { id: "HDMI_1".into(), label: "HDMI 1".into() },
                         MediaInputItem { id: "HDMI_2".into(), label: "HDMI 2".into() },
                         MediaInputItem { id: "HDMI_3".into(), label: "HDMI 3".into() },
                     ])).into(),
                     has_remote: true, now_playing: "Live TV \u{2022} 5 Pro TV".into(),
                     ..device("tv", "Living room TV", "Living room") },
        DeviceItem { has_switch: true, on: false, has_dimmer: true, level: 100, has_color: true,
                     color: slint::Color::from_rgb_u8(255, 64, 194), color_text: "#FF40C2".into(),
                     status: "Offline".into(), ..device("strip", "Desk strip", "Office") },
        DeviceItem { has_switch: true, on: false, status: "Needs pairing again".into(), can_reauth: true,
                     ..device("tv2", "Bedroom TV", "Bedroom") },
        DeviceItem { sensor_text: "temperature 21.5 °C   humidity 48 %".into(), ..device("climate", "Climate sensor", "Bedroom") },
        DeviceItem { has_switch: true, on: true, energy_text: "1.86 kW \u{2022} 12.40 kWh".into(),
                     ..device("kettle", "Kettle", "Kitchen") },
        // Issue #74: an EZVIZ plug: consumption and its own settings.
        DeviceItem { has_switch: true, on: true, energy_text: "8.4 W \u{2022} today 0.12 kWh \u{2022} 12.30 kWh".into(),
                     options: Rc::new(VecModel::from(vec![
                         OptionItem { id: "status_light".into(), name: "Status light".into(), on: true, ..Default::default() },
                         OptionItem { id: "power_recovery".into(), name: "Restore after a power cut".into(), on: false, ..Default::default() },
                     ])).into(),
                     ..device("priza", "Priza EzViz", "Office") },
        // Issue #77: a blind, a garage door, an air conditioner, a lock.
        DeviceItem { has_cover: true, cover_position: 40, cover_can_position: true,
                     ..device("blind", "Living room blind", "Living room") },
        DeviceItem { has_cover: true, cover_position: 0, cover_moving: "Opening".into(),
                     ..device("garage", "Garage door", "Garage") },
        DeviceItem { has_climate: true, climate_mode: "cool".into(),
                     climate_modes: strings(&["off", "heat", "cool", "auto", "dry"]),
                     climate_target: 22.5, climate_target_text: "22.5 °C".into(), climate_min: 16.0, climate_max: 30.0,
                     climate_step: 0.5, climate_current_text: "26.1 °C".into(), climate_fan: "auto".into(),
                     climate_fans: strings(&["auto", "low", "medium", "high"]),
                     ..device("ac", "Bedroom AC", "Bedroom") },
        DeviceItem { has_lock: true, lock_state: "locked".into(), ..device("door", "Front door", "Hall") },
    ];
    // Issue #47: scenes (all of them are the home screen's chips since
    // #95), and the devices "Save current state" offers.
    let scene = |id: &str, name: &str, detail: &str| SceneItem { id: id.into(), name: name.into(), detail: detail.into() };
    let scenes = vec![scene("cozy", "Cozy", "1 device"), scene("movie", "Movie night", "3 devices"), scene("all-off", "All off", "6 devices")];
    ui.set_scenes(Rc::new(VecModel::from(scenes)).into());
    ui.set_scenes_message("Cozy: done".into());
    let pick = |id: &str, name: &str, detail: &str, selected| PickItem { id: id.into(), name: name.into(), detail: detail.into(), selected };
    ui.set_pick_devices(Rc::new(VecModel::from(vec![
        pick("bulb", "Hall bulb", "On, 42 %, #00FF88", true),
        pick("tv", "Living room TV", "On, volume 12", true),
        pick("lamp-1", "Desk lamp", "Off", false),
        pick("blind", "Living room blind", "40 % open", false),
    ])).into());
    ui.set_pick_count(2);
    let automation = |id: &str, name: &str, summary: &str, enabled| AutomationItem { id: id.into(), name: name.into(), summary: summary.into(), enabled };
    ui.set_automations(Rc::new(VecModel::from(vec![
        automation("cozy-at-sunset", "Cozy at sunset", "15 min before sunset \u{2192} Cozy", true),
        automation("weekday-morning", "Weekday mornings", "At 07:00, Mon\u{2013}Fri \u{2192} Bright", true),
        automation("tv-on", "Movie when the TV turns on", "When Living room TV turns on \u{2192} Movie night (if 1 condition)", false),
    ])).into());
    ui.set_automations_message("Saved \u{201C}Cozy at sunset\u{201D}.".into());
    let entry = |time: &str, name: &str, cause: &str, ok, detail: &str| LogItem { time: time.into(), name: name.into(), cause: cause.into(), ok, detail: detail.into() };
    ui.set_log_entries(Rc::new(VecModel::from(vec![
        entry("13:41:00", "Timer test", "13:41", true, ""),
        entry("13:39:04", "Plug on bulb on", "Shelly plug: switch.on is true", true, ""),
        entry("13:38:38", "Cozy", "touchscreen", true, ""),
        entry("Thu 22:15", "All off", "at 22:15", false, "led-strip: 192.168.1.139 isn't answering (no reply within 5 s)"),
    ])).into());
    ui.set_editor_scenes(Rc::new(VecModel::from(vec![scene("cozy", "Cozy", ""), scene("movie", "Movie night", "")])).into());
    ui.set_has_location(true);
    ui.set_location_current("44.43, 26.10".into());
    ui.set_location_text("44.43, 26.10".into());
    // The controls page (issue #95) shows one of these full cards: the IR
    // LED strip's.
    ui.set_controls_item(devices[1].clone());
    ui.set_controls_favourite(true);
    // The tiles (issue #95) are made from devices as backend_daemon sends
    // them -- the DK2's own test devices, and a few more kinds.
    let json = serde_json::json!([
        {"id": "ld7", "name": "Board LED (LD7)", "room": "Hub", "template": "m4-led", "online": "online",
         "capabilities": {"switch": {"on": true}}},
        {"id": "tv", "name": "Shelf LED strip", "room": "Living room", "template": "ir-blaster", "online": "online", "favourite": true,
         "capabilities": {"switch": {"on": true}, "color": {"hex": "#FF0000", "palette": ["#FF0000", "#00FF00", "#0000FF"]},
                          "remote": {"buttons": ["power", "red", "green", "blue"], "learn": true}}},
        {"id": "wiz-light", "name": "Hall bulb", "room": "Hall", "template": "wiz", "online": "online", "favourite": true,
         "capabilities": {"switch": {"on": true}, "dimmer": {"level": 42}, "color": {"kelvin": 2700}}},
        {"id": "led-strip", "name": "Desk strip", "room": "Office", "template": "wled", "online": "online",
         "capabilities": {"switch": {"on": true}, "dimmer": {"level": 80}, "color": {"hex": "#8000FF"}}},
        {"id": "led-strip-2", "name": "Bed strip", "room": "Bedroom", "template": "wled", "online": "online",
         "capabilities": {"switch": {"on": false}, "dimmer": {"level": 30}, "color": {"hex": "#FF8000"}}},
        {"id": "lg-tv-webos", "name": "Living room TV", "room": "Living room", "template": "lg-webos-tv", "online": "online", "favourite": true,
         "capabilities": {"switch": {"on": true}, "media": {"volume": 12, "muted": false, "input": "TV",
                          "app": {"id": "livetv", "label": "Live TV"}, "channel": {"id": "5", "number": "5", "name": "Pro TV"}},
                          "remote": {"buttons": ["UP"]}}},
        {"id": "shelly-plug", "name": "Kettle", "room": "Kitchen", "template": "shelly-plug-gen3", "online": "online",
         "capabilities": {"switch": {"on": true}, "energy": {"power_w": 1860.0, "energy_kwh": 12.4}}},
        {"id": "dk2-test-wled", "name": "Test WLED", "room": "Office", "template": "wled-mqtt", "online": "offline",
         "capabilities": {"switch": {"on": false}, "dimmer": {"level": 0}, "color": {"hex": "#1FFFC7"}}},
        {"id": "air", "name": "Bedroom air", "room": "Bedroom", "template": "tasmota-sensor-battery", "last_seen": 1_789_999_820u64,
         "capabilities": {"sensor": {"readings": {"temperature": {"value": 21.5, "unit": "°C"}, "humidity": {"value": 48.0, "unit": "%"}}}}},
        {"id": "blind", "name": "Living room blind", "room": "Living room", "template": "virtual",
         "capabilities": {"cover": {"position": 40, "moving": "stopped", "can_position": true}}},
        {"id": "ac", "name": "Bedroom AC", "room": "Bedroom", "template": "virtual",
         "capabilities": {"climate": {"mode": "cool", "target": 22.5, "current": 26.1, "modes": ["off", "cool"], "fans": [], "min": 16.0, "max": 30.0, "step": 0.5}}},
        {"id": "door", "name": "Front door", "room": "Hall", "template": "virtual",
         "capabilities": {"lock": {"state": "locked", "confirmed": false}}},
        {"id": "s7", "name": "S7", "room": "Living room", "template": "roborock-vacuum", "online": "online", "favourite": true,
         "capabilities": {"vacuum": {"state": "docked", "battery": 85, "detail": "charging"}}},
        {"id": "projector", "name": "Projector", "room": "Office", "template": "ir-blaster", "online": "unauthorized",
         "capabilities": {"remote": {"buttons": ["Power", "Menu"], "learn": true}}}
    ]);
    let samples: Vec<ws_client::Device> = serde_json::from_value(json).expect("sample devices");
    // The controls page's top part: the IR strip's tile.
    ui.set_controls_tile(tiles::tile(&samples[1], &[], ui.global::<Theme>().get_accent(), 1_790_000_000));
    // The home screen's clock, as zones.rs makes it.
    let (time, date) = zones::clock("Europe/Bucharest", 1_790_969_460);
    ui.set_clock_time(time.into());
    ui.set_clock_date(date.into());
    DEVICES.with(|d| *d.borrow_mut() = samples);

    ui.set_net(NetStatus {
        uplink: "wifi".into(),
        eth_connected: false,
        wifi_available: true,
        wifi_connected: true,
        wifi_ssid: "DIGI-F9rT".into(),
        wifi_bars: 4,
        wifi_ip: "192.168.1.136".into(),
        wifi_saved: "DIGI-F9rT".into(),
        wifi_country: "RO".into(),
        hotspot_ssid: "UC-Setup-46D6".into(),
        hotspot_url: "http://192.168.4.1".into(),
        ..Default::default()
    });
    let networks: Vec<WifiNetwork> = [("DIGI-F9rT", 4, true, true), ("N4@2.4GHz - D+P", 1, true, false),
                                      ("Orange-643F", 0, true, false), ("Cafe guest", 2, false, false)]
        .iter()
        .map(|&(ssid, bars, secure, saved)| WifiNetwork { ssid: ssid.into(), bars, secure, saved })
        .collect();
    ui.set_networks(Rc::new(VecModel::from(networks)).into());
    ui.set_join_ssid("N4@2.4GHz - D+P".into());
    ui.set_password("hunter2".into());
    ui.set_country_code("RO".into());

    let clients: Vec<ClientItem> = [("c-423c5f41", "hub_ws.py on ciprian-PC", "last seen just now"),
                                    ("c-afde1461", "Phone (Android)", "last seen 3 h ago")]
        .iter()
        .map(|&(id, name, seen)| ClientItem { id: id.into(), name: name.into(), seen_text: seen.into() })
        .collect();
    ui.set_clients(Rc::new(VecModel::from(clients)).into());

    ui.set_pairing_state("waiting".into());
    ui.set_pairing_code("212 570".into());
    ui.set_pairing_seconds(174);
    ui.set_pairing_fingerprint("06e8 3dde 9c01 1d4b".into());
    ui.set_pairing_address("192.168.1.136".into());
    ui.set_pairing_qr(fake_qr(260));
    ui.set_hotspot_qr(fake_qr(150));

    // Settings (issue #39): the preset being rendered, a time zone, and
    // the places of its region on the time zone page.
    ui.set_setting_mode(appearance.mode.clone().into());
    ui.set_setting_accent(appearance.accent.clone().into());
    ui.set_setting_density(appearance.density.clone().into());
    ui.set_setting_time_zone("Europe/Bucharest".into());
    ui.set_local_time("14:05".into());
    let dark = theme::is_dark(&appearance.mode, 12);
    let accents = theme::accents(dark);
    let name = accents.iter().find(|(id, _, _)| *id == appearance.accent).map_or("", |(_, n, _)| n.as_str());
    ui.set_setting_accent_name(name.into());
    let rows: Vec<AccentItem> =
        accents.into_iter().map(|(id, name, color)| AccentItem { id: id.into(), name: name.into(), color }).collect();
    ui.set_accents(Rc::new(VecModel::from(rows)).into());
    ui.set_zone_title("Europe".into());
    ui.set_zone_hint("Choose the place whose time the hub should follow.".into());
    let zones: Vec<ZoneItem> = zones::places("Europe/", "Europe/Bucharest")
        .into_iter()
        .map(|z| ZoneItem { label: z.label.into(), value: z.value.into(), marked: z.marked })
        .collect();
    ui.set_zone_items(Rc::new(VecModel::from(zones)).into());

    // Adding devices (issue #40).
    let found = vec![FoundItem { template: "wled".into(), name: "WLED-Desk".into(), address: "192.168.1.139".into(), type_name: "WLED light".into() }];
    ui.set_found(Rc::new(VecModel::from(found)).into());
    let templates = vec![
        TemplateItem { id: "wled".into(), name: "WLED light".into(), category: "Lighting".into(),
                       description: "LED strips and lamps running WLED (ESP8266 / ESP32).".into() },
        TemplateItem { id: "lg-webos-tv".into(), name: "LG TV (webOS)".into(), category: "TV & media".into(),
                       description: "LG smart TVs with webOS (2014 and later).".into() },
    ];
    ui.set_templates(Rc::new(VecModel::from(templates)).into());
    ui.set_remote_id("tv".into());
    ui.set_remote_name("Living room TV".into());
    ui.set_remote_playing("Live TV \u{2022} 5 Pro TV".into());
    ui.set_remote_keyboard(true);
    ui.set_dev_id("tv".into());
    ui.set_dev_name("Living room TV".into());
    ui.set_dev_rooms(strings(&["Bedroom", "Hall", "Kitchen", "Living room", "Office"]));
    ui.set_dev_room("Living room".into());
    ui.set_dev_type("LG TV (webOS)".into());
    ui.set_dev_status("Online".into());
    ui.set_dev_can_reauth(true);
    ui.set_dev_can_reconfigure(true);
    ui.set_dev_can_remove(true);
    ui.set_dev_has_remote(true);
    ui.set_dev_remote_learns(true);
}

/// A QR-code-sized checkerboard stand-in (the real code isn't the point of
/// a layout preview).
fn fake_qr(side: u32) -> slint::Image {
    let mut buf = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side, side);
    let cell = (side / 29).max(1);
    for (i, p) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % side / cell, i as u32 / side / cell);
        let v = if (x * 7 + y * 13) % 3 == 0 { 0 } else { 255 };
        *p = slint::Rgb8Pixel { r: v, g: v, b: v };
    }
    slint::Image::from_rgb8(buf)
}
