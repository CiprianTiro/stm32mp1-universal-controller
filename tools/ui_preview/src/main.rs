// ui-preview -- renders every page of the hub UI to PNG files, at the
// screen sizes the hub runs at (issue #38), with made-up sample data.
//
//     cargo run --release --manifest-path tools/ui_preview/Cargo.toml -- <folder> [WxH ...]
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

thread_local! {
    /// The sample devices, kept to split them into rows for each size.
    static DEVICES: std::cell::RefCell<Vec<DeviceItem>> = Default::default();
}

/// Sets the window size, then the device rows for that size's column count.
fn set_size_and_rows(ui: &AppWindow, window: &MinimalSoftwareWindow, width: u32, height: u32) {
    window.set_size(PhysicalSize::new(width, height));
    let columns = ui.get_columns().max(1) as usize;
    let rows: Vec<slint::ModelRc<DeviceItem>> = DEVICES.with(|d| {
        d.borrow().chunks(columns).map(|row| Rc::new(VecModel::from(row.to_vec())).into()).collect()
    });
    ui.set_device_rows(Rc::new(VecModel::from(rows)).into());
}

/// The pages worth looking at, with the number app.slint uses for each.
/// None: the welcome screen (shown until the backend first answers).
const PAGES: [(&str, Option<i32>); 8] = [
    ("welcome", None),
    ("devices", Some(0)),
    ("network", Some(1)),
    ("password", Some(2)),
    ("country", Some(3)),
    ("settings", Some(4)),
    ("clients", Some(5)),
    ("pairing", Some(6)),
];

/// Slint needs a Platform before any window exists; this one hands out the
/// in-memory window. The event loop is never run (frames are drawn by hand).
struct PreviewPlatform(Rc<MinimalSoftwareWindow>);

impl Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
    fill_sample_data(&ui);
    ui.show().unwrap();

    for (width, height) in sizes {
        set_size_and_rows(&ui, &window, width, height);
        for (name, page) in PAGES {
            ui.set_ever_connected(page.is_some());
            ui.set_page(page.unwrap_or(0));
            let path = format!("{out_dir}/{width}x{height}-{name}.png");
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
fn fill_sample_data(ui: &AppWindow) {
    ui.set_connected(true);
    let device = |id: &str, name: &str, room: &str| DeviceItem {
        id: id.into(),
        name: name.into(),
        room: room.into(),
        ..Default::default()
    };
    let devices = vec![
        DeviceItem { has_switch: true, on: true, ..device("ld7", "Board LED (LD7)", "Hub") },
        DeviceItem { has_switch: true, on: true, has_dimmer: true, level: 42, has_color: true,
                     color: slint::Color::from_rgb_u8(0, 255, 136), color_text: "#00FF88".into(),
                     ..device("bulb", "Hall bulb", "Hall") },
        DeviceItem { has_switch: true, on: false, has_dimmer: true, level: 73, ..device("lamp-1", "Desk lamp", "Office") },
        DeviceItem { has_switch: true, on: true, ..device("tv", "Living room TV", "Living room") },
        DeviceItem { sensor_text: "temperature 21.5 °C   humidity 48 %".into(), ..device("climate", "Climate sensor", "Bedroom") },
        DeviceItem { has_switch: true, on: false, ..device("kettle", "Kettle", "Kitchen") },
    ];
    // Split into rows the way main.rs's DeviceRows does, for the column
    // count app.slint computes -- which depends on the window size, so the
    // rows are made again for every size (see set_size_and_rows).
    DEVICES.with(|d| *d.borrow_mut() = devices);

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
