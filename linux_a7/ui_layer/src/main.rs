// main.rs -- entry point for ui_layer (issue #14). Per ARCHITECTURE.md,
// this process is deliberately "dumb": it displays whatever state
// backend_daemon gives it and forwards touch input back, and holds no state
// of its own beyond what's needed to draw the current frame. Everything
// this file does falls into exactly one of these:
//
//   1. Set up rendering (fb_platform.rs) and touch input (touch_input.rs) --
//      the two things Slint's software-renderer path doesn't provide out of
//      the box, unlike the alternative DRM-based backend this project isn't
//      using (see ui_layer/Cargo.toml's comment on why).
//   2. Connect to backend_daemon over WebSocket (ws_client.rs) and wire its
//      updates into the UI's displayed state.
//   3. Run one hand-written loop tying all three together -- there's no
//      Tokio runtime and no single "just call .run() and let Slint drive
//      everything" option here, because touch input and WebSocket updates
//      both need polling on the same thread as rendering (see
//      fb_platform.rs's `WindowOnlyPlatform` doc comment for why).

mod fb_platform;
mod touch_input;
mod ws_client;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use std::time::Duration;

/// One display frame at 60 fps -- the longest the main loop ever sleeps,
/// and also how long it sleeps between animation frames.
const FRAME: Duration = Duration::from_millis(16);

/// How long to wait for the real display driver at most (see
/// fb_platform::wait_for_display_driver). It normally takes ~8 s from the
/// UI's start; generous, since a slow boot is better than a UI on the
/// wrong framebuffer.
const DISPLAY_WAIT: Duration = Duration::from_secs(60);

/// How long a refused command's message stays under the device list.
const DEVICE_MESSAGE_TIME: Duration = Duration::from_secs(4);

/// How often to look for the touch panel again while it isn't there yet
/// (its driver is loaded by udev at about the same time as the display's).
const TOUCH_RETRY: Duration = Duration::from_secs(1);

// This macro reads the compiled output of build.rs (which itself compiled
// ui/app.slint) and makes its `AppWindow` type -- along with the
// `set_led_on`/`get_led_on`/`set_connected`/`on_toggle_led` methods
// app.slint's properties and callbacks generate -- available right here, as
// if they were hand-written in this file. None of those methods exist
// anywhere as literal Rust source; they're all generated fresh from
// app.slint on every build.
slint::include_modules!();

