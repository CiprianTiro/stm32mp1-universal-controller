// pointer.rs -- the mouse pointer (issue #38).
//
// Slint's KMS backend reads the mouse through libinput and clicks where
// the pointer is -- but with the software renderer the hub uses, it never
// DRAWS the pointer (its GPU renderers do; the software one skips that
// step). A mouse on the DK2 worked, invisibly. So app.slint draws the
// arrow itself (its pointer-x/-y/-visible properties), and this file keeps
// those properties where Slint's own, invisible pointer is.
//
// How: Slint lets the program look at every libinput event before Slint
// handles it (BackendSelector::with_libinput_event_hook, see main.rs).
// This file does exactly the same arithmetic Slint does with each event
// (i-slint-backend-linuxkms 1.18, calloop_backend/input.rs):
//   - the pointer starts in the middle of the screen;
//   - a mouse reports MOVEMENTS ("3 px right, 1 px up"), added up and kept
//     on the screen;
//   - a device reporting POSITIONS (a tablet, a virtual-machine mouse) is
//     scaled to the screen.
// Same starting point, same events, same arithmetic: the arrow always sits
// exactly on the spot Slint clicks. The hook only watches -- every event
// still goes on to Slint, with one exception: touches while the UI is on a
// monitor (see on_event).

use slint::{ComponentHandle, LogicalPosition};
use std::cell::{Cell, RefCell};

pub struct Pointer {
    /// Where the pointer is, in logical pixels; None until a mouse first
    /// moves (like Slint's own).
    pos: Cell<Option<LogicalPosition>>,
    /// The UI to show the arrow on. Set by attach(), right after the UI is
    /// created -- the hook exists before the UI does (it's handed to Slint
    /// when the backend is chosen). Weak: see the callbacks in main.rs.
    ui: RefCell<Option<slint::Weak<crate::AppWindow>>>,
    /// Whether the UI is on an external monitor (display::choose_output).
    on_monitor: bool,
}

impl Pointer {
    pub fn new(on_monitor: bool) -> Self {
        Pointer {
            pos: Cell::new(None),
            ui: RefCell::new(None),
            on_monitor,
        }
    }

    pub fn attach(&self, ui: &crate::AppWindow) {
        *self.ui.borrow_mut() = Some(ui.as_weak());
    }

    /// Called for every libinput event, before Slint handles it. Returns
    /// true to make Slint IGNORE the event: touches while the UI is on a
    /// monitor. The touchscreen is dark then (one screen at a time), but
    /// the touch panel still works -- and Slint would turn a finger on the
    /// dark panel into a click on the monitor's UI, at the same spot scaled
    /// up (seen on the DK2).
    pub fn on_event(&self, event: &input::Event) -> bool {
        use input::event::PointerEvent;
        if self.on_monitor && matches!(event, input::Event::Touch(_)) {
            return true;
        }
        let Some(ui) = self.ui.borrow().as_ref().and_then(|ui| ui.upgrade()) else {
            return false;
        };
        // The screen's size in logical pixels, as Slint computes it.
        let window = ui.window();
        let screen = window.size().to_logical(window.scale_factor());
        match event {
            input::Event::Pointer(PointerEvent::Motion(motion)) => {
                let old = self
                    .pos
                    .get()
                    .unwrap_or(LogicalPosition::new(screen.width / 2.0, screen.height / 2.0));
                self.show(
                    &ui,
                    LogicalPosition::new(
                        (old.x + motion.dx() as f32).clamp(0.0, screen.width),
                        (old.y + motion.dy() as f32).clamp(0.0, screen.height),
                    ),
                );
            }
            input::Event::Pointer(PointerEvent::MotionAbsolute(motion)) => {
                self.show(
                    &ui,
                    LogicalPosition::new(
                        motion.absolute_x_transformed(screen.width as u32) as f32,
                        motion.absolute_y_transformed(screen.height as u32) as f32,
                    ),
                );
            }
            // A finger on the touchscreen: no arrow in the way. It comes
            // back, at the same spot, with the next mouse movement.
            input::Event::Touch(_) => ui.set_pointer_visible(false),
            _ => {}
        }
        false
    }

    fn show(&self, ui: &crate::AppWindow, pos: LogicalPosition) {
        self.pos.set(Some(pos));
        ui.set_pointer_x(pos.x);
        ui.set_pointer_y(pos.y);
        ui.set_pointer_visible(true);
    }
}
