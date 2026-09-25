// main.rs -- entry point for ui_layer (issue #14). Per ARCHITECTURE.md,
// this process is deliberately "dumb": it displays whatever state
// backend_daemon gives it and forwards touch input back, and holds no state
// of its own beyond what's needed to draw the current frame. Everything
// this file does falls into exactly one of these:
//
//   1. Choose the screen -- the touchscreen, or an HDMI monitor if one is
//      connected (display.rs, issue #38) -- and start Slint on it. Slint's
//      KMS backend draws, and reads touch, mouse and keyboard (libinput).
//   2. Connect to backend_daemon over WebSocket (ws_client.rs) and wire its
//      updates into the UI's displayed state.
//   3. Poll what Slint's own event loop can't know about -- the
//      WebSocket's updates -- from a timer on that loop, and watch for a
//      monitor being plugged in or out.

mod display;
mod pointer;
mod theme;
mod ws_client;
mod zones;

use std::time::Duration;

/// How often backend_daemon's updates are checked. 50 ms: a switched lamp
/// shows up on screen within a twentieth of a second, which reads as
/// instant, while the timer wakes the CPU only 20 times a second. (Until
/// #38 the same loop also polled the touch panel, which needed every
/// display frame, 16 ms; Slint's libinput reads input itself now, woken by
/// the kernel only when something happens.)
const POLL: Duration = Duration::from_millis(50);

/// How often to check whether a monitor was plugged in or out (issue #38).
const HOTPLUG_CHECK: Duration = Duration::from_secs(1);

/// How often the time is looked at again (issue #39): for "auto" mode's
/// switch between light and dark, and the clock on the Settings page.
const CLOCK_CHECK: Duration = Duration::from_secs(30);

/// The exit code when the monitor was plugged in or out and restarting in
/// place (see restart_on_other_screen) failed: the UI quits, and systemd
/// starts it again (ui-layer.service, Restart=on-failure). 75 is the usual
/// "temporary failure, try again" code (EX_TEMPFAIL).
const EXIT_SCREEN_CHANGED: i32 = 75;

/// How long to wait for the real display driver at most (see
/// display::wait_for_display_driver). It normally takes ~5 s from the
/// UI's start; generous, since a slow boot is better than a UI on the
/// wrong display device.
const DISPLAY_WAIT: Duration = Duration::from_secs(60);

/// How long a refused command's message stays under the device list.
const DEVICE_MESSAGE_TIME: Duration = Duration::from_secs(4);

// This macro reads the compiled output of build.rs (which itself compiled
// ui/app.slint) and makes its `AppWindow` type -- along with the
// `set_led_on`/`get_led_on`/`set_connected`/`on_toggle_led` methods
// app.slint's properties and callbacks generate -- available right here, as
// if they were hand-written in this file. None of those methods exist
// anywhere as literal Rust source; they're all generated fresh from
// app.slint on every build.
slint::include_modules!();

