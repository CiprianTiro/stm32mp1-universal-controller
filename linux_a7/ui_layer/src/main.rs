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
    // `request_tx` to ask for a LED change, `update_rx` to receive
    // connection-status/LED-state updates.
    let (request_tx, update_rx) = ws_client::start();

    // Wires app.slint's `toggle-led` callback (fired when the button is
    // tapped -- see app.slint's `clicked =>` handler) to actually do
    // something: ask ws_client.rs's background thread to send a `SetLed`
    // request for the *opposite* of whatever's currently displayed.
    //
    // `ui.as_weak()`: a `Weak` reference to the UI, not a strong `Rc` one --
    // this callback closure gets stored *inside* `ui` itself (Slint wires
    // callbacks by holding onto the handler), so capturing `ui` by a strong
    // reference here would make the UI keep itself alive forever (a
    // reference cycle: ui -> callback -> ui). `.unwrap()` inside the
    // closure is safe because this program never actually drops `ui` while
    // it's still running -- the `Weak` upgrade only becomes `None` after
    // the window is torn down, which doesn't happen until the process
    // exits anyway.
    let ui_weak = ui.as_weak();
    let toggle_request_tx = request_tx.clone();
    ui.on_toggle_led(move || {
        let ui = ui_weak.unwrap();
        let currently_on = ui.get_led_on();
        // `let _ =`: if this send fails, ws_client.rs's background thread
        // has already exited (the receiving end was dropped) -- nothing
        // this callback can usefully do about that beyond not crashing.
        let _ = toggle_request_tx.send(ws_client::Request::SetLed { on: !currently_on });
    });

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

        // 3. Drain any updates from backend_daemon (connection status, or
        //    the LED's actual state) and reflect them in the UI's
        //    properties -- `try_recv()` never blocks, so this loop doesn't
        //    stall waiting for a message that might not be coming this
        //    iteration. Every update here is applied directly to app.slint's
        //    `led-on`/`connected` properties -- this is the *only* place in
        //    this whole crate that ever writes to them, matching ui_layer's
        //    "just displays what the daemon says" design (see this file's
        //    header comment): note in particular that tapping the button
        //    does NOT set `led-on` itself (see `on_toggle_led` above) --
        //    the display only ever changes once backend_daemon actually
        //    confirms it, arriving back here as one of these updates.
        while let Ok(update) = update_rx.try_recv() {
            match update {
                // ever-connected (issue #59): from the first successful
                // connection on, app.slint shows the normal screen instead
                // of the welcome screen -- and a LATER lost connection is
                // shown as "reconnecting", not as the welcome screen again.
                ws_client::Update::Connected => {
                    ui.set_connected(true);
                    ui.set_ever_connected(true);
                }
                ws_client::Update::Disconnected => ui.set_connected(false),
                ws_client::Update::LedState(on) => ui.set_led_on(on),
                ws_client::Update::Network(status) => ui.set_net(to_net_status(&status)),
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
                ws_client::Update::ActionDone(action, result) => apply_action_result(&ui, action, result),
            }
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
    }
}
