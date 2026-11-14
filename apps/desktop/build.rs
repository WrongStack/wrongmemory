//! Platform link settings for the desktop executable.
//!
//! - Windows gives the main thread a 1 MiB stack, while Linux and macOS give
//!   8 MiB. GPUI builds whole element trees on the main thread, and
//!   unoptimized builds keep every intermediate element in one frame, so match
//!   the Unix size.
//! - macOS names a bare executable's menu-bar entry after the file
//!   (`wrongmemory-desktop`) unless the binary carries an Info.plist. Embed one
//!   in `__TEXT,__info_plist` so it is "WrongMemory" with or without the
//!   installed `WrongMemory.app`.
use std::path::PathBuf;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    if target_os == "macos" {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let plist = format!(
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
                "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
                "<plist version=\"1.0\">\n<dict>\n",
                "  <key>CFBundleIdentifier</key><string>com.wrongstack.WrongMemory</string>\n",
                "  <key>CFBundleName</key><string>WrongMemory</string>\n",
                "  <key>CFBundleDisplayName</key><string>WrongMemory</string>\n",
                "  <key>CFBundleShortVersionString</key><string>{v}</string>\n",
                "  <key>CFBundleVersion</key><string>{v}</string>\n",
                "  <key>NSHighResolutionCapable</key><true/>\n",
                "</dict>\n</plist>\n"
            ),
            v = version
        );
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("Info.plist");
        std::fs::write(&out, plist).expect("write embedded Info.plist");
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            out.display()
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