fn main() {
    // Since issue #59 this process starts very early in boot (see
    // ui-layer.service), so first wait for the real display driver.
    display::wait_for_display_driver(DISPLAY_WAIT);

    // Touchscreen or monitor (issue #38): tells Slint through an
    // environment variable, so it must happen before the first window.
    let output = display::choose_output();
    // Remembered to notice a change later (the hotplug timer below).
    let monitor_at_start = display::external_connected();

    // Slint's KMS backend, with a look at every input event first: the
    // mouse pointer, and ignoring the touch panel while on a monitor (see
    // pointer.rs).
    let on_monitor = output.as_deref().is_some_and(display::is_external);
    let pointer = std::rc::Rc::new(pointer::Pointer::new(on_monitor));
    let hook_pointer = pointer.clone();
    slint::BackendSelector::new()
        .backend_name("linuxkms".into())
        .with_libinput_event_hook(move |event| hook_pointer.on_event(event))
        .select()
        .expect("failed to set up Slint's KMS backend");

    // Creating the first window starts the backend: it opens
    // /dev/dri/card0 and sets the output's mode. `.expect()`: if this fails
    // there is no possible UI, so stopping with a clear message in the
    // journal is the most useful thing left to do (systemd restarts the
    // service).
    let ui = AppWindow::new().expect("failed to start the UI on the display (see the ui_layer: lines above for the chosen output)");
    pointer.attach(&ui);

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

    // ---- Hub settings (issue #39) ----------------------------------------
    // The settings as backend_daemon last reported them, shared by the
    // update handling, the callbacks and the clock timer below (Rc: shared
    // ownership on this one thread; RefCell: changed at run time). The
    // defaults show until the backend answers -- the same look as
    // theme.slint's starting values.
    let hub_settings = std::rc::Rc::new(std::cell::RefCell::new(ws_client::HubSettings::default()));
    let mut shown_dark: Option<bool> = None;
    show_settings(&ui, &hub_settings.borrow(), &mut shown_dark);

    // A tap on the Appearance page sends ONE changed field; the page shows
    // the change once the backend confirms (Update::Settings below).
    let set = |field: fn(String) -> ws_client::Request| {
        let tx = request_tx.clone();
        move |value: slint::SharedString| {
            let _ = tx.send(field(value.to_string()));
        }
    };
    ui.on_set_mode(set(|mode| ws_client::Request::SetSettings { mode: Some(mode), accent: None, density: None, time_zone: None }));
    ui.on_set_accent(set(|accent| ws_client::Request::SetSettings { mode: None, accent: Some(accent), density: None, time_zone: None }));
    ui.on_set_density(set(|density| ws_client::Request::SetSettings { mode: None, accent: None, density: Some(density), time_zone: None }));

    // The time zone page: the regions first...
    let ui_weak = ui.as_weak();
    let settings = hub_settings.clone();
    ui.on_open_time_zone(move || {
        let ui = ui_weak.unwrap();
        ui.set_zone_title("Time zone".into());
        ui.set_zone_hint("The hub's clock stays on UTC; this is for the times it shows, and when \"auto\" switches to light and dark.".into());
        ui.set_zone_items(zone_rows(zones::regions(&settings.borrow().time_zone)));
    });
    // ...then, for a tapped region, its places; a tapped place (or UTC) is
    // the new time zone.
    let ui_weak = ui.as_weak();
    let settings = hub_settings.clone();
    let tx = request_tx.clone();
    ui.on_pick_zone(move |item| {
        let ui = ui_weak.unwrap();
        if let Some(region) = item.value.strip_suffix('/') {
            ui.set_zone_title(region.into());
            ui.set_zone_hint("Choose the place whose time the hub should follow.".into());
            ui.set_zone_items(zone_rows(zones::places(&item.value, &settings.borrow().time_zone)));
        } else {
            let _ = tx.send(ws_client::Request::SetSettings {
                mode: None,
                accent: None,
                density: None,
                time_zone: Some(item.value.to_string()),
            });
            ui.set_page(4);
        }
    });

    // Everything Slint's event loop doesn't know about by itself, checked
    // every POLL on that same loop (Slint runs a timer's closure between
    // frames, on the UI thread -- so it may touch the UI freely). Before
    // #38 this was a hand-written loop that also drew the frames; Slint's
    // KMS backend now draws, in step with the display's refresh.
    //
    // `move`: the closure takes over everything it uses (the device rows,
    // the channel ends, ...) for the rest of the program. `ui_weak`: the UI through a weak reference, as in the
    // callbacks above.
    let ui_weak = ui.as_weak();
    let settings = hub_settings.clone();
    let poll_timer = slint::Timer::default();
    poll_timer.start(slint::TimerMode::Repeated, POLL, move || {
        let ui = ui_weak.unwrap();

        // Drain any updates from backend_daemon and reflect them in the
        //    UI's properties -- `try_recv()` never blocks, so this doesn't
        //    stall waiting for a message that might not be coming.
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
                // Issue #39: a new look (or time zone), from this screen or
                // a phone -- applied at once, no restart.
                ws_client::Update::Settings(new) => {
                    show_settings(&ui, &new, &mut shown_dark);
                    *settings.borrow_mut() = new;
                }
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
    });

    // A monitor plugged in or out (issue #38): start again on the right
    // screen (see restart_on_other_screen).
    let hotplug_timer = slint::Timer::default();
    hotplug_timer.start(slint::TimerMode::Repeated, HOTPLUG_CHECK, move || {
        let monitor_now = display::external_connected();
        if monitor_now != monitor_at_start {
            println!(
                "ui_layer: monitor {}, restarting on the other screen",
                if monitor_now { "plugged in" } else { "unplugged" }
            );
            restart_on_other_screen();
        }
    });

    // The time moves on (issue #39): "auto" mode may switch between light
    // and dark, and the Settings page's clock advances.
    let ui_weak = ui.as_weak();
    let settings = hub_settings.clone();
    let mut clock_dark = None;
    let clock_timer = slint::Timer::default();
    clock_timer.start(slint::TimerMode::Repeated, CLOCK_CHECK, move || {
        show_settings(&ui_weak.unwrap(), &settings.borrow(), &mut clock_dark);
    });

    // Shows the window and runs Slint's event loop -- forever, in
    // practice: this is a long-lived UI process.
    ui.run().expect("the UI's event loop failed");
}

