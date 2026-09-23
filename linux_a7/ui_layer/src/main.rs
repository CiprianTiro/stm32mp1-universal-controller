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
    let renderer = fb_platform::FbRenderer::new(window.clone())
        .expect("failed to open the framebuffer -- is CONFIG_DRM_FBDEV_EMULATION on and is a display actually connected?");

    let ui = AppWindow::new().expect("failed to construct AppWindow from app.slint");

    // A touchscreen not being present shouldn't be fatal (see
    // touch_input.rs's own doc comment) -- `None` here just means the loop
    // below skips polling it every iteration, and the UI is then only
    // usable if some other input mechanism exists (there currently isn't
    // one, but a missing/disconnected touch panel is still a better
    // failure mode than refusing to display anything at all).
    let mut touch = touch_input::TouchInput::open().unwrap_or_else(|e| {
        println!("ui_layer: touch input unavailable: {e}");
        None
    });

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
        let _ = toggle_request_tx.send(!currently_on);
    });

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
                ws_client::Update::Connected => ui.set_connected(true),
                ws_client::Update::Disconnected => ui.set_connected(false),
                ws_client::Update::LedState(on) => ui.set_led_on(on),
            }
        }

        // 4. Actually draw, if anything changed as a result of the above
        //    (a touch, a property update, or an animation frame) --
        //    `draw_if_needed` (inside `FbRenderer`) checks that on its own,
        //    so this call is cheap on iterations where nothing did.
        renderer.draw_if_needed();

        // Don't spin the CPU checking all of the above hundreds of times a
        // second when nothing is happening -- if Slint has no pending
        // timer/animation that needs a specific wakeup time, fall back to a
        // fixed ~60fps poll interval, matching typical display refresh
        // rates without wasting cycles far beyond what a human could
        // perceive anyway.
        if !window.has_active_animations() {
            std::thread::sleep(
                slint::platform::duration_until_next_timer_update()
                    .unwrap_or(Duration::from_millis(16)),
            );
        }
    }
}
