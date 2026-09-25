// theme.rs -- applies a design preset to the UI (issue #39).
//
// The design tokens live in ui/tokens.json, compiled into the program
// (include_str!), so there is no file to find or lose at run time. A
// PRESET is three choices, made on the Appearance page and saved by
// backend_daemon (settings.rs there):
//   - mode:    "dark", "light", or "auto" (light between 07:00 and 19:00 in
//              the hub's time zone -- tokens.json "auto");
//   - accent:  one of tokens.json's accents ("sky", "emerald", ...);
//   - density: "comfortable" (finger-sized) or "compact".
// apply() writes the matching values into app.slint's `Theme` global.
// Every component reads its colors and sizes from there, so Slint redraws
// the whole UI in the new look -- no restart.
//
// Shared with tools/ui_preview (it includes this file), so the preview
// renders every preset exactly as the hub does.

use serde::Deserialize;
use slint::{Color, ComponentHandle};
use std::sync::OnceLock;

/// The chosen preset, as backend_daemon reports it. Unknown values (a
/// newer app, a typo) fall back to tokens.json's defaults when applied.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Appearance {
    pub mode: String,
    pub accent: String,
    pub density: String,
}

impl Default for Appearance {
    fn default() -> Self {
        let d = &tokens().defaults;
        Appearance {
            mode: d.mode.clone(),
            accent: d.accent.clone(),
            density: d.density.clone(),
        }
    }
}

/// Writes the preset into the UI's Theme. `dark`: which palette, already
/// decided (see is_dark -- "auto" depends on the time).
pub fn apply(ui: &crate::AppWindow, appearance: &Appearance, dark: bool) {
    let t = tokens();
    let theme = ui.global::<crate::Theme>();
    let palette = if dark { &t.modes.dark } else { &t.modes.light };
    let accent = accent(&appearance.accent);
    let density = if appearance.density == "compact" { &t.density.compact } else { &t.density.comfortable };

    theme.set_dark(dark);
    theme.set_background(color(&palette.background));
    theme.set_surface(color(&palette.surface));
    theme.set_surface_pressed(color(&palette.surface_pressed));
    theme.set_outline(color(&palette.outline));
    theme.set_text(color(&palette.text));
    theme.set_text_muted(color(&palette.text_muted));
    theme.set_knob(color(&palette.knob));
    theme.set_error(color(&palette.error));
    theme.set_ok(color(&palette.ok));
    theme.set_accent(color(if dark { &accent.dark } else { &accent.light }));
    theme.set_on_accent(color(if dark { &accent.on_accent_dark } else { &accent.on_accent_light }));

    let ty = &t.typography;
    theme.set_font_caption(ty.caption);
    theme.set_font_body(ty.body);
    theme.set_font_subtitle(ty.subtitle);
    theme.set_font_heading(ty.heading);
    theme.set_font_title(ty.title);
    theme.set_font_display(ty.display);

    theme.set_radius_small(t.radii.small);
    theme.set_radius_medium(t.radii.medium);

    theme.set_touch_height(density.touch_height);
    theme.set_space_xxs(density.space_xxs);
    theme.set_space_xs(density.space_xs);
    theme.set_space_s(density.space_s);
    theme.set_space_m(density.space_m);
    theme.set_space_l(density.space_l);
    theme.set_space_xl(density.space_xl);

    theme.set_content_max_width(t.layout.content_max_width);
}

/// Whether the dark palette is on, for a mode and the current hour in the
/// hub's time zone (0-23). "auto": light from tokens.json's light-from-hour
/// until dark-from-hour. Anything unknown counts as the default mode.
pub fn is_dark(mode: &str, local_hour: u32) -> bool {
    let t = tokens();
    match mode {
        "dark" => true,
        "light" => false,
        "auto" => !(t.auto.light_from_hour..t.auto.dark_from_hour).contains(&local_hour),
        _ => t.defaults.mode != "light",
    }
}