/// Starts the UI again from scratch, so it chooses the screen afresh
/// (display::choose_output). Simpler and more robust than switching screens
/// inside a running Slint, whose KMS backend picks its output once, at
/// start.
///
/// Done with exec(): the program REPLACES itself with a fresh copy of
/// itself -- same process id, so systemd doesn't even notice. Everything
/// open is closed on the way (Rust opens every file and socket
/// "close-on-exec"), so the display is free for the new copy. The first
/// version quit instead and let systemd restart it, but systemd waits
/// RestartSec (2 s) first, and for those 2 s nobody drew anything: the
/// kernel's fallback framebuffer showed the boot image instead (seen on the
/// DK2 when unplugging the monitor). Now the gap is just the ~0.3 s the UI
/// takes to start.
///
/// Started again from its real path (/usr/bin/ui-layer): Linux names a
/// process after the file it was started from, so starting it through
/// /proc/self/exe made it show up as "exe" in ps/top/systemctl (seen on
/// the DK2). /proc/self/exe is only the fallback, for when the file was
/// replaced on disk meanwhile (make deploy-ui): the kernel then reports the
/// path as ".../ui-layer (deleted)", but /proc/self/exe still reaches this
/// very program. exec() only returns if it failed; then quitting for
/// systemd to restart is the last resort.
fn restart_on_other_screen() -> ! {
    use std::os::unix::process::CommandExt;
    let program = std::env::current_exe()
        .ok()
        .filter(|path| !path.to_string_lossy().ends_with(" (deleted)"))
        .unwrap_or_else(|| "/proc/self/exe".into());
    let error = std::process::Command::new(program).exec();
    println!("ui_layer: couldn't restart in place ({error}), quitting for systemd to restart the UI");
    std::process::exit(EXIT_SCREEN_CHANGED);
}

/// Shows the hub's settings (issue #39): the design preset into the Theme
/// (theme.rs), and the values the Settings/Appearance pages display.
/// `shown_dark`: which palette the accent swatches were last made for, so
/// they're only rebuilt when the mode actually flips (this runs every
/// CLOCK_CHECK). Setting a Slint property to the value it already has
/// changes nothing, so the Theme itself can be applied every time.
fn show_settings(ui: &AppWindow, settings: &ws_client::HubSettings, shown_dark: &mut Option<bool>) {
    let (hour, time) = zones::local_time(&settings.time_zone);
    let dark = theme::is_dark(&settings.mode, hour);
    let appearance = theme::Appearance {
        mode: settings.mode.clone(),
        accent: settings.accent.clone(),
        density: settings.density.clone(),
    };
    theme::apply(ui, &appearance, dark);

    ui.set_setting_mode(settings.mode.clone().into());
    ui.set_setting_accent(settings.accent.clone().into());
    ui.set_setting_density(settings.density.clone().into());
    ui.set_setting_time_zone(settings.time_zone.clone().into());
    ui.set_local_time(time.into());
    let accents = theme::accents(dark);
    let name = accents.iter().find(|(id, _, _)| *id == settings.accent).map_or("", |(_, name, _)| name.as_str());
    ui.set_setting_accent_name(name.into());
    if *shown_dark != Some(dark) {
        let rows: Vec<AccentItem> = accents
            .into_iter()
            .map(|(id, name, color)| AccentItem { id: id.into(), name: name.into(), color })
            .collect();
        ui.set_accents(std::rc::Rc::new(slint::VecModel::from(rows)).into());
        *shown_dark = Some(dark);
    }
}

