fn main() {
    // The GUI's window/Space control (src/window_manager.rs) calls undocumented
    // CGS/SkyLight symbols to enumerate windows across Spaces and switch the
    // active Space. They live in the private SkyLight framework, which isn't on
    // the default framework search path — add it and link, but only when the
    // `gui` feature is built on macOS (the only place window_manager compiles).
    let is_macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    let gui = std::env::var("CARGO_FEATURE_GUI").is_ok();
    if is_macos && gui {
        println!("cargo:rustc-link-search=framework=/System/Library/PrivateFrameworks");
        println!("cargo:rustc-link-lib=framework=SkyLight");
    }
}
