//! What Windows offers ai-auto-desktop: the screen as pixels with what
//! changed (DXGI desktop duplication, GDI where it is missing) and a window's
//! own rendering, input through SendInput, the pointer, the clipboard, text
//! recognition, and the probe's checks. Built on Windows only; elsewhere
//! this crate is empty.
#![cfg(windows)]

pub mod capture;
pub mod clipboard;
pub mod com;
mod desktop;
pub mod input;
pub mod ocr;
pub mod probe;
mod screen;

pub use desktop::{open, screens};
pub use screen::dpi_aware;

type Result<T> = std::result::Result<T, String>;
