//! What every operating system offers ai-auto-desktop, in one vocabulary.
//!
//! The platform crates (`aad-windows`, `aad-linux`, `aad-macos`) are each
//! built only on their own system and implement what is declared here; the
//! rest of the workspace depends on this crate and on the one platform crate
//! of the system it is built for. Nothing here touches the system itself, so
//! it builds and is tested everywhere.
//!
//! - [`Desktop`]: a shared screen and the input passed to it (`aad assist`),
//!   with [`Raw`] pictures, what changed in them ([`damage`]), the pictures
//!   kept between captures ([`buffer`]) and the keys a viewer presses
//!   ([`keys`]).
//! - [`Error`]: a structured failure the callers map to their own errors.
use serde_json::{Map, Value};
use std::sync::Arc;

pub mod buffer;
pub mod damage;
pub mod keys;

/// How pixels are laid out in a [`Raw`] picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Rgba,
}

impl PixelFormat {
    /// The layout's usual lower-case name, as ffmpeg's `-pix_fmt` takes it.
    pub fn name(self) -> &'static str {
        match self {
            PixelFormat::Bgra => "bgra",
            PixelFormat::Rgba => "rgba",
        }
    }
}

/// Uncompressed pixels, four bytes each, top row first.
#[derive(Clone)]
pub struct Raw {
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// What changed since the source's previous picture, when the source
    /// knows; the pictures are compared otherwise.
    pub damage: Option<damage::Damage>,
}

/// What part of the desktop is shared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Area {
    All,
    Monitor(usize),
    /// A window by its handle (as listed) or a part of its title.
    Window(String),
}

impl std::fmt::Display for Area {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Area::All => write!(f, "the desktop"),
            Area::Monitor(index) => write!(f, "display {index}"),
            Area::Window(window) => write!(f, "window {window}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseAction {
    Move,
    Down,
    Up,
    /// Scrolled by this many pixels, as a browser reports a wheel.
    Wheel {
        dx: f64,
        dy: f64,
    },
}

/// Input from the person watching, in the shared part's terms.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// `x` and `y` from 0 to 1 across the shared part; `count` makes double
    /// and triple clicks.
    Mouse {
        action: MouseAction,
        button: Button,
        x: f64,
        y: f64,
        count: i64,
    },
    Key {
        key: keys::Key,
        down: bool,
    },
    Text(String),
}

/// A shared screen and the input passed to it.
pub trait Desktop {
    /// How the screen is read, for the `source` a viewer is told about.
    fn name(&self) -> Map<String, Value>;
    /// The picture when it changed, or when a repeat was asked for.
    fn capture(&mut self) -> Result<Option<Raw>, String>;
    /// The next capture repeats the picture even when nothing changed.
    fn refresh(&mut self);
    /// Where the pointer is (and, when `fresh` or changed, its shape), as
    /// the `cursor` message sends it.
    fn cursor(&mut self, fresh: bool) -> Option<Value>;
    fn input(&mut self, input: &Input) -> Result<(), String>;
    /// Lets go of every key and button this side holds down.
    fn release(&mut self);
    /// The clipboard's text, as the `clipboard` message sends it.
    fn clipboard(&mut self, max_characters: usize) -> Result<Value, String>;
}

/// A structured failure: a stable code, what happened, and whether trying
/// again may help.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}
