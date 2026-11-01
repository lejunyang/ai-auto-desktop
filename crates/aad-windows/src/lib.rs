//! What Windows offers ai-auto-desktop: the screen as pixels with what
//! changed (DXGI desktop duplication, GDI where it is missing), input
//! through SendInput, the pointer, and the clipboard. Built on Windows only;
//! elsewhere this crate is empty.
#![cfg(windows)]

pub mod clipboard;
mod desktop;
mod screen;

pub use desktop::{open, screens};
pub use screen::dpi_aware;

type Result<T> = std::result::Result<T, String>;
