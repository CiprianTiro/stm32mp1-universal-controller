// vacuum_map.rs -- draws a robot vacuum's map (issue #74).
//
// backend_daemon sends the map small (roborock_map.rs, Map::to_json): the
// floor plan as one class per 5 cm square -- 0 outside, 1 wall, 2 floor,
// 10+n room n -- run-length packed ([value, count]...) and base64, plus
// the dock, the vacuum (with its heading), the path it drove, no-go areas
// and virtual walls, in that image's pixels (y down).
//
// Here it becomes a picture: each square SCALE x SCALE screen pixels, in
// the theme's colours (rooms each in a soft tint of their own), the path
// as a thin line, the dock as a green dot, the vacuum as an accent dot in a
// ring. Drawn into a plain RGBA buffer -- Slint shows it as
// an Image; no drawing library needed.

use base64::Engine;
use serde_json::Value;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// The colours, from the theme (main.rs reads them).
pub struct Colors {
    pub floor: slint::Color,
    pub wall: slint::Color,
    pub path: slint::Color,
    pub robot: slint::Color,
    pub dock: slint::Color,
    pub no_go: slint::Color,
}

/// Rooms' tints: soft, told apart from each other and from the floor.
const ROOM_TINTS: [(u8, u8, u8); 8] = [
    (110, 160, 220),
    (120, 190, 140),
    (220, 170, 100),
    (190, 130, 200),
    (100, 190, 190),
    (220, 130, 130),
    (170, 170, 110),
    (140, 150, 220),
];

/// The largest picture made (pixels on its longer side): sharp on the
/// touchscreen and a monitor, small enough to draw quickly on the A7.
const TARGET: usize = 720;

/// base64 + runs -> one class byte per square.
fn unpack(text: &str, size: usize) -> Option<Vec<u8>> {
    let runs = base64::engine::general_purpose::STANDARD.decode(text).ok()?;
    let mut out = Vec::with_capacity(size);
    for pair in runs.chunks_exact(2) {
        out.extend(std::iter::repeat_n(pair[0], pair[1] as usize));
        if out.len() > size {
            return None;
        }
    }
    (out.len() == size).then_some(out)
}

fn point(value: &Value) -> Option<(f32, f32)> {
    Some((value.get(0)?.as_f64()? as f32, value.get(1)?.as_f64()? as f32))
}

/// The map as an image; None if it isn't one.
pub fn render(map: &Value, colors: &Colors) -> Option<Image> {
    let width = map["width"].as_u64()? as usize;
    let height = map["height"].as_u64()? as usize;
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return None;
    }
    let classes = unpack(map["pixels"].as_str()?, width * height)?;
    let scale = (TARGET / width.max(height)).clamp(1, 6);
    let (w, h) = (width * scale, height * scale);
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
    let pixels = buffer.make_mut_slice();
    let rgba = |c: slint::Color| Rgba8Pixel { r: c.red(), g: c.green(), b: c.blue(), a: c.alpha() };
    let clear = Rgba8Pixel { r: 0, g: 0, b: 0, a: 0 };

    // The plan.
    for y in 0..height {
        for x in 0..width {
            let color = match classes[x + width * y] {
                0 => clear,
                1 => rgba(colors.wall),
                2 => rgba(colors.floor),
                n => {
                    // Half the room's tint, half the floor: rooms told
                    // apart, not shouting.
                    let (r, g, b) = ROOM_TINTS[(n as usize - 10) % ROOM_TINTS.len()];
                    let mix = |tint: u8, floor: u8| ((u16::from(tint) + u16::from(floor)) / 2) as u8;
                    Rgba8Pixel {
                        r: mix(r, colors.floor.red()),
                        g: mix(g, colors.floor.green()),
                        b: mix(b, colors.floor.blue()),
                        a: 255,
                    }
                }
            };
            for dy in 0..scale {
                let row = (y * scale + dy) * w + x * scale;
                pixels[row..row + scale].fill(color);
            }
        }
    }

    // Draws a dot of radius r (in screen pixels) at map point p.
    let mut dot = |pixels: &mut [Rgba8Pixel], (px, py): (f32, f32), r: f32, color: Rgba8Pixel| {
        let (cx, cy) = (px * scale as f32 + scale as f32 / 2.0, py * scale as f32 + scale as f32 / 2.0);
        let (x0, x1) = ((cx - r).floor().max(0.0) as usize, ((cx + r).ceil() as usize).min(w.saturating_sub(1)));
        let (y0, y1) = ((cy - r).floor().max(0.0) as usize, ((cy + r).ceil() as usize).min(h.saturating_sub(1)));
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (dx, dy) = (x as f32 - cx, y as f32 - cy);
                if dx * dx + dy * dy <= r * r {
                    pixels[y * w + x] = color;
                }
            }
        }
    };
    // A line of dots from a to b (thin: the path, a virtual wall).
    let line = |pixels: &mut [Rgba8Pixel], dot: &mut dyn FnMut(&mut [Rgba8Pixel], (f32, f32), f32, Rgba8Pixel), a: (f32, f32), b: (f32, f32), r: f32, color: Rgba8Pixel| {
        let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()) * 2.0).ceil().max(1.0) as usize;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            dot(pixels, (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t), r, color);
        }
    };

    let thin = (scale as f32 / 3.0).max(0.6);
    // No-go areas: their outline. Virtual walls: lines.
    let no_go = rgba(colors.no_go);
    for area in map["no_go"].as_array().into_iter().flatten() {
        let corners: Vec<(f32, f32)> = area.as_array().into_iter().flatten().filter_map(point).collect();
        for i in 0..corners.len() {
            line(pixels, &mut dot, corners[i], corners[(i + 1) % corners.len()], thin, no_go);
        }
    }
    for wall in map["walls"].as_array().into_iter().flatten() {
        if let (Some(a), Some(b)) = (wall.get(0).and_then(point), wall.get(1).and_then(point)) {
            line(pixels, &mut dot, a, b, thin * 1.5, no_go);
        }
    }
    // The path it drove.
    let path: Vec<(f32, f32)> = map["path"].as_array().into_iter().flatten().filter_map(point).collect();
    for pair in path.windows(2) {
        line(pixels, &mut dot, pair[0], pair[1], thin, rgba(colors.path));
    }
    // The dock, then the vacuum on top (when it's docked, both show).
    // (Its heading isn't drawn: which way Roborock's angle turns isn't
    // certain -- better no arrow than a wrong one.)
    if let Some(p) = point(&map["dock"]) {
        dot(pixels, p, scale as f32 * 3.0, rgba(colors.dock));
    }
    if let Some(p) = point(&map["robot"]) {
        // A ring of the floor colour around it: visible on any background.
        dot(pixels, p, scale as f32 * 4.2, rgba(colors.floor));
        dot(pixels, p, scale as f32 * 3.4, rgba(colors.robot));
    }
    Some(Image::from_rgba8_premultiplied(buffer))
}

