// touch_input.rs -- bridges the kernel's raw touch events into Slint's
// pointer events, so tapping the "Turn ON"/"Turn OFF" button on the DK2's
// physical touchscreen actually does anything.
//
// Unlike the framebuffer side (fb_platform.rs), Slint's software-renderer
// path doesn't come with ANY input handling built in -- that's a deliberate
// tradeoff of choosing that renderer over the alternative "backend-linuxkms"
// (which auto-detects and reads touch/mouse/keyboard via libinput, but
// needs a working DRM/KMS + libinput/libseat stack this board doesn't have
// the crate budget for -- see Cargo.toml's comment). Everything below is
// what that convenience would otherwise have done for us.
//
// The DK2's capacitive touch panel shows up as an ordinary Linux evdev
// device named "EP0110M09" (confirmed via /proc/bus/input/devices on the
// real board), reporting plain single-touch ABS_X/ABS_Y + BTN_TOUCH events
// -- not the multi-touch "protocol B" (ABS_MT_*) events some touchscreens
// use. Since this UI only ever needs to recognize one tap on one button,
// tracking multiple simultaneous touches isn't needed even if the hardware
// supported it, so this deliberately only implements the single-touch path.

use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode};
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::LogicalPosition;
use std::rc::Rc;

/// Finds and opens the touch panel's evdev device, or returns `None` if
/// nothing matching is plugged in. Rather than hardcoding a device path
/// like `/dev/input/event1` (which depends on enumeration order and could
/// shift if another input device were ever added), this looks for whichever
/// device the kernel has tagged with `INPUT_PROP_DIRECT` -- the standard
/// "this is a touchscreen, not a mouse/trackpad" marker every touchscreen
/// driver sets, which is a more robust match than the specific chip name.
fn find_touch_device() -> Option<Device> {
    for (_path, device) in evdev::enumerate() {
        if device.properties().contains(evdev::PropType::DIRECT) {
            return Some(device);
        }
    }
    None
}

/// Everything this module needs to keep running: the open device, and the
/// touch panel's raw coordinate ranges (read once at startup) so raw touch
/// values can be scaled into the logical pixel coordinates Slint expects
/// (see `scale` below) -- a touch controller's own coordinate space
/// (commonly something like 0..4095) very rarely matches the display's
/// actual pixel resolution.
pub struct TouchInput {
    device: Device,
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
    /// Whether a finger is currently down -- BTN_TOUCH's value (0 or 1)
    /// only tells us when it *changes*; this remembers the current state
    /// between reads so a `PointerMoved` (position-only) event knows
    /// whether it should really be treated as a drag or ignored.
    finger_down: bool,
    /// Last known finger position, in screen pixels. Kept between polls
    /// (not reset each time) because the kernel only sends ABS_X/ABS_Y
    /// when a coordinate CHANGES -- e.g. a finger lifting sends BTN_TOUCH=0
    /// with no coordinates at all, and the release must still be reported
    /// where the finger actually was.
    pos: LogicalPosition,
    /// Changes seen in the current, not-yet-finished event frame (see
    /// `poll()`): the new BTN_TOUCH state if it changed, and whether the
    /// position moved. Applied, then cleared, on each SYN_REPORT.
    pending_touch: Option<bool>,
    pending_move: bool,
}

impl TouchInput {
    /// Opens the touch device and reads its calibration range. Returns
    /// `Ok(None)` (not an error) if no touchscreen is present at all --
    /// main.rs treats that as "run with no touch input" rather than
    /// refusing to start, since a missing/disconnected touch panel
    /// shouldn't take down the whole UI when the display itself still
    /// works.
    pub fn open() -> Result<Option<Self>, String> {
        let Some(device) = find_touch_device() else {
            return Ok(None);
        };

        // `get_absinfo()` returns the calibration range for every
        // supported absolute axis at once; picking out just ABS_X/ABS_Y
        // here since this is a single-touch (not multi-touch) device, per
        // this file's header comment.
        let mut x_range = None;
        let mut y_range = None;
        for (code, info) in device
            .get_absinfo()
            .map_err(|e| format!("failed to read touch panel calibration: {e}"))?
        {
            if code == AbsoluteAxisCode::ABS_X {
                x_range = Some((info.minimum(), info.maximum()));
            } else if code == AbsoluteAxisCode::ABS_Y {
                y_range = Some((info.minimum(), info.maximum()));
            }
        }
        let (x_min, x_max) = x_range.ok_or("touch panel has no ABS_X range")?;
        let (y_min, y_max) = y_range.ok_or("touch panel has no ABS_Y range")?;
        // Logged once at startup (lands in `journalctl -u ui-layer`) so the
        // raw range can be compared against the 480x800 screen when
        // checking touch calibration.
        println!("ui_layer: touch panel X {x_min}..{x_max}, Y {y_min}..{y_max}");

        // evdev opens devices in BLOCKING mode by default: `fetch_events()`
        // would then sleep until the next touch arrives. main.rs calls
        // `poll()` from the same loop that draws the screen, so a blocking
        // read froze the whole UI -- nothing was ever drawn until someone
        // touched the panel. Non-blocking mode makes `fetch_events()`
        // return a WouldBlock error immediately when there's nothing new,
        // which `poll()` already treats as "no events this time".
        device
            .set_nonblocking(true)
            .map_err(|e| format!("failed to make touch device non-blocking: {e}"))?;

        Ok(Some(Self {
            device,
            x_min,
            x_max,
            y_min,
            y_max,
            finger_down: false,
            pos: LogicalPosition::new(0.0, 0.0),
            pending_touch: None,
            pending_move: false,
        }))
    }

