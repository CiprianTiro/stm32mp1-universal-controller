// Compiles the hub UI's own app.slint -- the same file, the same options as
// linux_a7/ui_layer/build.rs (glyphs embedded for the software renderer),
// so the preview draws exactly what the board draws.
// The same text sizes as the UI's build.rs (see its comment): from
// tokens.json, so the preview's text matches the board's.
const APP: &str = "../../linux_a7/ui_layer/ui/app.slint";
const TOKENS: &str = "../../linux_a7/ui_layer/ui/tokens.json";

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
