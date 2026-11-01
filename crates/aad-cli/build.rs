fn main() {
    // ScreenCaptureKit is used only where it exists (macOS 12.3 and later).
    // Linked weakly, aad still starts on an older macOS and reads the display
    // through CoreGraphics there.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
    }
}