fn main() {
    // The one `MinimalSoftwareWindow` this whole app has, created once and
    // shared (via cheap `Rc` clones -- see fb_platform.rs's header comment)
    // between the two things that each need their own handle to it: the
    // `Platform` registration Slint requires globally, and the
    // `FbRenderer` this loop calls directly every iteration.
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);

    // Slint requires exactly one call to `set_platform`, before creating
    // any window (`AppWindow::new()`, below) -- this is what makes Slint
    // know how to actually get pixels on screen at all. `.expect()` here is
    // deliberate: if this fails, there is no possible UI, so there is
    // nothing more useful this program could do than stop immediately with
    // a clear message in the journal (see fb_platform.rs's own doc comment
    // on why panicking is acceptable for a systemd service with no
    // attached terminal).
    slint::platform::set_platform(Box::new(fb_platform::WindowOnlyPlatform(window.clone())))
        .expect("failed to set Slint platform");

    // Now that a platform is registered, Slint can actually construct the
    // window described in app.slint. `renderer` is kept around separately
    // from `window` (even though `window` is technically for the platform
    // registration above) because it also needs to open the real
    // framebuffer device -- see fb_platform.rs's header comment for why
    // these responsibilities are split into two structs instead of one.
    //
    // Since issue #59 this process starts very early in boot (see
    // ui-layer.service), so first wait for the real display driver.
    fb_platform::wait_for_display_driver(DISPLAY_WAIT);
    let renderer = fb_platform::FbRenderer::new(window.clone())
        .expect("failed to open the framebuffer -- is CONFIG_DRM_FBDEV_EMULATION on and is a display actually connected?");

    let ui = AppWindow::new().expect("failed to construct AppWindow from app.slint");

    // A touchscreen not being present shouldn't be fatal (see
    // touch_input.rs's own doc comment) -- `None` here just means the loop
    // below skips polling it, and a missing/disconnected touch panel is
    // still a better failure mode than refusing to display anything.
    //
    // Since issue #59 the UI starts before udev has necessarily loaded the
    // touch panel's driver, so "not there yet" is normal right after boot:
    // the loop below keeps looking for it every TOUCH_RETRY until it shows
    // up. Only the first failure is logged.
    let mut touch = open_touch(true);
    let mut touch_checked = std::time::Instant::now();

    // Starts ws_client.rs's background thread (see its own doc comment for
    // why this needs to be a separate OS thread rather than something
    // awaited inline) and gets back the two channel ends this loop uses:
    // `request_tx` to send requests (commands, WiFi actions), `update_rx`
    // to receive what backend_daemon reports (devices, network, ...).
    let (request_tx, update_rx) = ws_client::start();

    // The device list (issue #34): what backend_daemon last reported,
    // turned into the rows app.slint shows. See DeviceRows.
    let mut rows = DeviceRows::new(&ui);
    // When the current error message under the list goes away again.
    let mut device_message_until: Option<std::time::Instant> = None;
    // Pairing screen (issue #35): the last network status (to put the
    // hub's current address into the QR code), the code the current QR
    // image was made for, and when the status was last asked for.
    let mut last_net = ws_client::NetworkStatus::default();
    let mut qr_code_for = String::new();
    // The setup hotspot's join QR (issue #36): made again only when the
    // hotspot's name or password change.
    let mut hotspot_qr_for = String::new();
    let mut pairing_polled = std::time::Instant::now();

    // Toggle and level bar (devices.slint): turned into a command for
    // that capability. The row does NOT change by itself -- it changes
    // when backend_daemon confirms, as a DeviceChanged update below.
    //
    // `move` closures: each callback gets its own clone of the channel end
    // (a cheap handle). `let _ =`: a failed send only means ws_client's
    // thread has already exited -- nothing useful to do about it here.
    let tx = request_tx.clone();
    ui.on_set_switch(move |id, on| {
        let _ = tx.send(ws_client::Request::Command {
            id: id.to_string(),
            capability: "switch".into(),
            value: serde_json::json!({ "on": on }),
        });
    });
    let tx = request_tx.clone();
    ui.on_set_level(move |id, level| {
        let _ = tx.send(ws_client::Request::Command {
            id: id.to_string(),
            capability: "dimmer".into(),
            value: serde_json::json!({ "level": level }),
        });
    });

    // `ui.as_weak()` below: a `Weak` reference to the UI, not a strong one
    // -- the callback closure is stored *inside* `ui` itself, so a strong
    // reference would make the UI keep itself alive forever (a reference
    // cycle). `.unwrap()` is safe: the window lives as long as the process.
    // The network screens (issue #61): each callback just forwards the
    // request; the answer comes back as an `Update` in the loop below.
    // `scanning`/`busy` are set here so the screen reacts to the tap at
    // once, and cleared when the answer arrives.
    let tx = request_tx.clone();
    let ui_weak = ui.as_weak();
    ui.on_wifi_scan(move || {
        ui_weak.unwrap().set_scanning(true);
        let _ = tx.send(ws_client::Request::WifiScan);
    });
    let tx = request_tx.clone();
    ui.on_wifi_connect(move |ssid, password| {
        // The password is only handed on, never printed or stored here.
        let _ = tx.send(ws_client::Request::WifiConnect {
            ssid: ssid.to_string(),
            password: password.to_string(),
        });
    });
    let tx = request_tx.clone();
    ui.on_wifi_forget(move || {
        let _ = tx.send(ws_client::Request::WifiForget);
    });
    let tx = request_tx.clone();
    ui.on_set_country(move |country| {
        let _ = tx.send(ws_client::Request::SetWifiCountry {
            country: country.to_string(),
        });
    });
    // The setup hotspot (issue #36).
    let tx = request_tx.clone();
    ui.on_start_hotspot(move || {
        let _ = tx.send(ws_client::Request::StartHotspot);
    });
    let tx = request_tx.clone();
    ui.on_stop_hotspot(move || {
        let _ = tx.send(ws_client::Request::StopHotspot);
    });

    // Paired devices and pairing (issue #35).
    let tx = request_tx.clone();
    ui.on_open_clients(move || {
        let _ = tx.send(ws_client::Request::ListClients);
    });
    let tx = request_tx.clone();
    ui.on_start_pairing(move || {
        let _ = tx.send(ws_client::Request::StartPairing);
    });
    let tx = request_tx.clone();
    ui.on_cancel_pairing(move || {
        let _ = tx.send(ws_client::Request::CancelPairing);
    });
    let tx = request_tx.clone();
    ui.on_revoke_client(move |id| {
        let _ = tx.send(ws_client::Request::RevokeClient { id: id.to_string() });
    });

    // The two string helpers app.slint can't do itself.
    ui.on_drop_last(|text| {
        let mut text = text.to_string();
        text.pop();
        text.into()
    });
    ui.on_mask(|text| "\u{2022}".repeat(text.chars().count()).into());

    // The main loop. Runs forever (this is a long-lived UI process, not a
    // one-shot tool), doing four things every iteration:
    loop {
        // 1. Let Slint advance any in-progress animations (button press
        //    visual feedback, etc.) -- required even though app.slint
        //    doesn't declare any explicit animations, since built-in
        //    widgets like Button have their own.
        slint::platform::update_timers_and_animations();

        // 2. Check for new touch events and turn them into Slint pointer
        //    events (see touch_input.rs) -- this is what makes tapping the
        //    button actually register as a tap.
        if let Some(touch) = touch.as_mut() {
            touch.poll(&window);
        } else if touch_checked.elapsed() >= TOUCH_RETRY {
            // (TouchInput::open logs the panel's range once it's found.)
            touch = open_touch(false);
            touch_checked = std::time::Instant::now();
        }

        // 3. Drain any updates from backend_daemon and reflect them in the
        //    UI's properties -- `try_recv()` never blocks, so this loop
        //    doesn't stall waiting for a message that might not be coming.
        //    This is the *only* place that changes what the screen shows
        //    about devices, matching ui_layer's "just displays what the
        //    daemon says" design: tapping a toggle does NOT flip it by
        //    itself (see `on_set_switch` above) -- it changes once
        //    backend_daemon confirms, arriving here as DeviceChanged.
        while let Ok(update) = update_rx.try_recv() {
            match update {
                // ever-connected (issue #59): from the first successful
                // connection on, app.slint shows the normal screen instead
                // of the welcome screen -- and a LATER lost connection is
                // shown as "reconnecting", not as the welcome screen again.
                ws_client::Update::Connected => {
                    ui.set_connected(true);
                    ui.set_ever_connected(true);
                    // For the settings page's "N devices can control this hub".
                    let _ = request_tx.send(ws_client::Request::ListClients);
                }
                ws_client::Update::Disconnected => ui.set_connected(false),
                ws_client::Update::Devices(list) => rows.replace_all(list),
                ws_client::Update::DeviceChanged(device) => rows.changed(device),
                ws_client::Update::DeviceRemoved(id) => rows.removed(&id),
                ws_client::Update::CommandFailed(message) => {
                    ui.set_device_message(message.into());
                    device_message_until = Some(std::time::Instant::now() + DEVICE_MESSAGE_TIME);
                }
                ws_client::Update::Network(status) => {
                    ui.set_net(to_net_status(&status));
                    if let Some(password) = status.hotspot.password.as_ref().filter(|_| status.hotspot.active) {
                        let key = format!("{}\n{password}", status.hotspot.ssid);
                        if key != hotspot_qr_for {
                            // The standard "join this WiFi" QR code phone
                            // cameras understand. (Our names and passwords
                            // have none of the characters the format would
                            // need escaped: \ ; , : ")
                            let join = format!("WIFI:T:WPA;S:{};P:{password};;", status.hotspot.ssid);
                            ui.set_hotspot_qr(qr_image(&join, 150));
                            hotspot_qr_for = key;
                        }
                    }
                    last_net = status;
                }
                ws_client::Update::Pairing(pairing) => {
                    if pairing.state == "paired" && ui.get_pairing_state() != "paired" {
                        // A new device: refresh the list (and its count).
                        let _ = request_tx.send(ws_client::Request::ListClients);
                    }
                    show_pairing(&ui, &pairing, &last_net, &mut qr_code_for);
                }
                ws_client::Update::Clients(clients) => ui.set_clients(client_rows(&clients)),
                ws_client::Update::Networks(networks) => {
                    ui.set_scanning(false);
                    let rows: Vec<WifiNetwork> = networks
                        .into_iter()
                        .map(|n| WifiNetwork {
                            ssid: n.ssid.into(),
                            bars: n.bars.into(),
                            secure: n.secure,
                            saved: n.saved,
                        })
                        .collect();
                    // A Slint list property takes a "model"; VecModel is
                    // the ready-made one backed by a Vec.
                    ui.set_networks(std::rc::Rc::new(slint::VecModel::from(rows)).into());
                }
                ws_client::Update::ScanFailed(message) => {
                    ui.set_scanning(false);
                    show_net_message(&ui, format!("Scan failed: {message}"), true);
                }
                ws_client::Update::ActionDone(action, result) => {
                    if action == ws_client::Action::Revoke {
                        let _ = request_tx.send(ws_client::Request::ListClients);
                    }
                    apply_action_result(&ui, action, result)
                }
            }
        }

        // While the pairing screen shows a code, ask once a second how the
        // pairing is going (countdown, and "paired" the moment it happens).
        if ui.get_page() == 6
            && ui.get_pairing_state() == "waiting"
            && pairing_polled.elapsed() >= Duration::from_secs(1)
        {
            pairing_polled = std::time::Instant::now();
            let _ = request_tx.send(ws_client::Request::PairingStatus);
        }

        if device_message_until.is_some_and(|until| std::time::Instant::now() >= until) {
            ui.set_device_message("".into());
            device_message_until = None;
        }

        // 4. Actually draw, if anything changed as a result of the above
        //    (a touch, a property update, or an animation frame) --
        //    `draw_if_needed` (inside `FbRenderer`) checks that on its own,
        //    so this call is cheap on iterations where nothing did.
        renderer.draw_if_needed();

        // Don't spin the CPU checking all of the above hundreds of times a
        // second -- ALWAYS sleep before the next iteration:
        //   - while an animation runs (e.g. Button's press effect): one
        //     frame, ~16 ms = 60 fps, which is all the display can show;
        //   - otherwise: until Slint's next timer is due, but at most one
        //     frame, so touch input is still picked up within ~16 ms.
        //
        // Before issue #31's measurement this loop didn't sleep at all
        // during animations -- it redrew as fast as the CPU allowed. Tapping
        // the button quickly keeps a press animation running almost
        // constantly, so the UI used a whole A7 core (~50% of the board,
        // measured) for an effect nobody can see above 60 fps. With the
        // frame cap, the same tapping costs a few percent.
        let wait = if window.has_active_animations() {
            FRAME
        } else {
            slint::platform::duration_until_next_timer_update()
                .map_or(FRAME, |until_timer| until_timer.min(FRAME))
        };
        std::thread::sleep(wait);
    }
}

