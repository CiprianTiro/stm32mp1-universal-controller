// build.rs runs before the rest of this crate compiles. Its one job here is
// to hand `ui/app.slint` to Slint's own compiler (`slint_build`), which
// turns that markup file into generated Rust code -- the `AppWindow` type
// (and its `set_*`/`get_*`/callback methods) that main.rs uses via the
// `slint::include_modules!()` macro doesn't exist anywhere as hand-written
// Rust; it's produced fresh from app.slint on every build.
//
// `EmbedForSoftwareRenderer`: tells the Slint compiler to bake the actual
// font glyph bitmaps into the compiled binary at build time, rather than
// looking up a system font at runtime via fontconfig. Two reasons this
// matters for this project specifically: (1) fontconfig requires a
// fontconfig.conf and a font cache that don't exist on this board's minimal
// rootfs, and (2) the software renderer (chosen because this board has no
// confirmed-working GPU driver -- see Cargo.toml's comment) needs the font
// pre-rasterized into the exact pixel format it renders with, which system
// font lookup doesn't provide anyway.
const APP: &str = "ui/app.slint";
const TOKENS: &str = "ui/tokens.json";

fn main() {
    // The text sizes to pre-render (issue #39). The compiler embeds glyphs
    // only at the font sizes it finds written as numbers in the .slint
    // files -- but since #39 every size is a design token (Theme's
    // font-* properties, set at run time), so it would find almost none,
    // and Slint then drew all text with the nearest (smaller) glyphs it
    // had (seen in the preview: every label shrank). SLINT_FONT_SIZES is
    // the compiler's own way to add sizes; they come straight from
    // tokens.json's "typography", so the sizes stay defined in one place.
    let tokens: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(TOKENS).expect("can't read ui/tokens.json"),
    )
    .expect("ui/tokens.json is not valid JSON");
    let sizes: Vec<String> = tokens["typography"]
        .as_object()
        .expect("tokens.json has no \"typography\"")
        .values()
        .map(|size| size.as_f64().expect("a typography size is not a number").to_string())
        .collect();
    std::env::set_var("SLINT_FONT_SIZES", sizes.join(","));
    println!("cargo:rerun-if-changed={TOKENS}");

    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config(APP, config).expect("failed to compile app.slint");
}
