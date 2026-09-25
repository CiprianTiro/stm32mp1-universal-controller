// display.rs -- which screen the UI shows on (issue #38).
//
// The DK2 has two display outputs: the built-in touchscreen (MIPI-DSI,
// 480x800, "DSI-1") and an HDMI port (through a SiI9022 bridge chip,
// "HDMI-A-1"). The display controller (LTDC) can drive only ONE of them at
// a time -- it has a single output channel. The rule (decided in #38):
// **a connected monitor takes over**; without one, the touchscreen.
//
// Slint's KMS backend does the actual drawing. It shows on whichever
// output the SLINT_DRM_OUTPUT environment variable names, in the mode
// SLINT_DRM_MODE picks (see Card::outputs for why we choose that too), so
// this file's job is to set those variables before Slint starts, and later to notice a
// monitor being plugged in or out (main.rs then restarts the UI, which
// makes the choice again).
//
// Until #38 the UI drew into /dev/fb0 (the old fb_platform.rs). The kernel
// binds /dev/fb0 to one output at boot -- the touchscreen -- so a monitor
// never got a picture. Slint's KMS backend uses /dev/dri/card0 directly.

use drm::control::{connector, Device as ControlDevice};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

/// The DRM device the display controller shows up as.
const CARD_PATH: &str = "/dev/dri/card0";
/// Its sysfs folder: the driver behind it, and one folder per output
/// ("card0-HDMI-A-1", ...).
const CARD_SYSFS: &str = "/sys/class/drm/card0";
const DRM_SYSFS: &str = "/sys/class/drm";

/// The driver of the boot-time stand-in display (issue #59).
///
/// What happens to the display during boot: U-Boot sets up the panel and
/// draws the welcome image. The kernel then keeps that very picture on
/// screen through a stand-in driver, simpledrm, which just takes over
/// U-Boot's framebuffer memory -- as /dev/dri/card0. ~13 s later (on the
/// DK2) udev loads the real display driver (stm32-display); it resets the
/// panel and REPLACES card0 with its own. A UI that opened the stand-in
/// would be left drawing into memory that is no longer on screen -- so the
/// UI must wait for the real one.
const STANDIN_DRIVER: &str = "simple-framebuffer";

