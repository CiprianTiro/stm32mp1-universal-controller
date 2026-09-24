// fb_platform.rs -- makes Slint render directly onto /dev/fb0, with no
// window system (X11/Wayland/compositor) involved at all, per issue #14's
// DoD. This is the piece Slint's "renderer-software" gives you the tools
// to build but doesn't provide out of the box (see Cargo.toml's comment on
// why that renderer was chosen over the alternative DRM-based backend).
//
// Two separate things live in this file, and it's worth being clear about
// why they're split into two structs instead of one:
//
//   - `FbRenderer`: owns the actual open framebuffer device and does the
//     real work of copying rendered pixels into it. main.rs keeps this
//     around and calls `draw_if_needed()` on it once per loop iteration.
//   - `WindowOnlyPlatform`: the bare minimum needed to satisfy Slint's
//     `slint::platform::Platform` trait, which is required once, globally,
//     via `slint::platform::set_platform()` before `AppWindow::new()` can
//     be called at all. Registering a platform *consumes* it (hands
//     ownership to Slint's internal runtime state) -- if `FbRenderer`
//     itself were registered that way, main.rs would lose access to the
//     framebuffer it still needs to draw into every frame. So instead, only
//     this small wrapper (holding just a cheap `Rc` clone of the shared
//     window, not the framebuffer) gets handed away; `FbRenderer` stays
//     entirely in main.rs's hands.
//
// The window itself, `slint::platform::software_renderer::
// MinimalSoftwareWindow`, is a ready-made implementation Slint provides --
// not something this file builds. Both structs above just hold their own
// `Rc` clone of the *same* one (see main.rs for where it's created once and
// split two ways).

use linuxfb::Framebuffer;
use memmap2::{MmapMut, MmapOptions};
use slint::platform::software_renderer::{MinimalSoftwareWindow, Rgb565Pixel};
use slint::platform::{Platform, WindowAdapter};
use slint::{PhysicalSize, PlatformError};
use std::cell::RefCell;
use std::fs::OpenOptions;
use std::rc::Rc;

const FB_PATH: &str = "/dev/fb0";

/// sysfs file where the kernel publishes fb0's real row length in BYTES
/// (the `line_length` field of the framebuffer's "fixed screen info").
/// linuxfb reads that same field internally but doesn't expose it, so it's
/// read from here instead -- same number, no extra ioctl code needed.
const FB_STRIDE_SYSFS: &str = "/sys/class/graphics/fb0/stride";

/// sysfs file with the name of the driver that currently owns fb0.
const FB_NAME_SYSFS: &str = "/sys/class/graphics/fb0/name";

/// The name fb0 has while it still belongs to "simpledrm" (issue #59).
///
/// What happens to the display during boot: U-Boot sets up the panel and
/// draws the welcome image. The kernel then keeps that very picture on
/// screen through a stand-in driver, simpledrm, which just takes over
/// U-Boot's framebuffer memory as /dev/fb0. ~13 s later (on the DK2) udev
/// loads the real display driver (stm, "stmdrmfb"); it resets the panel and
/// REPLACES /dev/fb0 with its own. A UI that opened the stand-in fb0 would
/// be left drawing into memory that is no longer on screen -- so the UI
/// must wait for the real one.
const STANDIN_FB_NAME: &str = "simpledrmdrmfb";

