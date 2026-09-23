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
fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile app.slint");
}
