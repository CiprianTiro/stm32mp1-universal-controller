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
mod ir_finder;
mod automation_text;
mod ir_layout;
mod pointer;
mod setup;
mod theme;
mod tiles;
mod vacuum_map;
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
/// Live video: no frame for this long, the watching is asked for again
/// (the camera's stream broke off, or the backend restarted).
const LIVE_STALL: Duration = Duration::from_secs(4);

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
    // turned into the rows app.slint shows. See DeviceRows. Shared (Rc +
    // RefCell) with the device details page's callbacks (issue #40).
    let rows = std::rc::Rc::new(std::cell::RefCell::new(DeviceRows::new(&ui)));
    // Adding and setting up devices (issue #40): the wizard pages' state,
    // see setup.rs.
    let wizard = std::rc::Rc::new(std::cell::RefCell::new(setup::Setup::default()));
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

    // A TV's volume/mute/input (issue #40): the media capability, set as a
    // whole.
    let tx = request_tx.clone();
    ui.on_set_media(move |id, volume, muted, input| {
        let _ = tx.send(ws_client::Request::Command {
            id: id.to_string(),
            capability: "media".into(),
            value: serde_json::json!({ "volume": volume, "muted": muted, "input": input.as_str() }),
        });
    });

    // ---- Issue #77: covers, climate, locks ------------------------------
    // A cover's open / close / stop are actions; its position, a
    // climate's mode / target / fan and a lock's state are commands. A
    // failed one shows under the list (Update::CommandFailed, or the
    // action's answer below).
    let tx = request_tx.clone();
    ui.on_cover_action(move |id, action| {
        let _ = tx.send(ws_client::Request::DeviceAction {
            id: id.to_string(),
            capability: "cover".into(),
            name: action.to_string(),
            args: serde_json::json!({}),
        });
    });
    // Issue #43: a camera's pan/tilt step and preset positions (ONVIF).
    let tx = request_tx.clone();
    ui.on_camera_move(move |id, pan, tilt| {
        let _ = tx.send(ws_client::Request::DeviceAction {
            id: id.to_string(),
            capability: "camera".into(),
            name: "move".into(),
            args: serde_json::json!({ "pan": pan, "tilt": tilt }),
        });
    });
    let tx = request_tx.clone();
    ui.on_camera_preset(move |id, token| {
        let _ = tx.send(ws_client::Request::DeviceAction {
            id: id.to_string(),
            capability: "camera".into(),
            name: "preset".into(),
            args: serde_json::json!({ "token": token.as_str() }),
        });
    });
    // Issue #74: a device's own setting (an EZVIZ plug's status light).
    let tx = request_tx.clone();
    ui.on_set_option(move |id, option, on| {
        let _ = tx.send(ws_client::Request::DeviceAction {
            id: id.to_string(),
            capability: "options".into(),
            name: "set".into(),
            args: serde_json::json!({ "id": option.as_str(), "on": on }),
        });
    });
    let tx = request_tx.clone();
    ui.on_set_option_value(move |id, option, value| {
        let _ = tx.send(ws_client::Request::DeviceAction {
            id: id.to_string(),
            capability: "options".into(),
            name: "set".into(),
            args: serde_json::json!({ "id": option.as_str(), "value": value.as_str() }),
        });
    });
    // Issue #74: a vacuum's start / pause / dock / locate are actions too.
    let tx = request_tx.clone();
    ui.on_vacuum_action(move |id, action, arg| {
        let arg = arg.to_string();
        let args = match action.as_str() {
            "set_fan" => serde_json::json!({ "fan": arg }),
            "set_water" => serde_json::json!({ "water": arg }),
            "set_mop" => serde_json::json!({ "mop": arg }),
            "reset_part" => serde_json::json!({ "part": arg }),
            "clean_rooms" => serde_json::json!({ "rooms": [arg.parse::<u32>().unwrap_or(0)] }),
            _ => serde_json::json!({}),
        };
        let _ = tx.send(ws_client::Request::DeviceAction {
            id: id.to_string(),
            capability: "vacuum".into(),
            name: action.to_string(),
            args,
        });
    });
    let tx = request_tx.clone();
    ui.on_set_cover_position(move |id, position| {
        let _ = tx.send(ws_client::Request::Command {
            id: id.to_string(),
            capability: "cover".into(),
            value: serde_json::json!({ "position": position }),
        });
    });
    let tx = request_tx.clone();
    ui.on_set_climate(move |id, mode, target, fan| {
        // - / + add the step to a float: round to 0.1 so 21.5 + 0.5 is 22,
        // not 21.999999.
        let target = (f64::from(target) * 10.0).round() / 10.0;
        let mut value = serde_json::json!({ "mode": mode.as_str(), "target": target });
        if !fan.is_empty() {
            value["fan"] = fan.as_str().into();
        }
        let _ = tx.send(ws_client::Request::Command { id: id.to_string(), capability: "climate".into(), value });
    });
    // Unlocking only comes from the card's "Unlock …?" question, so it's
    // sent as confirmed -- backend_daemon refuses an unlock without it.
    // Issue #95: the Devices page's filter chips, a tile's controls page,
    // "Turn all off", the star.
    let (ui_weak, r) = (ui.as_weak(), rows.clone());
    ui.on_pick_filter(move |id| {
        ui_weak.unwrap().set_device_filter(id);
        r.borrow_mut().refresh();
    });
    let (ui_weak, r, tx) = (ui.as_weak(), rows.clone(), request_tx.clone());
    ui.on_open_controls(move |id| {
        let ui = ui_weak.unwrap();
        if r.borrow().show_controls(&ui, id.as_str()) {
            ui.set_device_message("".into());
            ui.set_device_back(PAGE_CONTROLS);
            ui.set_page(PAGE_CONTROLS);
            // Issue #74: a vacuum's map, fetched now (it takes a moment:
            // it comes through the vendor's cloud).
            if ui.get_controls_item().vacuum_has_map {
                ui.set_controls_map_state("Loading the map\u{2026}".into());
                request_map(&tx, id.as_str());
            }
            // Issue #43: a camera's picture (the first takes a few
            // seconds: the stream's next key frame).
            if ui.get_controls_item().has_camera {
                ui.set_controls_live(false);
                ui.set_controls_map_state("Loading the picture\u{2026}".into());
                request_picture(&tx, id.as_str(), false);
            }
        }
    });
    // While a vacuum's page is open and it's on the move, its map follows.
    let (map_ui, map_tx) = (ui.as_weak(), request_tx.clone());
    let map_timer = slint::Timer::default();
    map_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(10), move || {
        let Some(ui) = map_ui.upgrade() else { return };
        let item = ui.get_controls_item();
        let moving = matches!(item.vacuum_state.as_str(), "cleaning" | "returning" | "paused");
        if ui.get_page() == PAGE_CONTROLS && item.vacuum_has_map && moving {
            request_map(&map_tx, item.id.as_str());
        }
    });
    // A camera's picture follows while its page is open: a still every
    // 2 s (a new one only asked for once the last one came, or 10 s
    // passed: requests never pile up) -- or, in Live, real video: the
    // backend sends frames of exactly the picture's size (camera.rs) into
    // ws_client::VIDEO, shown by the poll timer below. Watching is asked
    // again when no frame came for a while (the stream broke off), and
    // ended when Live is turned off or the page closes.
    let picture_waiting: std::rc::Rc<std::cell::Cell<Option<std::time::Instant>>> = Default::default();
    // The camera watched live, and when its last frame came (or the
    // watching was asked for).
    let watching: std::rc::Rc<std::cell::RefCell<Option<(String, std::time::Instant)>>> = Default::default();
    let (picture_ui, picture_tx, waiting, watched) = (ui.as_weak(), request_tx.clone(), picture_waiting.clone(), watching.clone());
    let picture_timer = slint::Timer::default();
    let picture_ticks = std::cell::Cell::new(0u32);
    picture_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(250), move || {
        let Some(ui) = picture_ui.upgrade() else { return };
        let item = ui.get_controls_item();
        let on_camera = ui.get_page() == PAGE_CONTROLS && item.has_camera;
        let live = on_camera && ui.get_controls_live();
        let mut watched = watched.borrow_mut();
        if live {
            let stale = watched.as_ref().is_none_or(|(id, at)| id != item.id.as_str() || at.elapsed() > LIVE_STALL);
            if stale {
                let scale = ui.window().scale_factor();
                let size = |l: f32| ((l * scale) as u32).min(u16::MAX as u32) as u16;
                let _ = picture_tx.send(ws_client::Request::WatchCamera {
                    id: item.id.to_string(),
                    width: size(ui.get_controls_picture_width()),
                    height: size(ui.get_controls_picture_height()),
                });
                *watched = Some((item.id.to_string(), std::time::Instant::now()));
            }
            return;
        }
        if watched.take().is_some() {
            let _ = picture_tx.send(ws_client::Request::UnwatchCamera);
        }
        if !on_camera {
            return;
        }
        picture_ticks.set(picture_ticks.get().wrapping_add(1));
        let due = picture_ticks.get() % 8 == 0;
        let free = waiting.get().is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(10));
        if due && free {
            waiting.set(Some(std::time::Instant::now()));
            request_picture(&picture_tx, item.id.as_str(), false);
        }
    });
    let (tx, r) = (request_tx.clone(), rows.clone());
    ui.on_all_off(move || {
        let rows = r.borrow();
        for device in rows.devices.values() {
            let on = device.capabilities.switch.as_ref().is_some_and(|s| s.on);
            if on && tiles::reachable(device) && tiles::is_light(device, &rows.templates) {
                let _ = tx.send(ws_client::Request::Command {
                    id: device.id.clone(),
                    capability: "switch".into(),
                    value: serde_json::json!({ "on": false }),
                });
            }
        }
    });
    let tx = request_tx.clone();
    ui.on_set_favourite(move |id, favourite| {
        let _ = tx.send(ws_client::Request::UpdateDeviceInfo { id: id.to_string(), name: None, room: None, favourite: Some(favourite) });
    });

    // Issue #85: an IR light's colour chip, and its brightness -/+ (a
    // press of one of its remote's buttons, for that card's device).
    let tx = request_tx.clone();
    ui.on_set_color(move |id, hex| {
        let value = serde_json::json!({ "hex": hex.as_str() });
        let _ = tx.send(ws_client::Request::Command { id: id.to_string(), capability: "color".into(), value });
    });
    let tx = request_tx.clone();
    ui.on_device_press(move |id, button| {
        let _ = tx.send(ws_client::Request::DeviceAction {
            id: id.to_string(),
            capability: "remote".into(),
            name: "press".into(),
            args: serde_json::json!({ "button": button.as_str() }),
        });
    });
    let tx = request_tx.clone();
    ui.on_set_lock(move |id, locked| {
        let value = if locked {
            serde_json::json!({ "state": "locked" })
        } else {
            serde_json::json!({ "state": "unlocked", "confirmed": true })
        };
        let _ = tx.send(ws_client::Request::Command { id: id.to_string(), capability: "lock".into(), value });
    });

    // ---- Scenes (issue #47, scenes.slint) -------------------------------
    // The scenes as backend_daemon last sent them (for names in messages),
    // and the devices offered by "Save current state" (their ids, in the
    // order of the pick list).
    let scenes: std::rc::Rc<std::cell::RefCell<Vec<ws_client::Scene>>> = Default::default();
    let tx = request_tx.clone();
    let ui_weak = ui.as_weak();
    ui.on_run_scene(move |id| {
        let ui = ui_weak.unwrap();
        // One at a time: the buttons wait for its answer.
        if !ui.get_scene_running().is_empty() {
            return;
        }
        ui.set_scene_running(id.clone());
        let _ = tx.send(ws_client::Request::RunScene { id: id.to_string() });
    });
    let tx = request_tx.clone();
    ui.on_delete_scene(move |id| {
        let _ = tx.send(ws_client::Request::DeleteScene { id: id.to_string() });
    });
    let ui_weak = ui.as_weak();
    ui.on_open_scenes(move || {
        let ui = ui_weak.unwrap();
        ui.set_scenes_message("".into());
        ui.set_page(PAGE_SCENES);
    });
    // "Save current state": every device a scene can set, none picked yet.
    let ui_weak = ui.as_weak();
    let rows_for_pick = rows.clone();
    ui.on_open_capture(move || {
        let ui = ui_weak.unwrap();
        let rows = rows_for_pick.borrow();
        let mut devices: Vec<&ws_client::Device> = rows.devices.values().filter(|d| capturable(d)).collect();
        devices.sort_by(|a, b| (&a.room, &a.name).cmp(&(&b.room, &b.name)));
        let items: Vec<PickItem> = devices
            .into_iter()
            .map(|d| PickItem { id: d.id.clone().into(), name: d.name.clone().into(), detail: state_text(d).into(), selected: false })
            .collect();
        // A new model each time: the page sees `devices` change and starts
        // over at its first view.
        ui.set_pick_devices(std::rc::Rc::new(slint::VecModel::from(items)).into());
        ui.set_pick_count(0);
        ui.set_scene_name("".into());
        ui.set_capture_message("".into());
        ui.set_capture_busy(false);
        ui.set_page(PAGE_CAPTURE);
    });
    let ui_weak = ui.as_weak();
    ui.on_toggle_pick(move |index| {
        use slint::Model;
        let ui = ui_weak.unwrap();
        let model = ui.get_pick_devices();
        if let Some(mut item) = model.row_data(index as usize) {
            item.selected = !item.selected;
            model.set_row_data(index as usize, item);
        }
        ui.set_pick_count(model.iter().filter(|d| d.selected).count() as i32);
    });
    let tx = request_tx.clone();
    let ui_weak = ui.as_weak();
    ui.on_capture_scene(move |name| {
        use slint::Model;
        let ui = ui_weak.unwrap();
        let devices: Vec<String> = ui.get_pick_devices().iter().filter(|d| d.selected).map(|d| d.id.to_string()).collect();
        ui.set_capture_message("".into());
        ui.set_capture_busy(true);
        let _ = tx.send(ws_client::Request::CaptureScene { name: name.trim().to_string(), devices });
    });

    // ---- Adding and setting up devices (issue #40) ----------------------
    // Each callback hands the tap to setup.rs (which sends the request);
    // backend_daemon's answer arrives as an Update in the loop below.
    let tx = request_tx.clone();
    let ui_weak = ui.as_weak();
    ui.on_open_add_device(move || {
        let ui = ui_weak.unwrap();
        ui.set_add_message("".into());
        ui.set_page(setup::PAGE_ADD);
        // Fresh lists, and a search round now: what's shown is current.
        let _ = tx.send(ws_client::Request::ListTemplates);
        let _ = tx.send(ws_client::Request::DiscoverNow);
        let _ = tx.send(ws_client::Request::ListFound);
    });
    let tx = request_tx.clone();
    ui.on_search_devices(move || {
        let _ = tx.send(ws_client::Request::DiscoverNow);
    });
    let (tx, ui_weak, w) = (request_tx.clone(), ui.as_weak(), wizard.clone());
    ui.on_pick_template(move |id| {
        ui_weak.unwrap().set_add_message("".into());
        w.borrow_mut().start_new(&id);
        let _ = tx.send(ws_client::Request::WizardStart { template: id.to_string(), variant: None, found: None });
    });
    let (tx, ui_weak, w) = (request_tx.clone(), ui.as_weak(), wizard.clone());
    ui.on_add_found(move |template, address| {
        ui_weak.unwrap().set_add_message("".into());
        w.borrow_mut().start_new(&template);
        let _ = tx.send(ws_client::Request::WizardStart {
            template: template.to_string(),
            variant: None,
            found: Some(address.to_string()),
        });
    });
    let (tx, ui_weak, w) = (request_tx.clone(), ui.as_weak(), wizard.clone());
    ui.on_wizard_next(move || w.borrow_mut().next(&ui_weak.unwrap(), &tx));
    let (tx, ui_weak, w) = (request_tx.clone(), ui.as_weak(), wizard.clone());
    ui.on_wizard_retry(move || w.borrow_mut().retry(&ui_weak.unwrap(), &tx));
    let (tx, ui_weak, w) = (request_tx.clone(), ui.as_weak(), wizard.clone());
    ui.on_wizard_back(move || w.borrow_mut().back(&ui_weak.unwrap(), &tx));
    let (tx, ui_weak, w) = (request_tx.clone(), ui.as_weak(), wizard.clone());
    ui.on_wizard_pick_found(move |address| w.borrow_mut().pick_found(&ui_weak.unwrap(), &tx, Some(address.to_string())));
    let (tx, ui_weak, w) = (request_tx.clone(), ui.as_weak(), wizard.clone());
    ui.on_wizard_search(move || w.borrow_mut().pick_found(&ui_weak.unwrap(), &tx, None));
    let (ui_weak, w) = (ui.as_weak(), wizard.clone());
    ui.on_wizard_edit(move |i| w.borrow_mut().edit(&ui_weak.unwrap(), i.max(0) as usize));
    let (ui_weak, w) = (ui.as_weak(), wizard.clone());
    ui.on_wizard_edit_done(move || w.borrow_mut().edit_done(&ui_weak.unwrap()));
    let (tx, ui_weak, w) = (request_tx.clone(), ui.as_weak(), wizard.clone());
    ui.on_wizard_pick(move |i, value| w.borrow_mut().pick(&ui_weak.unwrap(), &tx, i.max(0) as usize, value.to_string()));
    let (tx, w) = (request_tx.clone(), wizard.clone());
    ui.on_wizard_switch_variant(move |variant| w.borrow_mut().switch_variant(&tx, variant.to_string()));

    // ---- A TV's remote (issue #44, remote.slint) --------------------------
    // Every tap is a device_action for the device on show (remote-id); lists
    // come back as Update::DeviceAction below. The apps / channels list is
    // one model for the page's lifetime: a channel page is APPENDED to it
    // ("Show more"), not rebuilt.
    let remote_items = std::rc::Rc::new(slint::VecModel::<RemoteItem>::default());
    ui.set_remote_items(remote_items.clone().into());
    let (ui_weak, r) = (ui.as_weak(), rows.clone());
    ui.on_open_remote(move |id| {
        let ui = ui_weak.unwrap();
        if let Some(device) = r.borrow().devices.get(id.as_str()) {
            // A remote that learns (an IR blaster, issue #42) has its own
            // page: taught buttons instead of a TV's fixed layout.
            if device.capabilities.remote.as_ref().is_some_and(|r| r.learn) {
                show_ir_remote(&ui, device);
                ui.set_ir_view(0);
                ui.set_ir_message("".into());
                ui.set_page(PAGE_IR_REMOTE);
                return;
            }
            show_remote(&ui, device);
            ui.set_remote_view(0);
            ui.set_remote_message("".into());
            ui.set_remote_text("".into());
            ui.set_page(PAGE_REMOTE);
        }
    });
    // Sends one action for the remote's device.
    let remote_action = {
        let tx = request_tx.clone();
        let ui_weak = ui.as_weak();
        move |capability: &str, name: &str, args: serde_json::Value| {
            let ui = ui_weak.unwrap();
            let _ = tx.send(ws_client::Request::DeviceAction {
                id: ui.get_remote_id().to_string(),
                capability: capability.into(),
                name: name.into(),
                args,
            });
        }
    };
    let act = remote_action.clone();
    ui.on_remote_press(move |button| act("remote", "press", serde_json::json!({ "button": button.as_str() })));
    let (act, ui_weak, items) = (remote_action.clone(), ui.as_weak(), remote_items.clone());
    ui.on_remote_open_apps(move || {
        let ui = ui_weak.unwrap();
        ui.set_remote_view(1);
        ui.set_remote_loading(true);
        items.set_vec(Vec::new());
        act("media", "apps", serde_json::json!({}));
    });
    let (act, ui_weak, items) = (remote_action.clone(), ui.as_weak(), remote_items.clone());
    ui.on_remote_open_channels(move || {
        let ui = ui_weak.unwrap();
        // The search first, then the view: the page asks again whenever
        // the search changes while channels show.
        ui.set_remote_query("".into());
        ui.set_remote_total(0);
        ui.set_remote_view(2);
        ui.set_remote_loading(true);
        items.set_vec(Vec::new());
        act("media", "channels", serde_json::json!({ "limit": CHANNEL_PAGE }));
    });
    // Every change of the search: its first page.
    let (act, ui_weak) = (remote_action.clone(), ui.as_weak());
    ui.on_remote_search(move |query| {
        ui_weak.unwrap().set_remote_loading(true);
        act("media", "channels", serde_json::json!({ "query": query.as_str(), "limit": CHANNEL_PAGE }));
    });
    // "Show more": the next page of the same search.
    let (act, ui_weak, items) = (remote_action.clone(), ui.as_weak(), remote_items.clone());
    ui.on_remote_more(move || {
        use slint::Model;
        let ui = ui_weak.unwrap();
        act(
            "media",
            "channels",
            serde_json::json!({ "query": ui.get_remote_query().as_str(), "offset": items.row_count(), "limit": CHANNEL_PAGE }),
        );
    });
    let (act, ui_weak) = (remote_action.clone(), ui.as_weak());
    ui.on_remote_pick(move |id| {
        let ui = ui_weak.unwrap();
        if ui.get_remote_view() == 1 {
            act("media", "launch", serde_json::json!({ "app": id.as_str() }));
        } else {
            act("media", "tune", serde_json::json!({ "channel": id.as_str() }));
        }
        ui.set_remote_view(0);
    });
    let (act, ui_weak) = (remote_action.clone(), ui.as_weak());
    ui.on_remote_send_text(move || {
        let ui = ui_weak.unwrap();
        let text = ui.get_remote_text().to_string();
        if !text.is_empty() {
            // Typed, then Enter: what a phone keyboard's "Done" does.
            act("remote", "type", serde_json::json!({ "text": text }));
            act("remote", "submit", serde_json::json!({}));
        }
        ui.set_remote_text("".into());
        ui.set_remote_view(0);
    });

    // ---- An IR device's remote (issue #42, ir_remote.slint) --------------
    // The same device_action as the TV's remote (remote-id), with the
    // learning actions. Answers come back as Update::DeviceAction below.
    let act = remote_action.clone();
    ui.on_ir_press(move |button| act("remote", "press", serde_json::json!({ "button": button.as_str() })));
    let (act, ui_weak) = (remote_action.clone(), ui.as_weak());
    ui.on_ir_learn(move |button| {
        ui_weak.unwrap().set_ir_message("".into());
        act("remote", "learn", serde_json::json!({ "button": button.as_str(), "timeout_s": IR_LEARN_SECONDS }));
    });
    let act = remote_action.clone();
    ui.on_ir_forget(move |button| act("remote", "forget", serde_json::json!({ "button": button.as_str() })));
    let act = remote_action.clone();
    ui.on_ir_rename(move |button, to| {
        act("remote", "rename", serde_json::json!({ "button": button.as_str(), "to": to.trim() }))
    });
    // Issue #85: "Use as a light", and the strip's colour order.
    let act = remote_action.clone();
    ui.on_ir_set_light(move |on, order| {
        let mut args = serde_json::json!({ "enabled": on });
        if !order.is_empty() {
            args["order"] = order.as_str().into();
        }
        act("remote", "light", args)
    });

    // Its code finder for a lost remote (issue #82, ir_finder.rs): the
    // finder keeps where it is; each tap gives the next action to send.
    let ir_items = std::rc::Rc::new(slint::VecModel::<RemoteItem>::default());
    ui.set_ir_items(ir_items.clone().into());
    let finder = std::rc::Rc::new(std::cell::RefCell::new(ir_finder::Finder::default()));
    let (act, ui_weak, f, items) = (remote_action.clone(), ui.as_weak(), finder.clone(), ir_items.clone());
    ui.on_ir_find(move || {
        let (name, args) = f.borrow_mut().start(&ui_weak.unwrap(), &items);
        act("remote", name, args);
    });
    let (act, ui_weak, f, items) = (remote_action.clone(), ui.as_weak(), finder.clone(), ir_items.clone());
    ui.on_ir_pick(move |id, label| {
        if let Some((name, args)) = f.borrow_mut().pick(&ui_weak.unwrap(), &items, &id, &label) {
            act("remote", name, args);
        }
    });
    let (act, ui_weak, f) = (remote_action.clone(), ui.as_weak(), finder.clone());
    ui.on_ir_answer(move |yes| {
        if let Some((name, args)) = f.borrow_mut().answer(&ui_weak.unwrap(), yes) {
            act("remote", name, args);
        }
    });
    let (act, ui_weak, f) = (remote_action.clone(), ui.as_weak(), finder.clone());
    ui.on_ir_resend(move || {
        if let Some((name, args)) = f.borrow().resend(&ui_weak.unwrap()) {
            act("remote", name, args);
        }
    });
    let finder_act = remote_action.clone();

    // A device's details page, and what can be done from it.
    let (ui_weak, r, w) = (ui.as_weak(), rows.clone(), wizard.clone());
    ui.on_open_device(move |id| {
        let ui = ui_weak.unwrap();
        if let Some(device) = r.borrow().devices.get(id.as_str()) {
            show_device_page(&ui, device, &w.borrow());
            show_power_link(&ui, device, &r.borrow().devices, &r.borrow().templates);
            ui.set_dev_power_status("".into());
            ui.set_dev_power_busy(false);
            ui.set_dev_editing_power(false);
            // "Name and room" offers the rooms in use.
            let mut rooms: Vec<String> = r.borrow().devices.values().map(|d| d.room.clone()).filter(|r| !r.is_empty()).collect();
            rooms.sort();
            rooms.dedup();
            ui.set_dev_rooms(std::rc::Rc::new(slint::VecModel::from(rooms.into_iter().map(Into::into).collect::<Vec<slint::SharedString>>())).into());
            ui.set_page(setup::PAGE_DEVICE);
        }
    });
    // Issue #99: "Powered by" saved ("" = no plug), and a learning step.
    let tx = request_tx.clone();
    ui.on_device_save_power(move |id, plug, cut, delay| {
        let _ = tx.send(ws_client::Request::SetPowerLink {
            id: id.to_string(),
            plug: (!plug.is_empty()).then(|| plug.to_string()),
            cut_power: cut,
            start_delay_s: Some(delay.max(0) as u64),
        });
    });
    let (tx, ui_weak) = (request_tx.clone(), ui.as_weak());
    ui.on_device_learn_power(move |id, step| {
        let ui = ui_weak.unwrap();
        ui.set_dev_power_busy(true);
        ui.set_dev_power_status(
            match step.as_str() {
                "forget" => "",
                // A cloud plug (EZVIZ) shows a change only ~30 s later.
                "off" => "Measuring\u{2026} (with a cloud plug like EZVIZ: about half a minute)",
                _ => "Switching it on and measuring\u{2026} (with a cloud plug: about half a minute)",
            }
            .into(),
        );
        let _ = tx.send(ws_client::Request::LearnPower { id: id.to_string(), step: step.to_string() });
    });
    // "Name and room" saved: an empty name keeps the old one.
    let tx = request_tx.clone();
    ui.on_device_save_info(move |id, name, room| {
        let name = name.trim();
        let _ = tx.send(ws_client::Request::UpdateDeviceInfo {
            id: id.to_string(),
            name: (!name.is_empty()).then(|| name.to_string()),
            room: Some(room.trim().to_string()),
            favourite: None,
        });
    });
    let (tx, ui_weak, r, w) = (request_tx.clone(), ui.as_weak(), rows.clone(), wizard.clone());
    ui.on_device_reauth(move |id| {
        let name = r.borrow().devices.get(id.as_str()).map_or_else(|| id.to_string(), |d| d.name.clone());
        ui_weak.unwrap().set_dev_message("".into());
        w.borrow_mut().start_existing(format!("Pair again: {name}"));
        let _ = tx.send(ws_client::Request::WizardReauth { device: id.to_string() });
    });
    let (tx, ui_weak, r, w) = (request_tx.clone(), ui.as_weak(), rows.clone(), wizard.clone());
    ui.on_device_reconfigure(move |id| {
        let name = r.borrow().devices.get(id.as_str()).map_or_else(|| id.to_string(), |d| d.name.clone());
        ui_weak.unwrap().set_dev_message("".into());
        w.borrow_mut().start_existing(format!("Settings: {name}"));
        let _ = tx.send(ws_client::Request::WizardReconfigure { device: id.to_string() });
    });
    let tx = request_tx.clone();
    ui.on_device_remove(move |id| {
        let _ = tx.send(ws_client::Request::RemoveDevice { id: id.to_string() });
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
    // Issue #74: Settings > Accounts.
    let tx = request_tx.clone();
    ui.on_open_accounts(move || {
        let _ = tx.send(ws_client::Request::ListAccounts);
    });
    let tx = request_tx.clone();
    ui.on_sign_out_account(move |id| {
        let _ = tx.send(ws_client::Request::RemoveAccount { id: id.to_string() });
    });
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
    ui.on_set_mode(set(|mode| ws_client::Request::SetSettings { mode: Some(mode), accent: None, density: None, time_zone: None, latitude: None, longitude: None }));
    ui.on_set_accent(set(|accent| ws_client::Request::SetSettings { mode: None, accent: Some(accent), density: None, time_zone: None, latitude: None, longitude: None }));
    ui.on_set_density(set(|density| ws_client::Request::SetSettings { mode: None, accent: None, density: Some(density), time_zone: None, latitude: None, longitude: None }));

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
                latitude: None,
                longitude: None,
            });
            ui.set_page(4);
        }
    });

    // ---- Automations (issue #47, automations.slint) ---------------------
    // The automations as backend_daemon last sent them (JSON: only the
    // parts automation_text.rs reads), and the log, newest last.
    let automations: std::rc::Rc<std::cell::RefCell<Vec<serde_json::Value>>> = Default::default();
    let log: std::rc::Rc<std::cell::RefCell<Vec<ws_client::LogEntry>>> = Default::default();
    let ui_weak = ui.as_weak();
    ui.on_open_automations(move || {
        let ui = ui_weak.unwrap();
        ui.set_automations_message("".into());
        ui.set_page(PAGE_AUTOMATIONS);
    });
    let tx = request_tx.clone();
    ui.on_set_automation_enabled(move |id, enabled| {
        let _ = tx.send(ws_client::Request::SetAutomationEnabled { id: id.to_string(), enabled });
    });
    let tx = request_tx.clone();
    let ui_weak = ui.as_weak();
    ui.on_run_automation(move |id| {
        let ui = ui_weak.unwrap();
        if !ui.get_automation_running().is_empty() {
            return;
        }
        ui.set_automation_running(id.clone());
        let _ = tx.send(ws_client::Request::RunAutomation { id: id.to_string() });
    });
    let tx = request_tx.clone();
    ui.on_delete_automation(move |id| {
        let _ = tx.send(ws_client::Request::DeleteAutomation { id: id.to_string() });
    });
    let ui_weak = ui.as_weak();
    ui.on_open_log(move || ui_weak.unwrap().set_page(PAGE_LOG));
    // The editor: fresh lists (which also makes it start over), the
    // devices that switch on and off.
    let ui_weak = ui.as_weak();
    let (rows_for_editor, scenes_for_editor) = (rows.clone(), scenes.clone());
    ui.on_new_automation(move || {
        let ui = ui_weak.unwrap();
        let rows = rows_for_editor.borrow();
        let mut switchable: Vec<&ws_client::Device> = rows.devices.values().filter(|d| d.capabilities.switch.is_some()).collect();
        switchable.sort_by(|a, b| (&a.room, &a.name).cmp(&(&b.room, &b.name)));
        let devices: Vec<SceneItem> = switchable
            .into_iter()
            .map(|d| SceneItem { id: d.id.clone().into(), name: d.name.clone().into(), detail: d.room.clone().into() })
            .collect();
        ui.set_switch_devices(std::rc::Rc::new(slint::VecModel::from(devices)).into());
        let scenes: Vec<SceneItem> = scenes_for_editor
            .borrow()
            .iter()
            .map(|s| SceneItem { id: s.id.clone().into(), name: s.name.clone().into(), detail: "".into() })
            .collect();
        ui.set_editor_scenes(std::rc::Rc::new(slint::VecModel::from(scenes)).into());
        ui.set_automation_name("".into());
        ui.set_automation_message("".into());
        ui.set_automation_busy(false);
        ui.set_page(PAGE_NEW_AUTOMATION);
    });
    ui.on_suggest_automation_name(|when, hour, minute, offset, device, scene| {
        automation_text::suggested_name(when, hour, minute, offset, &device, &scene).into()
    });
    let tx = request_tx.clone();
    let ui_weak = ui.as_weak();
    ui.on_save_automation(move |name, when, hour, minute, offset, days, device, scene| {
        let ui = ui_weak.unwrap();
        ui.set_automation_message("".into());
        ui.set_automation_busy(true);
        let automation = automation_text::simple_automation(&name, when, hour, minute, offset, days, &device, &scene);
        let _ = tx.send(ws_client::Request::SaveAutomation { automation });
    });
    // The location page, from Settings (4) or the editor (18).
    let ui_weak = ui.as_weak();
    ui.on_open_location(move |back| {
        let ui = ui_weak.unwrap();
        ui.set_location_back(back);
        ui.set_location_text(ui.get_location_current());
        ui.set_location_message("".into());
        ui.set_page(PAGE_LOCATION);
    });
    let tx = request_tx.clone();
    let ui_weak = ui.as_weak();
    ui.on_save_location(move |text| {
        let ui = ui_weak.unwrap();
        match automation_text::parse_location(&text) {
            Ok((latitude, longitude)) => {
                let _ = tx.send(ws_client::Request::SetSettings {
                    mode: None,
                    accent: None,
                    density: None,
                    time_zone: None,
                    latitude: Some(latitude),
                    longitude: Some(longitude),
                });
                // The new value shows once backend_daemon confirms
                // (Update::Settings).
                ui.set_page(ui.get_location_back());
            }
            Err(why) => ui.set_location_message(why.into()),
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
    // Issue #72: "seen 3 min ago" ages by itself: the cards are made again
    // every 30 s (only the ones that changed are redrawn).
    let rows_for_ages = rows.clone();
    let ages_timer = slint::Timer::default();
    // Issue #95: the home screen's clock, in the hub's time zone. Checked
    // every 2 s; the screen only redraws when the minute changes.
    let (clock_ui, clock_settings) = (ui.as_weak(), hub_settings.clone());
    let clock_timer = slint::Timer::default();
    clock_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(2), move || {
        let Some(ui) = clock_ui.upgrade() else { return };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let (time, date) = zones::clock(&clock_settings.borrow().time_zone, now);
        if ui.get_clock_time() != time.as_str() {
            ui.set_clock_time(time.into());
        }
        if ui.get_clock_date() != date.as_str() {
            ui.set_clock_date(date.into());
        }
    });
    ages_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(30), move || {
        if let Ok(mut rows) = rows_for_ages.try_borrow_mut() {
            if rows.devices.values().any(|d| d.last_seen.is_some()) {
                rows.refresh();
            }
        }
    });
    // Live video: a frame is shown the moment it comes (ws_client wakes
    // the event loop, which runs SHOW_FRAME), not at the next POLL.
    let shown: std::rc::Rc<std::cell::Cell<(u32, std::time::Instant)>> = std::rc::Rc::new(std::cell::Cell::new((0, std::time::Instant::now())));
    let (frame_ui, frame_watching, frame_shown) = (ui.as_weak(), watching.clone(), shown.clone());
    SHOW_FRAME.with(|show| {
        *show.borrow_mut() = Some(Box::new(move || {
            if let Some(ui) = frame_ui.upgrade() {
                show_video_frame(&ui, &frame_watching, &frame_shown);
            }
        }));
    });
    let _ = ws_client::FRAME_READY.set(Box::new(|| {
        // From the connection thread: run SHOW_FRAME on the GUI thread.
        let _ = slint::invoke_from_event_loop(|| {
            SHOW_FRAME.with(|show| {
                if let Some(show) = show.borrow().as_ref() {
                    show();
                }
            })
        });
    }));
    let (poll_watching, poll_shown) = (watching.clone(), shown.clone());
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
        // A camera's live frame that the wake-up (FRAME_READY) missed.
        show_video_frame(&ui, &poll_watching, &poll_shown);
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
                ws_client::Update::Disconnected => {
                    ui.set_connected(false);
                    wizard.borrow_mut().connection_lost(&ui);
                }
                ws_client::Update::Devices(list) => rows.borrow_mut().replace_all(list),
                ws_client::Update::DeviceChanged(device) => {
                    // The details page follows its device (e.g. "online").
                    if ui.get_page() == setup::PAGE_DEVICE && ui.get_dev_id() == device.id.as_str() {
                        show_device_page(&ui, &device, &wizard.borrow());
                    }
                    // So does the remote ("now playing").
                    if ui.get_page() == PAGE_REMOTE && ui.get_remote_id() == device.id.as_str() {
                        show_remote(&ui, &device);
                    }
                    // ...and an IR device's (its buttons, issue #42).
                    if ui.get_page() == PAGE_IR_REMOTE && ui.get_remote_id() == device.id.as_str() {
                        show_ir_remote(&ui, &device);
                    }
                    let on_page = ui.get_page() == setup::PAGE_DEVICE && ui.get_dev_id() == device.id.as_str();
                    rows.borrow_mut().changed(device);
                    if on_page {
                        let rows = rows.borrow();
                        if let Some(device) = rows.devices.get(ui.get_dev_id().as_str()) {
                            show_power_link(&ui, device, &rows.devices, &rows.templates);
                        }
                    }
                }
                ws_client::Update::DeviceRemoved(id) => rows.borrow_mut().removed(&id),
                // Issue #99: the plug link wasn't saved.
                ws_client::Update::PowerLink(Err(message)) => ui.set_dev_message(message.into()),
                ws_client::Update::PowerLink(Ok(())) => ui.set_dev_message("".into()),
                // A learning step finished.
                ws_client::Update::PowerLearned { step, result } => {
                    ui.set_dev_power_busy(false);
                    ui.set_dev_power_status(learned_text(&step, result).into());
                }
                ws_client::Update::CommandFailed(message) => {
                    ui.set_device_message_ok(false);
                    ui.set_device_message(message.into());
                    device_message_until = Some(std::time::Instant::now() + DEVICE_MESSAGE_TIME);
                }
                // A camera's live video didn't start: back to stills, and
                // why (like a refused command).
                ws_client::Update::VideoFailed(message) => {
                    ui.set_controls_live(false);
                    ui.set_device_message_ok(false);
                    ui.set_device_message(format!("No live video: {message}").into());
                    device_message_until = Some(std::time::Instant::now() + DEVICE_MESSAGE_TIME);
                }
                // Adding and setting up devices (issue #40, setup.rs).
                ws_client::Update::Templates(templates) => {
                    wizard.borrow_mut().templates = templates.clone();
                    wizard.borrow().show_lists(&ui);
                    rows.borrow_mut().set_templates(templates);
                }
                ws_client::Update::Found(found) => {
                    wizard.borrow_mut().found = found;
                    wizard.borrow().show_lists(&ui);
                    wizard.borrow_mut().found_changed(&ui, &request_tx);
                }
                ws_client::Update::WizardStep(step) => wizard.borrow_mut().show_step(&ui, &request_tx, &step),
                ws_client::Update::WizardError { session, field, message, detail } => {
                    wizard.borrow_mut().show_error(&ui, &request_tx, &session, field.as_deref(), &message, &detail)
                }
                ws_client::Update::WizardDone(device) => {
                    wizard.borrow_mut().finished(&ui, &device.name);
                    device_message_until = Some(std::time::Instant::now() + DEVICE_MESSAGE_TIME);
                }
                ws_client::Update::Removed(Ok(())) => {
                    ui.set_device_back(0);
                    ui.set_page(setup::PAGE_DEVICES);
                    ui.set_device_message_ok(true);
                    ui.set_device_message(format!("{} removed", ui.get_dev_name()).into());
                    device_message_until = Some(std::time::Instant::now() + DEVICE_MESSAGE_TIME);
                }
                ws_client::Update::Removed(Err(message)) => ui.set_dev_message(message.into()),
                // Scenes (issue #47).
                ws_client::Update::Scenes(list, autos) => {
                    show_scenes(&ui, &scenes, list);
                    show_automations(&ui, &automations, autos, &rows.borrow().devices, &scenes.borrow());
                }
                ws_client::Update::SceneSaved(list, autos) => {
                    show_scenes(&ui, &scenes, list);
                    show_automations(&ui, &automations, autos, &rows.borrow().devices, &scenes.borrow());
                    ui.set_capture_busy(false);
                    ui.set_scenes_message_ok(true);
                    ui.set_scenes_message(format!("Saved \u{201C}{}\u{201D}. Tap Run to try it.", ui.get_scene_name().trim()).into());
                    ui.set_page(PAGE_SCENES);
                }
                ws_client::Update::AutomationSaved(list, autos) => {
                    show_scenes(&ui, &scenes, list);
                    show_automations(&ui, &automations, autos, &rows.borrow().devices, &scenes.borrow());
                    ui.set_automation_busy(false);
                    ui.set_automations_message_ok(true);
                    ui.set_automations_message(format!("Saved \u{201C}{}\u{201D}.", ui.get_automation_name().trim()).into());
                    ui.set_page(PAGE_AUTOMATIONS);
                }
                ws_client::Update::AutomationsFailed(message) => {
                    ui.set_automation_busy(false);
                    if ui.get_page() == PAGE_NEW_AUTOMATION {
                        ui.set_automation_message(message.into());
                    } else {
                        ui.set_automations_message_ok(false);
                        ui.set_automations_message(message.into());
                    }
                }
                ws_client::Update::AutomationRanNow { id, result } => {
                    ui.set_automation_running("".into());
                    let name = automations
                        .borrow()
                        .iter()
                        .find(|a| a["id"] == id.as_str())
                        .and_then(|a| a["name"].as_str().map(str::to_string))
                        .unwrap_or(id);
                    ui.set_automations_message_ok(result.is_ok());
                    ui.set_automations_message(match result {
                        Ok(()) => format!("{name}: done"),
                        Err(why) => format!("{name}: {why}"),
                    }
                    .into());
                }
                ws_client::Update::Log(entries) => {
                    *log.borrow_mut() = entries;
                    show_log(&ui, &log.borrow(), &settings.borrow().time_zone);
                }
                ws_client::Update::LogEntry(entry) => {
                    let mut log = log.borrow_mut();
                    // The hub keeps 200; so does this screen.
                    if log.len() >= 200 {
                        log.remove(0);
                    }
                    log.push(entry);
                    show_log(&ui, &log, &settings.borrow().time_zone);
                }
                ws_client::Update::ScenesFailed(message) => {
                    ui.set_capture_busy(false);
                    if ui.get_page() == PAGE_CAPTURE {
                        ui.set_capture_message(message.into());
                    } else {
                        ui.set_scenes_message_ok(false);
                        ui.set_scenes_message(message.into());
                    }
                }
                ws_client::Update::SceneRan { id, result } => {
                    ui.set_scene_running("".into());
                    let name = scenes.borrow().iter().find(|s| s.id == id).map_or(id.clone(), |s| s.name.clone());
                    let (ok, message) = match result {
                        Ok(()) => (true, format!("{name}: done")),
                        Err(why) => (false, format!("{name}: {why}")),
                    };
                    // Said where it was asked from: the Scenes page, or
                    // under the device list (the quick buttons).
                    if ui.get_page() == PAGE_SCENES {
                        ui.set_scenes_message_ok(ok);
                        ui.set_scenes_message(message.into());
                    } else {
                        ui.set_device_message_ok(ok);
                        ui.set_device_message(message.into());
                        device_message_until = Some(std::time::Instant::now() + DEVICE_MESSAGE_TIME);
                    }
                }
                // Issue #43: a camera's picture came (or couldn't).
                ws_client::Update::DeviceAction { name, result, .. } if name == "snapshot" => match {
                    picture_waiting.set(None);
                    result
                } {
                    Ok(answer) => match answer["jpeg"].as_str().and_then(vacuum_map::picture) {
                        Some(image) => {
                            ui.set_controls_map(image);
                            ui.set_controls_map_state("".into());
                        }
                        None => ui.set_controls_map_state("The picture couldn't be read.".into()),
                    },
                    Err(message) => ui.set_controls_map_state(message.into()),
                },
                // Issue #74: a vacuum's map came (or couldn't).
                ws_client::Update::DeviceAction { name, result, .. } if name == "map" => match result {
                    Ok(map) => {
                        let theme = ui.global::<Theme>();
                        let colors = vacuum_map::Colors {
                            floor: theme.get_surface_pressed(),
                            wall: theme.get_text_muted(),
                            path: theme.get_text(),
                            robot: theme.get_accent(),
                            dock: theme.get_ok(),
                            no_go: theme.get_error(),
                        };
                        match vacuum_map::render(&map, &colors) {
                            Some(image) => {
                                ui.set_controls_map(image);
                                ui.set_controls_map_state("".into());
                            }
                            None => ui.set_controls_map_state("The map couldn't be read.".into()),
                        }
                    }
                    Err(message) => ui.set_controls_map_state(message.into()),
                },
                // The code finder's actions (issue #82): its next step.
                ws_client::Update::DeviceAction { name, args, result }
                    if ui.get_page() == PAGE_IR_REMOTE && matches!(name.as_str(), "library" | "finder" | "try" | "use_set") =>
                {
                    if let Some((name, args)) = finder.borrow_mut().result(&ui, &ir_items, &name, &args, &result) {
                        finder_act("remote", name, args);
                    }
                }
                // An IR device's action (issue #42): what happened, in one
                // line. The buttons themselves follow as DeviceChanged.
                ws_client::Update::DeviceAction { name, args, result } if ui.get_page() == PAGE_IR_REMOTE => {
                    let button = text_of(&args["button"]);
                    let (ok, message) = match (&result, name.as_str()) {
                        (Ok(_), "learn") => (true, format!("Learned \u{201C}{button}\u{201D}. Tap it to try it.")),
                        (Ok(_), "forget") => (true, format!("Deleted \u{201C}{button}\u{201D}.")),
                        (Ok(_), "rename") => (true, format!("Renamed to \u{201C}{}\u{201D}.", text_of(&args["to"]))),
                        (Ok(found), "light") => (true, light_summary(args["enabled"] == true, found)),
                        (Ok(_), _) => (true, String::new()),
                        (Err(why), _) => (false, why.clone()),
                    };
                    // A learn's answer ends the waiting view, either way.
                    if name == "learn" && ui.get_ir_view() == 3 {
                        ui.set_ir_view(0);
                    }
                    ui.set_ir_message_ok(ok);
                    ui.set_ir_message(message.into());
                }
                // A cover's open / close / stop (issue #77), or an IR
                // light's brightness step from its card (#85), that
                // failed: said under the device list, like a failed command.
                ws_client::Update::DeviceAction { name, result: Err(message), .. }
                    if matches!(name.as_str(), "open" | "close" | "stop" | "start" | "pause" | "dock" | "locate"
                        | "set_fan" | "set_water" | "set_mop" | "clean_rooms" | "reset_part" | "set")
                        || (name == "press" && matches!(ui.get_page(), setup::PAGE_DEVICES | PAGE_CONTROLS)) =>
                {
                    ui.set_device_message_ok(false);
                    ui.set_device_message(message.into());
                    device_message_until = Some(std::time::Instant::now() + DEVICE_MESSAGE_TIME);
                }
                // A remote action's answer (issue #44): a list to show, or
                // why it failed.
                ws_client::Update::DeviceAction { name, args, result } => match result {
                    Ok(result) => {
                        ui.set_remote_message("".into());
                        match name.as_str() {
                            "apps" => {
                                ui.set_remote_loading(false);
                                remote_items.set_vec(list_items(&result["apps"], |a| (text_of(&a["label"]), String::new())));
                            }
                            // A page of a search: only if it's still the
                            // search on screen (typing fast outruns them).
                            "channels" if text_of(&args["query"]) == ui.get_remote_query().as_str() => {
                                ui.set_remote_loading(false);
                                let page = list_items(&result["channels"], |c| (text_of(&c["number"]), text_of(&c["name"])));
                                if args["offset"].as_u64().unwrap_or(0) == 0 {
                                    remote_items.set_vec(page);
                                } else {
                                    for item in page {
                                        remote_items.push(item);
                                    }
                                }
                                ui.set_remote_total(result["total"].as_i64().unwrap_or(0) as i32);
                            }
                            _ => {}
                        }
                    }
                    Err(message) => {
                        ui.set_remote_loading(false);
                        ui.set_remote_message(message.into());
                    }
                },
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
                ws_client::Update::Accounts(accounts) => ui.set_accounts(account_rows(&accounts)),
                // Issue #39: a new look (or time zone), from this screen or
                // a phone -- applied at once, no restart.
                ws_client::Update::Settings(new) => {
                    show_settings(&ui, &new, &mut shown_dark);
                    // Issue #47: the location (sunrise/sunset).
                    let location = match (new.latitude, new.longitude) {
                        (Some(lat), Some(lon)) => format!("{lat:.2}, {lon:.2}"),
                        _ => String::new(),
                    };
                    ui.set_has_location(!location.is_empty());
                    ui.set_location_current(location.into());
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
/// backend_daemon last reported them, and what app.slint draws of them.
/// Since issue #95 that's tiles (tiles.rs builds them): the ones the
/// selected filter chip picks, under room headings, split into rows of
/// `tile-columns` tiles; plus the chips, and the open controls page's
/// full card.
///
/// The grid is one model of rows for the program's whole life. On a
/// change only the tiles that differ are updated (set_row_data); the rows
/// are only rebuilt when tiles appear, disappear or move (or the filter or
/// the column count changes) -- so the list doesn't jump back to the top
/// every time a lamp is switched.
struct DeviceRows {
    devices: std::collections::BTreeMap<String, ws_client::Device>,
    /// The device types (issue #40): "Pair again", and a tile's icon.
    templates: Vec<ws_client::Template>,
    /// What app.slint shows: one entry per row (a heading or tiles).
    grid: std::rc::Rc<slint::VecModel<TileRow>>,
    /// The tile rows' own models, by grid row (None for a heading).
    rows: Vec<Option<std::rc::Rc<slint::VecModel<TileItem>>>>,
    /// The grid's shape: each row's heading or tile ids.
    shape: Vec<(String, Vec<String>)>,
    /// The filter chips as last shown, and their model on the screen.
    chips: Vec<FilterChip>,
    chip_model: std::rc::Rc<slint::VecModel<FilterChip>>,
    /// Favourites start selected the first time there are any.
    filter_chosen: bool,
    /// To read the filter, the column count, the theme (weak: see the
    /// callbacks in main).
    ui: slint::Weak<AppWindow>,
}

impl DeviceRows {
    fn new(ui: &AppWindow) -> Self {
        let grid = std::rc::Rc::new(slint::VecModel::default());
        ui.set_tile_rows(grid.clone().into());
        let chip_model = std::rc::Rc::new(slint::VecModel::default());
        ui.set_filter_chips(chip_model.clone().into());
        DeviceRows {
            devices: Default::default(),
            templates: Vec::new(),
            grid,
            rows: Vec::new(),
            shape: Vec::new(),
            chips: Vec::new(),
            chip_model,
            filter_chosen: false,
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

    fn set_templates(&mut self, templates: Vec<ws_client::Template>) {
        self.templates = templates;
        self.refresh();
    }

    /// Brings the chips, the grid and the controls page in line with
    /// `devices`.
    fn refresh(&mut self) {
        use slint::Model;
        let Some(ui) = self.ui.upgrade() else { return };
        let all: Vec<&ws_client::Device> = self.devices.values().collect();

        // The chips; a filter whose chip is gone (no lights on any more,
        // a room emptied) falls back to All.
        let chips = tiles::chips(&all, &self.templates);
        if !self.filter_chosen && !all.is_empty() {
            self.filter_chosen = true;
            if all.iter().any(|d| d.favourite) {
                ui.set_device_filter(tiles::FAVOURITES.into());
            }
        }
        let filter = ui.get_device_filter().to_string();
        let filter = if chips.iter().any(|c| c.id == filter.as_str()) { filter } else { tiles::ALL.to_string() };
        // Only real changes reach the screen: every change redraws.
        if ui.get_device_filter() != filter.as_str() {
            ui.set_device_filter(filter.as_str().into());
        }
        let same_chips = chips.len() == self.chips.len() && chips.iter().zip(&self.chips).all(|(a, b)| a.id == b.id);
        if same_chips {
            // The same chips, maybe new numbers ("18 W" -> "19 W"): just
            // those labels.
            for (i, chip) in chips.iter().enumerate() {
                if *chip != self.chips[i] {
                    self.chip_model.set_row_data(i, chip.clone());
                }
            }
        } else {
            self.chip_model.set_vec(chips.clone());
        }
        self.chips = chips;

        // The grid: each group's heading, then its tiles in rows.
        let columns = ui.get_tile_columns().max(1) as usize;
        let accent = ui.global::<Theme>().get_accent();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut rows: Vec<(String, Vec<TileItem>)> = Vec::new();
        for (heading, group) in tiles::groups(&all, &filter, &self.templates, accent, now) {
            if !heading.is_empty() {
                rows.push((heading, Vec::new()));
            }
            for chunk in group.chunks(columns) {
                rows.push((String::new(), chunk.to_vec()));
            }
        }
        let shape: Vec<(String, Vec<String>)> =
            rows.iter().map(|(h, t)| (h.clone(), t.iter().map(|t| t.id.to_string()).collect())).collect();
        if shape == self.shape {
            // Same tiles in the same places: update the changed ones.
            for ((_, tiles), model) in rows.into_iter().zip(&self.rows) {
                if let Some(model) = model {
                    for (i, tile) in tiles.into_iter().enumerate() {
                        if model.row_data(i).as_ref() != Some(&tile) {
                            model.set_row_data(i, tile);
                        }
                    }
                }
            }
        } else {
            self.rows = rows
                .iter()
                .map(|(h, t)| h.is_empty().then(|| std::rc::Rc::new(slint::VecModel::from(t.clone()))))
                .collect();
            let grid: Vec<TileRow> = rows
                .iter()
                .zip(&self.rows)
                .map(|((h, _), model)| TileRow {
                    header: h.as_str().into(),
                    tiles: model.clone().map_or_else(Default::default, slint::ModelRc::from),
                })
                .collect();
            self.grid.set_vec(grid);
            self.shape = shape;
        }

        // The controls page follows its device.
        if ui.get_page() == PAGE_CONTROLS {
            self.show_controls(&ui, ui.get_controls_item().id.as_str());
        }
    }

    /// Fills the controls page (issue #95) for this device; false if it's
    /// gone.
    fn show_controls(&self, ui: &AppWindow, id: &str) -> bool {
        match self.devices.get(id) {
            Some(device) => {
                let accent = ui.global::<Theme>().get_accent();
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                ui.set_controls_tile(tiles::tile(device, &self.templates, accent, now));
                ui.set_controls_item(device_item(device, &self.templates));
                ui.set_controls_favourite(device.favourite);
                true
            }
            None => {
                // Removed meanwhile (from the app, the cloud).
                ui.set_device_back(0);
                ui.set_page(setup::PAGE_DEVICES);
                false
            }
        }
    }
}

/// Issue #95: one device's controls (app.slint's page 20).
const PAGE_CONTROLS: i32 = 20;

/// Automations (issue #47): the list, the log, the editor, the location.
const PAGE_AUTOMATIONS: i32 = 16;
const PAGE_LOG: i32 = 17;
const PAGE_NEW_AUTOMATION: i32 = 18;
const PAGE_LOCATION: i32 = 19;

/// The automations page's list: each with its one-line summary (device
/// and scene names, not ids).
fn show_automations(
    ui: &AppWindow,
    store: &std::cell::RefCell<Vec<serde_json::Value>>,
    list: Vec<serde_json::Value>,
    devices: &std::collections::BTreeMap<String, ws_client::Device>,
    scenes: &[ws_client::Scene],
) {
    let device_name = |id: &str| devices.get(id).map_or(id.to_string(), |d| d.name.clone());
    let scene_name = |id: &str| scenes.iter().find(|s| s.id == id).map_or(id.to_string(), |s| s.name.clone());
    let items: Vec<AutomationItem> = list
        .iter()
        .map(|a| AutomationItem {
            id: a["id"].as_str().unwrap_or_default().into(),
            name: a["name"].as_str().unwrap_or_default().into(),
            summary: automation_text::summary(a, &device_name, &scene_name).into(),
            enabled: a["enabled"].as_bool().unwrap_or(true),
        })
        .collect();
    ui.set_automations(std::rc::Rc::new(slint::VecModel::from(items)).into());
    *store.borrow_mut() = list;
}

/// The log page: newest first, times in the hub's time zone.
fn show_log(ui: &AppWindow, log: &[ws_client::LogEntry], time_zone: &str) {
    let items: Vec<LogItem> = log
        .iter()
        .rev()
        .map(|e| LogItem {
            time: zones::local_moment(time_zone, e.at).into(),
            name: e.name.clone().into(),
            cause: e.cause.clone().into(),
            ok: e.ok,
            detail: e.detail.clone().into(),
        })
        .collect();
    ui.set_log_entries(std::rc::Rc::new(slint::VecModel::from(items)).into());
}

/// The scenes (issue #47): the list, and "Save current state".
const PAGE_SCENES: i32 = 14;
const PAGE_CAPTURE: i32 = 15;

/// The scenes: on the Scenes page, and as the home screen's scene chips
/// (all of them since #95; the row scrolls sideways).
fn show_scenes(ui: &AppWindow, store: &std::cell::RefCell<Vec<ws_client::Scene>>, list: Vec<ws_client::Scene>) {
    let item = |s: &ws_client::Scene| {
        let devices: std::collections::BTreeSet<&str> = s.steps.iter().filter_map(|step| step["device"].as_str()).collect();
        let detail = match devices.len() {
            0 => format!("{} steps", s.steps.len()),
            1 => "1 device".to_string(),
            n => format!("{n} devices"),
        };
        SceneItem { id: s.id.clone().into(), name: s.name.clone().into(), detail: detail.into() }
    };
    let all: Vec<SceneItem> = list.iter().map(item).collect();
    ui.set_scenes(std::rc::Rc::new(slint::VecModel::from(all)).into());
    *store.borrow_mut() = list;
}

/// Can "Save current state" capture something of this device? (The same
/// rule as backend_daemon's automations::capture_steps: never a lock.)
fn capturable(device: &ws_client::Device) -> bool {
    let c = &device.capabilities;
    c.switch.is_some()
        || c.dimmer.is_some()
        || c.color.is_some()
        || c.climate.is_some()
        || c.cover.as_ref().is_some_and(|cover| cover.can_position)
        || c.media.as_ref().is_some_and(|m| !m.input.is_empty())
}

/// What "Save current state" would save, in a few words: "On, 30 %,
/// 2700 K", "Off", "Cool 22.5 °C", "40 % open".
fn state_text(device: &ws_client::Device) -> String {
    let c = &device.capabilities;
    if c.switch.as_ref().is_some_and(|s| !s.on) {
        return "Off".into();
    }
    let mut parts = Vec::new();
    if c.switch.is_some() {
        parts.push("On".to_string());
    }
    if let Some(d) = &c.dimmer {
        parts.push(format!("{} %", d.level));
    }
    if let Some(color) = &c.color {
        parts.push(color_of(color).1);
    }
    if let Some(position) = c.cover.as_ref().and_then(|cover| cover.position) {
        parts.push(format!("{position} % open"));
    }
    if let Some(climate) = &c.climate {
        parts.push(format!("{} {}", climate.mode, celsius(climate.target)));
    }
    if let Some(media) = c.media.as_ref().filter(|m| !m.input.is_empty()) {
        parts.push(format!("volume {}", media.volume));
    }
    parts.join(", ")
}

/// The remote page (issue #44). Not a setup page: its number lives here.
const PAGE_REMOTE: i32 = 12;

/// An IR device's remote (issue #42).
const PAGE_IR_REMOTE: i32 = 13;

/// How long the hub waits for a button on the original remote when
/// teaching (ir_remote.slint says it: "up to 30 seconds").
const IR_LEARN_SECONDS: u64 = 30;

/// Channels per page of the remote's list (the hub caps it at 500).
const CHANNEL_PAGE: u32 = 100;

/// Fills the remote page for a device.
fn show_remote(ui: &AppWindow, device: &ws_client::Device) {
    ui.set_remote_id(device.id.clone().into());
    ui.set_remote_name(device.name.clone().into());
    ui.set_remote_playing(now_playing(device).into());
    ui.set_remote_keyboard(device.capabilities.remote.as_ref().is_some_and(|r| r.keyboard));
}

/// Fills an IR device's remote page (issue #42).
fn show_ir_remote(ui: &AppWindow, device: &ws_client::Device) {
    ui.set_remote_id(device.id.clone().into());
    ui.set_remote_name(device.name.clone().into());
    let buttons: Vec<slint::SharedString> = device
        .capabilities
        .remote
        .as_ref()
        .map(|r| r.buttons.iter().map(|b| b.into()).collect())
        .unwrap_or_default();
    // As a remote's rows where the names are the hub's standard ones
    // (issue #82: a TV found in the code library).
    let names: Vec<String> = buttons.iter().map(|b| b.to_string()).collect();
    let (rows, others) = ir_layout::layout(&names);
    ui.set_ir_layout(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    let others: Vec<slint::SharedString> = others.into_iter().map(Into::into).collect();
let labelled: Vec<IrKeyItem> =
        names.iter().map(|n| IrKeyItem { name: n.as_str().into(), label: ir_layout::label(n).into() }).collect();
    ui.set_ir_labelled(std::rc::Rc::new(slint::VecModel::from(labelled)).into());
        ui.set_ir_others(std::rc::Rc::new(slint::VecModel::from(others)).into());
    ui.set_ir_buttons(std::rc::Rc::new(slint::VecModel::from(buttons)).into());
    ui.set_ir_light(device.config.get("light").is_some_and(|l| l == "on"));
    ui.set_ir_order(device.config.get("colour_order").cloned().unwrap_or_default().into());
    ui.set_ir_status(
        match device.online.as_deref() {
            Some("offline") => "The IR blaster is offline: buttons can't be sent or taught right now. Is it plugged in and on the WiFi?",
            Some("unauthorized") => "The IR blaster needs pairing again: open the device's details and choose Pair again.",
            _ => "",
        }
        .into(),
    );
}

/// Issue #85: what "Use as a light" found among the buttons, in one line.
fn light_summary(enabled: bool, found: &serde_json::Value) -> String {
    if !enabled {
        return "Not used as a light: its card shows only the remote.".into();
    }
    let mut parts = Vec::new();
    if found["switch"] == true {
        parts.push("on/off".to_string());
    }
    match found["colors"].as_u64().unwrap_or(0) {
        0 => {}
        1 => parts.push("1 colour".into()),
        n => parts.push(format!("{n} colours")),
    }
    if found["brightness"] == true {
        parts.push("brightness \u{2212} / +".into());
    }
    if parts.is_empty() {
        "No light buttons found yet: teach or name them On, Off or Power, Red, Green, Blue, Brighter, Dimmer.".into()
    } else {
        format!("On its card: {}.", parts.join(", "))
    }
}

/// What's on a TV's screen, in words: "Live TV \u{2022} 5 Pro TV",
/// "Netflix", "" (not known, or off).
fn now_playing(device: &ws_client::Device) -> String {
    let Some(media) = &device.capabilities.media else { return String::new() };
    let app = media.app.as_ref().map(|a| a.label.clone()).unwrap_or_default();
    match &media.channel {
        Some(c) => format!("{app} \u{2022} {} {}", c.number, c.name).trim().to_string(),
        None => app,
    }
}

/// A JSON list -> rows of the remote's list; `parts` gives (label, detail).
fn list_items(list: &serde_json::Value, parts: impl Fn(&serde_json::Value) -> (String, String)) -> Vec<RemoteItem> {
    list.as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let (label, detail) = parts(item);
                    RemoteItem { id: text_of(&item["id"]).into(), label: label.into(), detail: detail.into() }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn text_of(value: &serde_json::Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// A device's reachability (issue #40) as the card and details page say
/// it: "" when all is well (or not known).
/// "just now", "3 min ago", "2 h ago", "3 days ago" (a battery device's
/// last report, issue #72).
fn seen_ago(at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    tiles::seen_ago(at, now)
}

fn status_text(device: &ws_client::Device) -> &'static str {
    match device.online.as_deref() {
        Some("offline") => "Offline",
        Some("unauthorized") => "Needs pairing again",
        _ => "",
    }
}

/// Fills the device details page (issue #40).
/// Issue #99: the device page's "Powered by" (an IR device and a plug,
/// backend power_link.rs): the plugs it can use, the link's settings,
/// and what was learned.
fn show_power_link(
    ui: &AppWindow,
    device: &ws_client::Device,
    devices: &std::collections::BTreeMap<String, ws_client::Device>,
    templates: &[ws_client::Template],
) {
    let config = &device.config;
    ui.set_dev_can_power_link(device.source == "ir-blaster");
    let plug_id = config.get("powered_by").cloned().unwrap_or_default();
    let plug = devices.get(&plug_id);
    ui.set_dev_power_plug(plug_id.clone().into());
    ui.set_dev_power_cut(config.get("cut_power").map(String::as_str) == Some("on"));
    ui.set_dev_power_delay(config.get("start_delay_s").and_then(|d| d.parse().ok()).unwrap_or(2));
    ui.set_dev_power_measures(plug.is_some_and(|p| p.capabilities.energy.is_some()));
    let learned = match (config.get("power_off_w"), config.get("power_on_w"), config.get("power_threshold_w")) {
        (Some(off), Some(on), Some(_)) => format!("Off {off} W \u{2022} on {on} W: its state follows the plug's measurement."),
        _ => String::new(),
    };
    ui.set_dev_power_learned(learned.into());
    ui.set_dev_power_summary(match plug {
        None => "Not plugged into a plug the hub knows".to_string(),
        Some(p) => format!("{} \u{2022} {}", p.name, power_text(device).trim_start_matches("Power: ")),
    }.into());
    // The plugs: anything switchable that isn't an IR device or itself
    // linked; the ones that measure say so.
    let mut plugs: Vec<&ws_client::Device> = devices
        .values()
        .filter(|d| d.id != device.id && d.capabilities.switch.is_some() && d.source != "ir-blaster" && !d.config.contains_key("powered_by"))
        .filter(|d| tiles::is_plug(d, templates))
        .collect();
    plugs.sort_by(|a, b| a.name.cmp(&b.name));
    let items: Vec<ModeItem> = plugs
        .iter()
        .map(|p| ModeItem {
            value: p.id.clone().into(),
            label: if p.capabilities.energy.is_some() { format!("{} \u{2022} measures power", p.name) } else { p.name.clone() }.into(),
        })
        .collect();
    ui.set_dev_plugs(std::rc::Rc::new(slint::VecModel::from(items)).into());
}

/// Issue #99: the card's line for a device powered by a plug ("" = not).
fn power_text(device: &ws_client::Device) -> String {
    let config = &device.config;
    if !config.contains_key("powered_by") {
        return String::new();
    }
    if let Some(problem) = config.get("power_problem") {
        return format!("Power: {problem}");
    }
    if config.contains_key("power_threshold_w") { "Power: confirmed by the plug" } else { "Power: assumed" }.to_string()
}

/// Issue #99: what a learning step found, in words.
fn learned_text(step: &str, result: Result<serde_json::Value, String>) -> String {
    match (step, result) {
        (_, Err(why)) => format!("Not learned: {why}"),
        ("off", Ok(r)) => format!("Off: {} W. Now step 2.", r["watts"]),
        ("on", Ok(r)) => format!("On: {} W. Learned: from {} W it counts as on.", r["watts"], r["threshold"]),
        _ => String::new(),
    }
}

fn show_device_page(ui: &AppWindow, device: &ws_client::Device, wizard: &setup::Setup) {
    let template = wizard.template(&device.template);
    ui.set_dev_id(device.id.clone().into());
    ui.set_dev_name(device.name.clone().into());
    ui.set_dev_room(device.room.clone().into());
    // Issue #74: a cloud-only device says so where its type is shown.
    ui.set_dev_type(
        template
            .map_or_else(
                || if device.source == "virtual" { "Test device".into() } else { device.template.clone() },
                |t| if t.cloud { format!("{} \u{2022} needs internet", t.name) } else { t.name.clone() },
            )
            .into(),
    );
    ui.set_dev_status(match device.online.as_deref() {
        Some("online") => "Online",
        _ => status_text(device),
    }.into());
    let remote = device.capabilities.remote.as_ref();
    ui.set_dev_has_remote(remote.is_some());
    ui.set_dev_remote_learns(remote.is_some_and(|r| r.learn));
    ui.set_dev_can_reauth(template.is_some_and(|t| t.can_reauth));
    ui.set_dev_can_reconfigure(template.is_some_and(|t| t.can_reconfigure));
    // Built into the hub (the board's LED): hardware whose type the wizard
    // doesn't offer. backend_daemon would refuse; don't offer it.
    ui.set_dev_can_remove(device.source == "virtual" || template.is_some());
    if ui.get_dev_id() != device.id.as_str() || ui.get_page() != setup::PAGE_DEVICE {
        ui.set_dev_message("".into());
    }
}

/// One device -> one row of the list: which capabilities it has, and
/// their values in the form app.slint shows them.
fn device_item(device: &ws_client::Device, templates: &[ws_client::Template]) -> DeviceItem {
    let caps = &device.capabilities;
    let (color, color_text) = match &caps.color {
        Some(c) => color_of(c),
        None => (slint::Color::default(), String::new()),
    };
    let sensor_text = caps.sensor.as_ref().map_or(String::new(), |s| {
        s.readings
            .iter()
            .map(|(name, r)| match name.as_str() {
                /* A camera's ONVIF motion (#43): yes / no, not a number. */
                "motion" => if r.value >= 1.0 { "Motion now" } else { "No motion" }.to_string(),
                _ => format!("{name} {} {}", r.value, r.unit).trim_end().to_string(),
            })
            .collect::<Vec<_>>()
            .join("   ")
    });
    DeviceItem {
        id: device.id.clone().into(),
        name: device.name.clone().into(),
        // Issue #72: a battery device says when it last reported.
        room: match device.last_seen {
            Some(at) if device.room.is_empty() => format!("seen {}", seen_ago(at)),
            Some(at) => format!("{} \u{2022} seen {}", device.room, seen_ago(at)),
            None => device.room.clone(),
        }
        .into(),
        has_switch: caps.switch.is_some(),
        on: caps.switch.as_ref().is_some_and(|s| s.on),
        has_dimmer: caps.dimmer.is_some(),
        level: caps.dimmer.as_ref().map_or(0, |d| d.level.into()),
        has_color: caps.color.is_some(),
        color,
        color_text: color_text.into(),
        sensor_text: sensor_text.into(),
        status: status_text(device).into(),
        can_reauth: device.online.as_deref() == Some("unauthorized")
            && templates.iter().any(|t| t.id == device.template && t.can_reauth),
        has_media: caps.media.is_some(),
        volume: caps.media.as_ref().map_or(0, |m| m.volume.into()),
        muted: caps.media.as_ref().is_some_and(|m| m.muted),
        input: caps.media.as_ref().map_or(String::new(), |m| m.input.clone()).into(),
        has_remote: caps.remote.is_some(),
        remote_learns: caps.remote.as_ref().is_some_and(|r| r.learn),
        palette: std::rc::Rc::new(slint::VecModel::from(
            caps.color.as_ref().map_or(vec![], |c| {
                c.palette
                    .iter()
                    .map(|hex| {
                        let (color, hex) = color_of(&ws_client::Color { hex: Some(hex.clone()), kelvin: None, palette: vec![] });
                        ColorChip { hex: hex.into(), color }
                    })
                    .collect()
            }),
        ))
        .into(),
        brightness_up: caps.remote.as_ref().and_then(|r| r.brightness.as_ref()).map_or(String::new(), |b| b.up.clone()).into(),
        brightness_down: caps.remote.as_ref().and_then(|r| r.brightness.as_ref()).map_or(String::new(), |b| b.down.clone()).into(),
        now_playing: now_playing(device).into(),
        has_cover: caps.cover.is_some(),
        cover_position: caps.cover.as_ref().and_then(|c| c.position).map_or(-1, i32::from),
        cover_can_position: caps.cover.as_ref().is_some_and(|c| c.can_position),
        cover_moving: match caps.cover.as_ref().map(|c| c.moving.as_str()) {
            Some("opening") => "Opening",
            Some("closing") => "Closing",
            _ => "",
        }
        .into(),
        has_climate: caps.climate.is_some(),
        climate_mode: caps.climate.as_ref().map_or(String::new(), |c| c.mode.clone()).into(),
        climate_modes: string_model(caps.climate.as_ref().map_or(&[][..], |c| &c.modes)),
        climate_target: caps.climate.as_ref().map_or(0.0, |c| c.target as f32),
        climate_target_text: caps.climate.as_ref().map_or(String::new(), |c| celsius(c.target)).into(),
        climate_min: caps.climate.as_ref().map_or(0.0, |c| c.min as f32),
        climate_max: caps.climate.as_ref().map_or(0.0, |c| c.max as f32),
        climate_step: caps.climate.as_ref().map_or(1.0, |c| c.step as f32),
        climate_current_text: caps.climate.as_ref().and_then(|c| c.current).map_or(String::new(), celsius).into(),
        climate_fan: caps.climate.as_ref().and_then(|c| c.fan.clone()).unwrap_or_default().into(),
        climate_fans: string_model(caps.climate.as_ref().map_or(&[][..], |c| &c.fans)),
        has_lock: caps.lock.is_some(),
        lock_state: caps.lock.as_ref().map_or(String::new(), |l| l.state.clone()).into(),
        energy_text: caps.energy.as_ref().map_or(String::new(), energy_text).into(),
        power_text: power_text(device).into(),
        power_problem: device.config.contains_key("power_problem"),
        has_vacuum: caps.vacuum.is_some(),
        vacuum_state: caps.vacuum.as_ref().map_or(String::new(), |v| v.state.clone()).into(),
        vacuum_text: caps.vacuum.as_ref().map_or(String::new(), tiles::vacuum_line).into(),
        vacuum_fan: caps.vacuum.as_ref().and_then(|v| v.fan.clone()).unwrap_or_default().into(),
        vacuum_fans: modes(caps.vacuum.as_ref().map_or(&[][..], |v| &v.fans)),
        vacuum_water: caps.vacuum.as_ref().and_then(|v| v.water.clone()).unwrap_or_default().into(),
        vacuum_waters: modes(caps.vacuum.as_ref().map_or(&[][..], |v| &v.waters)),
        vacuum_mop: caps.vacuum.as_ref().and_then(|v| v.mop.clone()).unwrap_or_default().into(),
        vacuum_mops: modes(caps.vacuum.as_ref().map_or(&[][..], |v| &v.mops)),
        vacuum_run_text: caps.vacuum.as_ref().map_or(String::new(), tiles::vacuum_run).into(),
        vacuum_rooms: std::rc::Rc::new(slint::VecModel::from(caps.vacuum.as_ref().map_or(vec![], |v| {
            v.rooms.iter().map(|r| VacuumRoomItem { id: r.id as i32, name: r.name.clone().into() }).collect()
        })))
        .into(),
        vacuum_parts: std::rc::Rc::new(slint::VecModel::from(caps.vacuum.as_ref().map_or(vec![], |v| {
            v.parts.iter().map(|p| VacuumPartItem { id: p.id.clone().into(), name: p.name.clone().into(), left: i32::from(p.left) }).collect()
        })))
        .into(),
        vacuum_totals_text: caps.vacuum.as_ref().and_then(|v| v.totals.as_ref()).map_or(String::new(), |t| {
            format!("{} cleanings \u{2022} {:.0} m² \u{2022} {:.0} h in all", t.cleanings, t.area_m2, t.hours)
        }).into(),
        vacuum_quiet_text: caps.vacuum.as_ref().and_then(|v| v.quiet_hours.clone()).map_or(String::new(), |q| format!("Do not disturb {q}")).into(),
        vacuum_has_map: caps.vacuum.as_ref().is_some_and(|v| v.map),
        has_camera: caps.camera.as_ref().is_some_and(|c| c.snapshot),
        camera_ptz: caps.camera.as_ref().is_some_and(|c| c.ptz),
        camera_presets: std::rc::Rc::new(slint::VecModel::from(caps.camera.as_ref().map_or(vec![], |c| {
            c.presets.iter().map(|p| ModeItem { value: p.token.clone().into(), label: p.name.clone().into() }).collect()
        })))
        .into(),
        options: std::rc::Rc::new(slint::VecModel::from(caps.options.as_ref().map_or(vec![], |o| {
            o.options
                .iter()
                .map(|o| OptionItem {
                    id: o.id.clone().into(),
                    name: o.name.clone().into(),
                    on: o.on,
                    value: o.value.clone().unwrap_or_default().into(),
                    choices: modes(&o.choices),
                })
                .collect()
        })))
        .into(),
        inputs: std::rc::Rc::new(slint::VecModel::from(
            caps.media
                .as_ref()
                .map_or(vec![], |m| m.inputs.iter().map(|i| MediaInputItem { id: i.id.clone().into(), label: i.label.clone().into() }).collect()),
        ))
        .into(),
    }
}

/// Asks backend_daemon for a camera's picture (the answer: Update::DeviceAction
/// "snapshot"; issue #43).
thread_local! {
    // Shows the newest live frame; set up in main(), run on the GUI
    // thread when ws_client says a frame came (FRAME_READY). Thread-local:
    // it holds the UI's Rc state, which can't leave the GUI thread.
    static SHOW_FRAME: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

/// How often the frame rate shown goes to the journal while Live plays
/// (compare with backend camera.rs's "frames/s from ffmpeg": lower here =
/// the screen can't keep up; the same = ffmpeg can't).
const FPS_LOG: Duration = Duration::from_secs(10);

/// A camera's newest live frame (ws_client::VIDEO) onto the screen, if
/// it's still the camera watched. `shown`: frames shown since when, for
/// the frame-rate log line.
fn show_video_frame(
    ui: &AppWindow,
    watching: &std::cell::RefCell<Option<(String, std::time::Instant)>>,
    shown: &std::cell::Cell<(u32, std::time::Instant)>,
) {
    let Some(frame) = ws_client::VIDEO.lock().unwrap().take() else { return };
    let mut watched = watching.borrow_mut();
    let Some((_, at)) = watched.as_mut().filter(|(id, _)| *id == frame.id) else { return };
    *at = std::time::Instant::now();
    // No copy: the picture was made on the connection thread.
    ui.set_controls_map(slint::Image::from_rgb8(frame.pixels));
    ui.set_controls_map_state("".into());
    let (count, since) = shown.get();
    if count == 0 || since.elapsed() >= 2 * FPS_LOG {
        // Counted from the first frame shown (not from the UI's start or
        // the last time Live played, which gave a "0.0" line).
        shown.set((1, std::time::Instant::now()));
    } else if since.elapsed() >= FPS_LOG {
        println!("ui_layer: live video {:.1} frames/s shown", (count + 1) as f32 / since.elapsed().as_secs_f32());
        shown.set((0, std::time::Instant::now()));
    } else {
        shown.set((count + 1, since));
    }
}

fn request_picture(tx: &std::sync::mpsc::Sender<ws_client::Request>, id: &str, live: bool) {
    let _ = tx.send(ws_client::Request::DeviceAction {
        id: id.to_string(),
        capability: "camera".into(),
        name: "snapshot".into(),
        args: serde_json::json!({ "live": live }),
    });
}

/// Asks backend_daemon for a vacuum's map (the answer: Update::DeviceAction
/// "map").
fn request_map(tx: &std::sync::mpsc::Sender<ws_client::Request>, id: &str) {
    let _ = tx.send(ws_client::Request::DeviceAction {
        id: id.to_string(),
        capability: "vacuum".into(),
        name: "map".into(),
        args: serde_json::json!({}),
    });
}

/// A vacuum's choices with their labels: "max" -> "Max", "max+" -> "Max+",
/// "off" -> "Off" (issue #74).
fn modes(values: &[String]) -> slint::ModelRc<ModeItem> {
    let label = |v: &str| {
        let mut chars = v.chars();
        chars.next().map_or(String::new(), |c| c.to_uppercase().collect::<String>() + chars.as_str())
    };
    std::rc::Rc::new(slint::VecModel::from(
        values.iter().map(|v| ModeItem { value: v.as_str().into(), label: label(v).into() }).collect::<Vec<_>>(),
    ))
    .into()
}

/// A list of texts for Slint (a climate's modes, its fan speeds).
fn string_model(items: &[String]) -> slint::ModelRc<slint::SharedString> {
    std::rc::Rc::new(slint::VecModel::from(items.iter().map(|i| i.as_str().into()).collect::<Vec<slint::SharedString>>())).into()
}

/// 21.5 -> "21.5 °C", 22.0 -> "22 °C".
fn celsius(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0} °C")
    } else {
        format!("{rounded:.1} °C")
    }
}

/// What a metering device draws, as the card's one line: power first
/// (the number people look at), then the total: "40.2 W \u{2022} 1.23 kWh".
/// Voltage and current are left out on the card -- too much for one line.
fn energy_text(energy: &ws_client::Energy) -> String {
    let power = if energy.power_w.abs() >= 1000.0 {
        format!("{:.2} kW", energy.power_w / 1000.0)
    } else {
        format!("{:.1} W", energy.power_w)
    };
    let mut parts = vec![power];
    if let Some(today) = energy.today_kwh {
        parts.push(format!("today {today:.2} kWh"));
    }
    if let Some(kwh) = energy.energy_kwh {
        parts.push(format!("{kwh:.2} kWh"));
    }
    parts.join(" \u{2022} ")
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
/// Settings > Accounts' rows (issue #74).
fn account_rows(accounts: &[ws_client::Account]) -> slint::ModelRc<AccountItem> {
    let vendor = |v: &str| match v {
        "ezviz" => "EZVIZ".to_string(),
        "roborock" => "Roborock".to_string(),
        other => other.to_string(),
    };
    let rows: Vec<AccountItem> = accounts
        .iter()
        .map(|a| AccountItem {
            id: a.id.as_str().into(),
            vendor: vendor(&a.vendor).into(),
            account: a.account.as_str().into(),
            devices_text: if a.devices.is_empty() {
                "No devices use it".to_string()
            } else {
                format!("Used by: {}", a.devices.join(", "))
            }
            .into(),
        })
        .collect();
    std::rc::Rc::new(slint::VecModel::from(rows)).into()
}

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
