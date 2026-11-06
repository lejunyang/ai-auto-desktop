//! What Linux offers ai-auto-desktop: the X11 screen with what changed
//! (DAMAGE), input through XTest, the pointer (XFixes), displays (RandR),
//! windows and the clipboard. Built on Linux only; elsewhere this crate is
//! empty.
#![cfg(target_os = "linux")]

mod desktop;
pub mod probe;
pub mod wayland_portal;
mod x11;

pub use desktop::{open, screens};

type Result<T> = std::result::Result<T, String>;
