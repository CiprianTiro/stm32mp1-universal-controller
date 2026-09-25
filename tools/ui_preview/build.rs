// Compiles the hub UI's own app.slint -- the same file, the same options as
// linux_a7/ui_layer/build.rs (glyphs embedded for the software renderer),
// so the preview draws exactly what the board draws.
fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("../../linux_a7/ui_layer/ui/app.slint", config)
        .expect("failed to compile app.slint");
}