/// Waits until /dev/dri/card0 belongs to the real display driver (see
/// STANDIN_DRIVER). Since issue #59 the UI starts early in boot, long
/// before that driver is loaded, and simply waits here -- then draws its
/// first frame within a fraction of a second of the display becoming
/// usable.
///
/// "Ready" also means the UI may OPEN card0 (issue #37). The UI runs as
/// user hubui, allowed in only through group "video" -- but the kernel
/// creates the new device node as root-only, and udev sets its group a
/// moment AFTER the driver has appeared. Opening in that gap fails with
/// "permission denied" (seen on the DK2 with /dev/fb0: the UI crashed and
/// was restarted by systemd 2 s later). So the open is checked too.
///
/// Checks every 50 ms (one symlink read and one open: negligible CPU).
/// Gives up waiting after `timeout` and carries on with whatever there is
/// -- e.g. a kernel with the display driver built in never has the
/// stand-in, and a board without it still gets a (maybe blank) UI rather
/// than none.
pub fn wait_for_display_driver(timeout: Duration) {
    let start = Instant::now();
    let mut announced = false;
    loop {
        // /sys/class/drm/card0/device/driver is a link to the driver's own
        // folder, named after the driver.
        let driver = std::fs::read_link(format!("{CARD_SYSFS}/device/driver"))
            .ok()
            .and_then(|link| link.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        // Opened only to test the permissions: closed again right away.
        let can_open = open_card().is_ok();
        if !driver.is_empty() && driver != STANDIN_DRIVER && can_open {
            if announced {
                println!("ui_layer: display driver \"{driver}\" ready after {:.1} s", start.elapsed().as_secs_f32());
            }
            return;
        }
        if start.elapsed() >= timeout {
            println!("ui_layer: display driver not ready after {} s (card0: \"{driver}\", can open: {can_open}), starting anyway", timeout.as_secs());
            return;
        }
        if !announced {
            println!("ui_layer: waiting for the display driver (card0: \"{driver}\", can open: {can_open})");
            announced = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Picks the output (see this file's header) and tells Slint, through
/// SLINT_DRM_OUTPUT. Returns the chosen output's name, e.g. "HDMI-A-1", or
/// None if nothing usable was found -- Slint then picks by itself.
///
/// Must run BEFORE Slint's backend opens the display, and must CLOSE the
/// device again (it does: `card` is dropped at the end). Only one program
/// at a time may be the "DRM master", the one allowed to change what's on
/// screen, and the first to open the device becomes it -- that has to be
/// Slint, not this function.
pub fn choose_output() -> Option<String> {
    let card = match open_card() {
        Ok(card) => card,
        Err(e) => {
            println!("ui_layer: can't open {CARD_PATH} to choose a screen ({e}), leaving it to Slint");
            return None;
        }
    };
    let outputs = card.outputs();
    for output in &outputs {
        println!(
            "ui_layer: output {}: {}, {} usable mode(s){}",
            output.name,
            if output.connected { "connected" } else { "not connected" },
            output.modes,
            if output.external { ", external" } else { "" }
        );
    }
    // Usable = connected AND with at least one mode the display controller
    // can produce. A monitor offering only modes it can't (a 4K-only
    // screen, say) doesn't count: better the touchscreen than a black UI.
    let usable = |o: &&Output| o.connected && o.modes > 0;
    let chosen = outputs
        .iter()
        .filter(usable)
        .find(|o| o.external)
        .or_else(|| outputs.iter().find(usable))?;
    println!("ui_layer: showing the UI on {} at {}", chosen.name, chosen.mode_text);
    // Safe to set here: nothing else runs yet (ws_client's thread starts
    // later), and the variables are only read by Slint, in this process.
    std::env::set_var("SLINT_DRM_OUTPUT", &chosen.name);
    if let Some(index) = chosen.best_mode {
        std::env::set_var("SLINT_DRM_MODE", index.to_string());
    }
    Some(chosen.name.clone())
}

/// Whether an external monitor is connected right now, going by the
/// kernel's own record (sysfs "status" of each external output). Cheap: a
/// few tiny file reads, no probing of the monitor. The HDMI chip reports
/// plugging and unplugging to the kernel by itself (a "hot plug" signal),
/// so this record is up to date.
///
/// Doesn't check the monitor's modes (that needs the DRM device, which
/// Slint holds): if a monitor with no usable mode is plugged in, the UI
/// restarts once and choose_output() keeps the touchscreen.
pub fn external_connected() -> bool {
    let Ok(entries) = std::fs::read_dir(DRM_SYSFS) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        // "card0-HDMI-A-1" -> "HDMI-A-1". Only card0's outputs.
        let Some(output) = name.strip_prefix("card0-") else {
            return false;
        };
        is_external(output)
            && std::fs::read_to_string(entry.path().join("status")).is_ok_and(|s| s.trim() == "connected")
    })
}

/// External = a socket someone plugs a monitor into, as opposed to a panel
/// built into the device. By the name's type part, which is the kernel's
/// standard naming (the same names sysfs and Slint use).
pub fn is_external(name: &str) -> bool {
    ["HDMI", "DP-", "DVI", "VGA"].iter().any(|kind| name.starts_with(kind))
}

/// One display output, as choose_output() sees it.
struct Output {
    /// "DSI-1", "HDMI-A-1": the name SLINT_DRM_OUTPUT expects.
    name: String,
    connected: bool,
    /// How many modes the kernel accepted (after dropping the ones the
    /// display controller can't produce).
    modes: usize,
    /// The mode to use, as its position in the kernel's list (what
    /// SLINT_DRM_MODE expects), and how it reads in the log.
    best_mode: Option<usize>,
    mode_text: String,
    external: bool,
}

/// The open DRM device. The drm crate's traits provide the ioctls; they
/// only need access to the file descriptor (AsFd).
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl ControlDevice for Card {}

impl Card {
    /// All outputs, with the monitor asked for its modes on the spot.
    ///
    /// `get_connector(.., true)`: "force probe", i.e. read the monitor's
    /// list of modes (its EDID) NOW, instead of using what the kernel
    /// remembers. At boot the kernel's first look at the HDMI port found
    /// the monitor but no modes (seen on the DK2); a fresh probe gets them.
    /// The kernel only probes for the DRM master, which this process is at
    /// this point (the first to open the device).
    fn outputs(&self) -> Vec<Output> {
        let Ok(resources) = self.resource_handles() else {
            return Vec::new();
        };
        resources
            .connectors()
            .iter()
            .filter_map(|&handle| self.get_connector(handle, true).ok())
            .map(|info| {
                let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
                // The largest mode, and among equally large ones the
                // highest refresh rate. Slint on its own only compares the
                // size, and of the monitor's two 1280x720 modes (60 Hz, and
                // the 50 Hz of European TV) it took the 50 Hz one -- every
                // animation at 50 instead of 60 frames per second (measured
                // on the DK2 with a Dell monitor).
                let best = info
                    .modes()
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, mode)| {
                        let (width, height) = mode.size();
                        (u32::from(width) * u32::from(height), mode.vrefresh())
                    });
                Output {
                    external: is_external(&name),
                    connected: info.state() == connector::State::Connected,
                    modes: info.modes().len(),
                    best_mode: best.map(|(index, _)| index),
                    mode_text: best.map_or(String::new(), |(_, mode)| {
                        format!("{}x{} {} Hz", mode.size().0, mode.size().1, mode.vrefresh())
                    }),
                    name,
                }
            })
            .collect()
    }
}

fn open_card() -> std::io::Result<Card> {
    OpenOptions::new().read(true).write(true).open(CARD_PATH).map(Card)
}