    /// Reads whatever touch events have arrived since the last call and
    /// forwards them to Slint's window as pointer events, so a tap on the
    /// "Turn ON"/"Turn OFF" button actually triggers its `clicked` handler
    /// in app.slint. Meant to be called once per iteration of main.rs's
    /// loop -- `fetch_events()` never blocks waiting for new ones (`open()`
    /// switches the device to non-blocking mode), so calling this every
    /// loop iteration is cheap when nothing happened.
    ///
    /// evdev delivers input in FRAMES: a group of events that together
    /// describe one moment, ended by a SYN_REPORT event. A finger touching
    /// down arrives from this panel's driver as, in this order:
    ///
    ///     BTN_TOUCH=1, ABS_X=..., ABS_Y=..., SYN_REPORT
    ///
    /// Note BTN_TOUCH comes BEFORE the coordinates. Acting on each event
    /// the moment it arrives (as an earlier version of this function did)
    /// therefore reported the press at a stale/default position -- (0, 0),
    /// nowhere near the button -- so Slint never registered a click. So
    /// this collects a frame's changes first and only acts on SYN_REPORT,
    /// when the frame is complete. Frames can also be split across two
    /// `poll()` calls, which is why the pending state lives in `self`
    /// rather than in local variables.
    pub fn poll(&mut self, window: &Rc<MinimalSoftwareWindow>) {
        let events: Vec<_> = match self.device.fetch_events() {
            Ok(events) => events.collect(),
            Err(_) => return, // nothing new (WouldBlock) or the device went away -- try again next poll
        };

        for event in events {
            match event.destructure() {
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_X, value) => {
                    self.pos.x = self.scale_x(value);
                    self.pending_move = true;
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_Y, value) => {
                    self.pos.y = self.scale_y(value);
                    self.pending_move = true;
                }
                EventSummary::Key(_, KeyCode::BTN_TOUCH, value) => {
                    self.pending_touch = Some(value != 0);
                }
                EventSummary::Synchronization(_, evdev::SynchronizationCode::SYN_REPORT, _) => {
                    self.apply_frame(window);
                }
                _ => {} // multi-touch (ABS_MT_*) and other events aren't needed for a single-finger UI
            }
        }
    }

    /// Turns one completed evdev frame into at most one Slint pointer
    /// event, using the (now up-to-date) finger position.
    fn apply_frame(&mut self, window: &Rc<MinimalSoftwareWindow>) {
        let position = self.pos;
        match self.pending_touch.take() {
            Some(true) if !self.finger_down => {
                self.finger_down = true;
                window.dispatch_event(WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                });
            }
            Some(false) if self.finger_down => {
                self.finger_down = false;
                window.dispatch_event(WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                });
                // Tell Slint the pointer is gone, like a mouse leaving the
                // window -- otherwise the button would keep its "hovered"
                // look after the finger lifts, which makes no sense on a
                // touchscreen.
                window.dispatch_event(WindowEvent::PointerExited);
            }
            // No press/release in this frame: a finger sliding while down
            // is a drag.
            _ if self.pending_move && self.finger_down => {
                window.dispatch_event(WindowEvent::PointerMoved { position });
            }
            _ => {}
        }
        self.pending_move = false;
    }

    /// Maps a raw touch-panel X reading (in whatever range this specific
    /// touch controller reports, e.g. 0..4095) onto 0..width logical
    /// pixels. NOTE: this assumes the touch panel's coordinate axes are
    /// already aligned with the display's (no swap/inversion needed) --
    /// true for most panels, but this is exactly the kind of thing that
    /// can only really be confirmed by tapping the actual screen and
    /// checking the touch lands where expected, not by reading a
    /// datasheet; adjust here if testing on real hardware shows X/Y
    /// swapped or inverted.
    fn scale_x(&self, raw: i32) -> f32 {
        scale(raw, self.x_min, self.x_max, WIDTH)
    }

    fn scale_y(&self, raw: i32) -> f32 {
        scale(raw, self.y_min, self.y_max, HEIGHT)
    }
}

// The DK2's onboard MIPI-DSI panel's resolution -- see the GitHub wiki's
// "M4-Firmware" page / ST's own board documentation. Used only for scaling
// touch coordinates; the actual rendered window size comes from
// fb_platform.rs reading the framebuffer's real geometry directly, so if
// these two ever disagreed the picture would still be correct, only touch
// coordinates would be off -- another reason to verify touch placement
// empirically rather than trusting this constant alone.
const WIDTH: f32 = 480.0;
const HEIGHT: f32 = 800.0;

fn scale(raw: i32, min: i32, max: i32, out_max: f32) -> f32 {
    if max <= min {
        return 0.0; // degenerate calibration range -- nothing sensible to scale to
    }
    ((raw - min) as f32 / (max - min) as f32) * out_max
}
