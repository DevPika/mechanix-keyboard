// The old build script baked a glyph atlas + icon sprites at build time via
// `assets::builder`. The new mecha-wayland UI core loads fonts at runtime
// through `Atlas::add_font`, so there is nothing to generate here.

fn main() {}