/// A camera's picture (issue #43): JPEG (base64) -> an image.
pub fn picture(jpeg_base64: &str) -> Option<Image> {
    let jpeg = base64::engine::general_purpose::STANDARD.decode(jpeg_base64).ok()?;
    let options = zune_jpeg::zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(&jpeg), options);
    let pixels = decoder.decode().ok()?;
    let (w, h) = decoder.dimensions()?;
    if pixels.len() != w * h * 4 {
        return None;
    }
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&pixels, w as u32, h as u32);
    Some(Image::from_rgba8(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    /* A 16x8 JPEG made by ffmpeg's mjpeg encoder, as camera.rs sends
     * them: decoded to an image of that size; garbage isn't. */
    #[test]
    fn camera_pictures_are_decoded() {
        const TINY: &str = "/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYyLjExLjEwMAD/2wBDAAgMDA4MDhAQEBAQEBMSExQUFBMTExMUFBQVFRUZGRkVFRUUFBUVGBgZGRscGxoaGRocHB4eHiQkIiIqKiszMz7/xABuAAEBAQAAAAAAAAAAAAAAAAAHAgYBAQEBAQAAAAAAAAAAAAAAAAUHAwYQAAEDBAMBAQEAAAAAAAAAAAECAwQSEQUAISI1MXJBEQACAQMDBAIDAQAAAAAAAAABAgMEEQUAEyESgjE1MwZDIkEV/8AAEQgACAAQAwESAAISAAMSAP/aAAwDAQACEQMRAD8Ay8fEyzhm0hkVrWu3Zu5TUgp5q/XB+akxfOi6ngs1jaaolSWYLIzj8cpJG3YcqhHm/wDdSGn9n3DSOQqxjqaenq5GjvT06pH+zrurV1Dy2CdSg7bREtx1cC5toH718vdqFZOK3nWnC6QhoOB00ucBTNIBATdXcfwH5fT+V6MrevxODydFFuzQGOMg3O7Ebjk8hZCfNj41Zqj1nadKYaNMh/rPEqy7xonpiQAbxqyuy9dihCsVuekkMQNa/Rfi7df/2Q==";
        let image = picture(TINY).unwrap();
        assert_eq!((image.size().width, image.size().height), (16, 8));
        assert!(picture("bm90IGEganBlZw==").is_none());
    }

    #[test]
    fn runs_are_unpacked_exactly() {
        let text = base64::engine::general_purpose::STANDARD.encode([0u8, 3, 2, 1]);
        assert_eq!(unpack(&text, 4).unwrap(), vec![0, 0, 0, 2]);
        // Too few or too many squares: not this map.
        assert!(unpack(&text, 5).is_none());
        assert!(unpack(&text, 3).is_none());
    }
}
