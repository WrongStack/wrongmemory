//! Windows gives the main thread a 1 MiB stack, while Linux and macOS give
//! 8 MiB. GPUI builds whole element trees on the main thread, and unoptimized
//! builds keep every intermediate element in one frame, so match the Unix size.
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
