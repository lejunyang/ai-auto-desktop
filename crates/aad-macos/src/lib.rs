//! What macOS offers ai-auto-desktop: the screen through ScreenCaptureKit
//! (CoreGraphics before 12.3), input as CGEvents, the pointer's position and
//! the clipboard. Built on macOS only; elsewhere this crate is empty.
#![cfg(target_os = "macos")]

mod desktop;
mod macos;
mod sck;

pub use desktop::{open, screens};

type Result<T> = std::result::Result<T, String>;