/// zones.rs's rows -> the model the time zone page shows.
fn zone_rows(zones: Vec<zones::Zone>) -> slint::ModelRc<ZoneItem> {
    let rows: Vec<ZoneItem> = zones
        .into_iter()
        .map(|z| ZoneItem { label: z.label.into(), value: z.value.into(), marked: z.marked })
        .collect();
    std::rc::Rc::new(slint::VecModel::from(rows)).into()
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
/// backend_daemon last reported them, and the cards app.slint draws --
/// split into rows of `columns` cards (issue #38: 1 on the touchscreen, 2
/// or 3 on a monitor; app.slint decides by the window's width).
///
/// Each row is its own model (VecModel), all held by one model of rows,
/// for the program's whole life. On a change only the cards that differ
/// are updated (set_row_data); the rows are only rebuilt when devices
/// appear, disappear or change order (or the column count changes) -- so
/// the list doesn't jump back to the top every time a lamp is switched.
struct DeviceRows {
    devices: std::collections::BTreeMap<String, ws_client::Device>,
    /// What app.slint shows: one entry per row of cards.
    grid: std::rc::Rc<slint::VecModel<slint::ModelRc<DeviceItem>>>,
    /// The same rows' own models, to update a single card.
    rows: Vec<std::rc::Rc<slint::VecModel<DeviceItem>>>,
    /// The id of each card, in order (row by row).
    ids: Vec<String>,
    /// How many columns the rows were built for.
    columns: usize,
    /// To read the column count (weak: see the callbacks in main).
    ui: slint::Weak<AppWindow>,
}

impl DeviceRows {
    fn new(ui: &AppWindow) -> Self {
        let grid = std::rc::Rc::new(slint::VecModel::default());
        ui.set_device_rows(grid.clone().into());
        DeviceRows {
            devices: Default::default(),
            grid,
            rows: Vec::new(),
            ids: Vec::new(),
            columns: 0,
            ui: ui.as_weak(),
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

    /// Brings the rows in line with `devices`: sorted by room, then name
    /// (so a room's devices stay together), then id.
    fn refresh(&mut self) {
        use slint::Model;
        let columns = self.ui.upgrade().map_or(1, |ui| ui.get_columns().max(1) as usize);
        let mut sorted: Vec<&ws_client::Device> = self.devices.values().collect();
        sorted.sort_by(|a, b| (&a.room, &a.name, &a.id).cmp(&(&b.room, &b.name, &b.id)));
        let ids: Vec<String> = sorted.iter().map(|d| d.id.clone()).collect();
        let items: Vec<DeviceItem> = sorted.into_iter().map(device_item).collect();
        if ids == self.ids && columns == self.columns {
            // Same cards in the same places: card i is in row i / columns,
            // at position i % columns.
            for (i, item) in items.into_iter().enumerate() {
                let row = &self.rows[i / columns];
                if row.row_data(i % columns).as_ref() != Some(&item) {
                    row.set_row_data(i % columns, item);
                }
            }
        } else {
            self.rows = items
                .chunks(columns)
                .map(|cards| std::rc::Rc::new(slint::VecModel::from(cards.to_vec())))
                .collect();
            self.grid
                .set_vec(self.rows.iter().map(|row| slint::ModelRc::from(row.clone())).collect::<Vec<_>>());
            self.ids = ids;
            self.columns = columns;
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