/// Waits until /dev/fb0 belongs to the real display driver (see
/// STANDIN_FB_NAME). Since issue #59 the UI starts early in boot, long
/// before that driver is loaded, and simply waits here -- then draws its
/// first frame within a fraction of a second of the display becoming
/// usable, instead of whenever systemd would otherwise have got round to
/// starting it.
///
/// Checks every 50 ms (reading one small sysfs file: negligible CPU).
/// Gives up waiting after `timeout` and carries on with whatever fb0 there
/// is -- e.g. a kernel with the display driver built in never has the
/// stand-in, and an image without it still gets a (maybe blank) UI rather
/// than none.
pub fn wait_for_display_driver(timeout: std::time::Duration) {
    let start = std::time::Instant::now();
    let mut announced = false;
    loop {
        let name = std::fs::read_to_string(FB_NAME_SYSFS).unwrap_or_default();
        let name = name.trim();
        if !name.is_empty() && name != STANDIN_FB_NAME {
            if announced {
                println!("ui_layer: display driver \"{name}\" ready after {:.1} s", start.elapsed().as_secs_f32());
            }
            return;
        }
        if start.elapsed() >= timeout {
            println!("ui_layer: display driver not ready after {} s (fb0: \"{name}\"), starting anyway", timeout.as_secs());
            return;
        }
        if !announced {
            println!("ui_layer: waiting for the display driver (fb0: \"{name}\")");
            announced = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Owns the mapped `/dev/fb0` memory and actually draws into it. See this
/// file's header comment for why this is kept separate from the `Platform`
/// registration below.
pub struct FbRenderer {
    window: Rc<MinimalSoftwareWindow>,
    /// The framebuffer's pixel memory, mapped once at startup and kept for
    /// the life of the program. `RefCell` because `draw_if_needed` only has
    /// `&self` but writing pixels needs mutable access.
    mem: RefCell<MmapMut>,
    /// Distance from the start of one row to the start of the next, in
    /// PIXELS. This is NOT always the visible width: display controllers
    /// often pad each row for memory alignment. On the DK2 the screen is
    /// 480 px wide (960 bytes at 2 bytes/px), but each row actually takes
    /// 1024 bytes = 512 px, so 32 px of invisible padding per row. Using 480
    /// here made every row start 32 px too early, shearing the picture into
    /// diagonal garbage -- the bug this field's sysfs read fixes.
    stride: usize,
    /// The hidden buffer every frame is drawn into first (issue #34):
    /// same layout as the framebuffer (stride x height pixels). Only the
    /// finished, changed area is then copied to the screen -- see
    /// draw_if_needed.
    back: RefCell<Vec<Rgb565Pixel>>,
}

impl FbRenderer {
    /// Opens `/dev/fb0`, forces it into 16-bit RGB565 mode (the pixel
    /// format Slint's software renderer is being told to render in below,
    /// via `Rgb565Pixel` -- the two have to match, or the picture would
    /// come out with the wrong colors, not just fail to compile, since
    /// nothing here checks the two against each other beyond this explicit
    /// request), and wraps the given window (see main.rs for where it's
    /// created).
    ///
    /// Returns an error rather than panicking on any failure, so main.rs
    /// can decide how to report it (this is meant to run as a systemd
    /// service with no terminal attached, where a panic's backtrace would
    /// otherwise just vanish into the journal without much context).
    pub fn new(window: Rc<MinimalSoftwareWindow>) -> Result<Self, String> {
        let mut fb = Framebuffer::new(FB_PATH).map_err(|e| format!("failed to open {FB_PATH}: {e:?}"))?;

        /* This board's DRM driver doesn't fix bytes-per-pixel to a single
         * value -- ask explicitly for 2 (RGB565) rather than assuming
         * whatever the kernel defaulted to (commonly 4-byte XRGB8888 for
         * DRM fbdev emulation specifically), since Slint's software
         * renderer needs to be told a concrete pixel format at compile
         * time (`Rgb565Pixel`, below) and there's no code here that
         * adapts to a different one at runtime. */
        fb.set_bytes_per_pixel(2)
            .map_err(|e| format!("failed to set {FB_PATH} to 16bpp RGB565: {e:?}"))?;

        // The window is created (in main.rs) before this struct exists, at
        // which point its real size isn't known yet -- set it now that the
        // framebuffer has actually been opened and queried, so Slint lays
        // out app.slint's UI at the screen's true resolution rather than
        // some placeholder.
        let (width, height) = fb.get_size();
        window.set_size(PhysicalSize::new(width, height));

        // Real row length, read AFTER the bytes-per-pixel change above
        // (changing the pixel format can change it). The fb's "virtual
        // width" is not a substitute: here it reports 480 while rows are
        // really 512 px apart -- see the `stride` field's comment.
        let stride_bytes: usize = std::fs::read_to_string(FB_STRIDE_SYSFS)
            .map_err(|e| format!("failed to read {FB_STRIDE_SYSFS}: {e}"))?
            .trim()
            .parse()
            .map_err(|e| format!("bad value in {FB_STRIDE_SYSFS}: {e}"))?;
        let stride = stride_bytes / std::mem::size_of::<Rgb565Pixel>();

        // Map the pixel memory ourselves rather than via linuxfb's `map()`:
        // that one sizes the mapping as width * height * bytes-per-pixel,
        // i.e. it makes the same "no row padding" assumption and would map
        // too little memory (768,000 bytes instead of the 819,200 that 800
        // padded rows really need) -- Slint would then write past its end.
        // A second, independent open of the same device is fine; the
        // pixel-format setting above is a property of the device itself,
        // not of the file handle linuxfb used to set it.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(FB_PATH)
            .map_err(|e| format!("failed to open {FB_PATH} for mapping: {e}"))?;
        let len = stride_bytes * height as usize;
        // unsafe: mmap is inherently unsafe in Rust because another process
        // could change the memory underneath us. For a framebuffer that's
        // expected and harmless -- we only ever write pixels into it.
        let mem = unsafe { MmapOptions::new().len(len).map_mut(&file) }
            .map_err(|e| format!("failed to mmap {FB_PATH} ({len} bytes): {e}"))?;

        // Starts black, like the freshly initialised screen; the first frame
        // redraws (and copies) everything anyway.
        let back = vec![Rgb565Pixel::default(); stride * height as usize];

        Ok(Self {
            window,
            mem: RefCell::new(mem),
            stride,
            back: RefCell::new(back),
        })
    }

    /// Draws the current UI state into the framebuffer, if (and only if)
    /// Slint thinks something actually changed since the last frame --
    /// `draw_if_needed` handles that check internally, so calling this
    /// every loop iteration in main.rs doesn't mean redrawing every loop
    /// iteration.
    pub fn draw_if_needed(&self) {
        self.window.draw_if_needed(|renderer| {
            // WHY TWO STEPS (issue #34): Slint draws a frame in layers --
            // a row's background, then a toggle's track, then its knob,
            // then the soft (anti-aliased) edges -- touching the same
            // pixels several times. The display controller reads the
            // framebuffer ~60 times a second at moments of its own choosing,
            // so when Slint drew straight into it, the screen sometimes
            // showed a half-drawn frame: visible as flickering pixels while
            // a toggle's knob slid across (seen on the DK2). Drawing into
            // `back` first and then copying only the FINISHED pixels means
            // the screen never sees an in-between state; the copy of a
            // changed area is one fast memcpy per line.
            let mut back = self.back.borrow_mut();
            // `render` returns which parts it changed (RepaintBufferType::
            // ReusedBuffer: it relies on `back` still holding the previous
            // frame, which it does -- it's ours, and only Slint writes it).
            let changed = renderer.render(&mut back, self.stride);

            // Borrow the mapping made once in `new()` (no per-frame mmap).
            let mut frame = self.mem.borrow_mut();

            // The framebuffer's mapped memory comes back as raw bytes
            // (`&mut [u8]`); here it's viewed as a slice of `Rgb565Pixel`
            // (each one exactly 2 bytes). `align_to_mut` is how you
            // reinterpret one slice as another plain-data type in Rust
            // without copying -- `unsafe` only because the compiler can't
            // itself prove the alignment works out; RGB565 framebuffers are
            // 2-byte-aligned by construction.
            let (_, screen, _) = unsafe { frame.align_to_mut::<Rgb565Pixel>() };

            // Copy the changed rectangles, line by line.
            for (origin, size) in changed.iter() {
                let (x, y) = (origin.x.max(0) as usize, origin.y.max(0) as usize);
                for line in y..y + size.height as usize {
                    let start = line * self.stride + x;
                    let end = start + size.width as usize;
                    if end <= back.len() && end <= screen.len() {
                        screen[start..end].copy_from_slice(&back[start..end]);
                    }
                }
            }
        });
    }
}

/// The bare minimum object Slint's `Platform` trait needs -- see this
/// file's header comment for why this holds only a window handle, not the
/// framebuffer itself.
pub struct WindowOnlyPlatform(pub Rc<MinimalSoftwareWindow>);

impl Platform for WindowOnlyPlatform {
    /// Slint calls this once, when `AppWindow::new()` runs, to get
    /// something to render into.
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }

    /// Slint's `Platform` trait requires this method to exist, but this
    /// project's main.rs never actually calls it (it never calls
    /// `ui.run()`, which is the only thing that would) -- main.rs drives
    /// its own loop directly instead, specifically so it can also poll
    /// touch input (touch_input.rs) and WebSocket updates (ws_client.rs)
    /// each iteration, neither of which Slint's own built-in event loop has
    /// any way to know about. `unreachable!()` documents that as a real
    /// invariant, not just an oversight -- if this ever *does* get called,
    /// that's a bug in main.rs (calling `.run()` after all), not a
    /// legitimate code path to silently support.
    fn run_event_loop(&self) -> Result<(), PlatformError> {
        unreachable!("main.rs drives its own event loop instead of calling ui.run()")
    }
}