/// The accents in their order, as (id, name, color in the given mode): for
/// the swatches on the Appearance page.
pub fn accents(dark: bool) -> Vec<(String, String, Color)> {
    tokens()
        .accents
        .iter()
        .map(|a| (a.id.clone(), a.name.clone(), color(if dark { &a.dark } else { &a.light })))
        .collect()
}

fn accent(id: &str) -> &'static Accent {
    let t = tokens();
    t.accents
        .iter()
        .find(|a| a.id == id)
        .or_else(|| t.accents.iter().find(|a| a.id == t.defaults.accent))
        .unwrap_or(&t.accents[0])
}

/// "#RRGGBB" -> a Slint color. tokens.json is ours and compiled in, so a
/// malformed value is a bug caught by the tests below, not a run-time case;
/// it shows as black rather than crashing the UI.
fn color(hex: &str) -> Color {
    let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    Color::from_rgb_u8((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

// ---- tokens.json, as Rust types --------------------------------------------
// Only what the UI uses; `$comment` keys and "splash" (the boot image's)
// are ignored. kebab-case: the JSON's "surface-pressed" is surface_pressed.

#[derive(Deserialize)]
struct Tokens {
    defaults: Defaults,
    modes: Modes,
    accents: Vec<Accent>,
    typography: Typography,
    radii: Radii,
    density: Densities,
    layout: Layout,
    auto: Auto,
}

#[derive(Deserialize)]
struct Defaults {
    mode: String,
    accent: String,
    density: String,
}

#[derive(Deserialize)]
struct Modes {
    dark: Palette,
    light: Palette,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Palette {
    background: String,
    surface: String,
    surface_pressed: String,
    outline: String,
    text: String,
    text_muted: String,
    knob: String,
    error: String,
    ok: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Accent {
    id: String,
    name: String,
    dark: String,
    light: String,
    on_accent_dark: String,
    on_accent_light: String,
}

#[derive(Deserialize)]
struct Typography {
    caption: f32,
    body: f32,
    subtitle: f32,
    heading: f32,
    title: f32,
    display: f32,
}

#[derive(Deserialize)]
struct Radii {
    small: f32,
    medium: f32,
}

#[derive(Deserialize)]
struct Densities {
    comfortable: Density,
    compact: Density,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Density {
    touch_height: f32,
    space_xxs: f32,
    space_xs: f32,
    space_s: f32,
    space_m: f32,
    space_l: f32,
    space_xl: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Layout {
    content_max_width: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Auto {
    light_from_hour: u32,
    dark_from_hour: u32,
}

/// tokens.json, parsed once, the first time it's needed. A broken file is
/// a build-time mistake (the tests catch it), so failing loudly is right.
fn tokens() -> &'static Tokens {
    static TOKENS: OnceLock<Tokens> = OnceLock::new();
    TOKENS.get_or_init(|| {
        serde_json::from_str(include_str!("../ui/tokens.json")).expect("ui/tokens.json doesn't match theme.rs")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_parse_and_every_color_is_valid() {
        let t = tokens();
        let hex = |s: &str| s.len() == 7 && s.starts_with('#') && u32::from_str_radix(&s[1..], 16).is_ok();
        for p in [&t.modes.dark, &t.modes.light] {
            for c in [&p.background, &p.surface, &p.surface_pressed, &p.outline, &p.text, &p.text_muted, &p.knob, &p.error, &p.ok] {
                assert!(hex(c), "bad color {c}");
            }
        }
        for a in &t.accents {
            for c in [&a.dark, &a.light, &a.on_accent_dark, &a.on_accent_light] {
                assert!(hex(c), "bad color {c} in accent {}", a.id);
            }
        }
        assert!(t.accents.iter().any(|a| a.id == t.defaults.accent));
    }

    #[test]
    fn auto_is_light_by_day() {
        assert!(is_dark("auto", 6));
        assert!(!is_dark("auto", 7));
        assert!(!is_dark("auto", 18));
        assert!(is_dark("auto", 19));
        assert!(is_dark("dark", 12));
        assert!(!is_dark("light", 0));
    }
}