/// Opens the touch panel, if it exists yet (see its use in main). `log`:
/// whether "not found"/errors are printed -- only on the first attempt, so
/// retrying every second doesn't flood the journal.
fn open_touch(log: bool) -> Option<touch_input::TouchInput> {
    match touch_input::TouchInput::open() {
        Ok(Some(touch)) => Some(touch),
        Ok(None) => {
            if log {
                println!("ui_layer: no touch panel yet, will keep looking");
            }
            None
        }
        Err(e) => {
            if log {
                println!("ui_layer: touch input unavailable: {e}");
            }
            None
        }
    }
}

/// ws_client's network status -> the struct app.slint shows (absent
/// values become empty strings, Slint's "nothing").
fn to_net_status(status: &ws_client::NetworkStatus) -> NetStatus {
    let text = |value: &Option<String>| value.clone().unwrap_or_default().into();
    NetStatus {
        uplink: text(&status.uplink),
        eth_connected: status.ethernet.connected,
        eth_ip: text(&status.ethernet.ip),
        wifi_available: status.wifi.available,
        wifi_connected: status.wifi.connected,
        wifi_ssid: text(&status.wifi.ssid),
        wifi_bars: status.wifi.bars.into(),
        wifi_ip: text(&status.wifi.ip),
        wifi_saved: text(&status.wifi.saved_ssid),
        wifi_country: text(&status.wifi.country),
        hotspot_active: status.hotspot.active,
        hotspot_ssid: status.hotspot.ssid.clone().into(),
        // "xmfk7p2q9hta" -> "xmfk 7p2q 9hta": easier to read off the screen.
        hotspot_password: status
            .hotspot
            .password
            .as_deref()
            .map(|p| {
                p.as_bytes()
                    .chunks(4)
                    .map(|c| String::from_utf8_lossy(c).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
            .into(),
        hotspot_url: status.hotspot.url.clone().into(),
        hotspot_error: text(&status.hotspot.last_error),
        hotspot_connecting: status.hotspot.connecting,
    }
}

fn show_net_message(ui: &AppWindow, message: String, is_error: bool) {
    ui.set_net_message(message.into());
    ui.set_net_message_is_error(is_error);
}

/// What the screen does once backend_daemon has answered a WiFi action.
fn apply_action_result(ui: &AppWindow, action: ws_client::Action, result: Result<(), String>) {
    use ws_client::Action;
    match (action, result) {
        (Action::Connect, Ok(())) => {
            ui.set_busy(false);
            // Don't keep the password around in the UI longer than needed.
            ui.set_password("".into());
            show_net_message(ui, format!("Connected to {}", ui.get_join_ssid()), false);
            ui.set_page(1);
        }
        (Action::Connect, Err(message)) => {
            ui.set_busy(false);
            if ui.get_page() == 2 {
                // Stay on the password page to try again.
                ui.set_join_message(message.into());
            } else {
                // An open network (no password page).
                show_net_message(ui, format!("Could not join {}: {message}", ui.get_join_ssid()), true);
            }
        }
        (Action::Forget, Ok(())) => show_net_message(ui, "WiFi network forgotten".into(), false),
        (Action::Forget, Err(message)) => show_net_message(ui, message, true),
        (Action::Country, Ok(())) => {
            show_net_message(ui, format!("WiFi country set to {}", ui.get_country_code()), false);
            ui.set_page(1);
        }
        (Action::Country, Err(message)) => ui.set_country_message(message.into()),
        // The list itself is refreshed by the caller (see the main loop).
        (Action::Revoke, Ok(())) => ui.set_clients_message("Removed. It can no longer control the hub.".into()),
        (Action::Revoke, Err(message)) => ui.set_clients_message(message.into()),
        (Action::Hotspot, Ok(())) => {}
        (Action::Hotspot, Err(message)) => show_net_message(ui, format!("Setup hotspot: {message}"), true),
    }
}

/// The device list shown on the main screen (issue #34): the devices as
/// backend_daemon last reported them, and the rows app.slint draws.
///
/// The rows live in one VecModel for the program's whole life. On a change
/// only the rows that differ are updated (set_row_data); the whole list is
/// only replaced when devices appear, disappear or change order -- so the
/// list doesn't jump back to the top every time a lamp is switched.
struct DeviceRows {
    devices: std::collections::BTreeMap<String, ws_client::Device>,
    model: std::rc::Rc<slint::VecModel<DeviceItem>>,
    /// The id of each row, in the model's order.
    ids: Vec<String>,
}

impl DeviceRows {
    fn new(ui: &AppWindow) -> Self {
        let model = std::rc::Rc::new(slint::VecModel::default());
        ui.set_devices(model.clone().into());
        DeviceRows {
            devices: Default::default(),
            model,
            ids: Vec::new(),
        }
    }

    fn replace_all(&mut self, list: Vec<ws_client::Device>) {
        self.devices = list.into_iter().map(|d| (d.id.clone(), d)).collect();
        self.refresh();
    }

    fn changed(&mut self, device: ws_client::Device) {
        self.devices.insert(device.id.clone(), device);
        self.refresh();
    }

    fn removed(&mut self, id: &str) {
        self.devices.remove(id);
        self.refresh();
    }

    /// Brings the model in line with `devices`: sorted by room, then name
    /// (so a room's devices stay together), then id.
    fn refresh(&mut self) {
        use slint::Model;
        let mut sorted: Vec<&ws_client::Device> = self.devices.values().collect();
        sorted.sort_by(|a, b| (&a.room, &a.name, &a.id).cmp(&(&b.room, &b.name, &b.id)));
        let ids: Vec<String> = sorted.iter().map(|d| d.id.clone()).collect();
        let items: Vec<DeviceItem> = sorted.into_iter().map(device_item).collect();
        if ids == self.ids {
            for (row, item) in items.into_iter().enumerate() {
                if self.model.row_data(row).as_ref() != Some(&item) {
                    self.model.set_row_data(row, item);
                }
            }
        } else {
            self.model.set_vec(items);
            self.ids = ids;
        }
    }
}

/// One device -> one row of the list: which capabilities it has, and
/// their values in the form app.slint shows them.
fn device_item(device: &ws_client::Device) -> DeviceItem {
    let caps = &device.capabilities;
    let (color, color_text) = match &caps.color {
        Some(c) => color_of(c),
        None => (slint::Color::default(), String::new()),
    };
    let sensor_text = caps.sensor.as_ref().map_or(String::new(), |s| {
        s.readings
            .iter()
            .map(|(name, r)| format!("{name} {} {}", r.value, r.unit).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("   ")
    });
    DeviceItem {
        id: device.id.clone().into(),
        name: device.name.clone().into(),
        room: device.room.clone().into(),
        has_switch: caps.switch.is_some(),
        on: caps.switch.as_ref().is_some_and(|s| s.on),
        has_dimmer: caps.dimmer.is_some(),
        level: caps.dimmer.as_ref().map_or(0, |d| d.level.into()),
        has_color: caps.color.is_some(),
        color,
        color_text: color_text.into(),
        sensor_text: sensor_text.into(),
    }
}

/// The swatch color and its label: "#FF8800" as is; a white temperature
/// as an approximate tint between warm (2000 K, orange-ish) and cold
/// (6500 K, bluish white) -- only to hint at it, not a colorimetric
/// conversion.
fn color_of(color: &ws_client::Color) -> (slint::Color, String) {
    if let Some(hex) = &color.hex {
        let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
        let rgb = slint::Color::from_rgb_u8((value >> 16) as u8, (value >> 8) as u8, value as u8);
        return (rgb, hex.to_uppercase());
    }
    let kelvin = color.kelvin.unwrap_or(4000);
    let t = ((f32::from(kelvin) - 2000.0) / 4500.0).clamp(0.0, 1.0);
    let mix = |warm: f32, cold: f32| (warm + (cold - warm) * t).round() as u8;
    (
        slint::Color::from_rgb_u8(mix(255.0, 201.0), mix(167.0, 218.0), mix(87.0, 255.0)),
        format!("{kelvin} K"),
    )
}

/// Puts the pairing data on the pairing screen. The QR code is only drawn
/// again when the code changes (not on every once-a-second update).
fn show_pairing(ui: &AppWindow, p: &ws_client::Pairing, net: &ws_client::NetworkStatus, qr_code_for: &mut String) {
    ui.set_pairing_state(p.state.clone().into());
    ui.set_pairing_seconds(p.seconds_left.min(i32::MAX as u64) as i32);
    ui.set_pairing_client(p.client_name.clone().unwrap_or_default().into());
    ui.set_pairing_fingerprint(p.fingerprint_short.clone().into());

    // The address the phone should use: the one of the link that carries
    // traffic right now, if the backend lists it; else the first one.
    let uplink_ip = match net.uplink.as_deref() {
        Some("ethernet") => net.ethernet.ip.clone(),
        Some("wifi") => net.wifi.ip.clone(),
        _ => None,
    };
    let address = uplink_ip
        .filter(|ip| p.addresses.contains(ip))
        .or_else(|| p.addresses.first().cloned())
        .unwrap_or_default();
    ui.set_pairing_address(address.clone().into());

    let Some(code) = &p.code else {
        ui.set_pairing_code("".into());
        return;
    };
    // "212 570": two groups of three read and type easier.
    ui.set_pairing_code(format!("{} {}", &code[..3.min(code.len())], &code[3.min(code.len())..]).into());
    if *code != *qr_code_for {
        // What the phone app reads from the QR code: where the hub is, the
        // one-time code, and the certificate fingerprint to pin (see
        // backend_daemon's tls.rs). A URI of our own ("uchub:"), so the
        // app recognises it as a hub pairing code and nothing else.
        let uri = format!(
            "uchub://pair?host={address}&port={}&code={code}&fp={}",
            p.port, p.fingerprint
        );
        ui.set_pairing_qr(qr_image(&uri, 260));
        *qr_code_for = code.clone();
    }
}

/// Draws `text` as a QR code image: black modules on white, with the
/// 4-module white border scanners need, each module a whole number of
/// pixels, and the whole at most `max_px` wide. The page shows it at
/// exactly this size (no width/height set on the Image): scaling it on
/// screen made some modules a pixel wider than others, and the hotspot's
/// small QR code then didn't scan (found in a PC render, #36).
fn qr_image(text: &str, max_px: i32) -> slint::Image {
    use qrcodegen::{QrCode, QrCodeEcc};
    // Medium error correction: still readable with ~15 % of it unreadable
    // (glare on the screen). A pairing URI always fits, so encoding can't
    // fail; if it somehow did, an empty image is better than a crash.
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return slint::Image::default();
    };
    const BORDER: i32 = 4;
    let modules = qr.size() + 2 * BORDER;
    let scale = (max_px / modules).max(1);
    let side = (modules * scale) as u32;
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side, side);
    let width = side as usize;
    for (i, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
        let x = (i % width) as i32 / scale - BORDER;
        let y = (i / width) as i32 / scale - BORDER;
        // get_module is false outside the code, so the border is white.
        let v = if qr.get_module(x, y) { 0 } else { 255 };
        *pixel = slint::Rgb8Pixel { r: v, g: v, b: v };
    }
    slint::Image::from_rgb8(pixels)
}

/// The paired devices as list rows, with "last seen ..." worked out from
/// the hub's clock.
fn client_rows(clients: &[ws_client::Client]) -> slint::ModelRc<ClientItem> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let rows: Vec<ClientItem> = clients
        .iter()
        .map(|c| {
            let ago = now.saturating_sub(c.last_seen);
            let seen = match ago {
                0..=59 => "last seen just now".to_string(),
                60..=3599 => format!("last seen {} min ago", ago / 60),
                3600..=86399 => format!("last seen {} h ago", ago / 3600),
                _ => format!("last seen {} days ago", ago / 86400),
            };
            ClientItem {
                id: c.id.clone().into(),
                name: c.name.clone().into(),
                seen_text: seen.into(),
            }
        })
        .collect();
    std::rc::Rc::new(slint::VecModel::from(rows)).into()
}
