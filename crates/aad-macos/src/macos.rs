//! The macOS desktop, through CoreGraphics and
//! ScreenCaptureKit.
//!
//! A display is captured through ScreenCaptureKit where there is one (macOS
//! 12.3 and later): it tells what changed in each frame, so only that is read
//! (see `sck`). Elsewhere, or when it cannot start, the display is captured
//! whole with CGDisplayCreateImage and compared with the last picture tile by
//! tile; tiles and video send only what changed either way. Input is posted
//! as CGEvents: mouse events carry their click count (macOS makes double
//! clicks from it), keys use the Mac virtual key of the physical key with the
//! held modifiers as flags, and text is typed as Unicode strings, whatever
//! the keyboard layout. The pointer's position comes from an empty event; its
//! image is not available without AppKit, so a standard arrow is drawn.
//! Capture needs the Screen Recording permission and input the Accessibility
//! one; `source` reports both.
use crate::sck::Stream;
use crate::Result;
use aad_platform::{keys, Area, PixelFormat, Raw};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

type CFTypeRef = *const c_void;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCreateImage(display: u32) -> CFTypeRef;
    fn CGDisplayCopyDisplayMode(display: u32) -> CFTypeRef;
    fn CGDisplayModeGetPixelWidth(mode: CFTypeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: CFTypeRef) -> usize;
    fn CGDisplayModeRelease(mode: CFTypeRef);
    fn CGImageGetWidth(image: CFTypeRef) -> usize;
    fn CGImageGetHeight(image: CFTypeRef) -> usize;
    fn CGImageGetBytesPerRow(image: CFTypeRef) -> usize;
    fn CGImageGetBitsPerPixel(image: CFTypeRef) -> usize;
    fn CGImageGetBitmapInfo(image: CFTypeRef) -> u32;
    fn CGImageGetDataProvider(image: CFTypeRef) -> CFTypeRef;
    fn CGDataProviderCopyData(provider: CFTypeRef) -> CFTypeRef;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGEventCreate(source: CFTypeRef) -> CFTypeRef;
    fn CGEventGetLocation(event: CFTypeRef) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: CFTypeRef,
        kind: u32,
        point: CGPoint,
        button: u32,
    ) -> CFTypeRef;
    fn CGEventCreateKeyboardEvent(source: CFTypeRef, key: u16, down: bool) -> CFTypeRef;
    fn CGEventCreateScrollWheelEvent(
        source: CFTypeRef,
        units: u32,
        count: u32,
        wheel1: i32,
        ...
    ) -> CFTypeRef;
    fn CGEventKeyboardSetUnicodeString(event: CFTypeRef, length: usize, text: *const u16);
    fn CGEventSetFlags(event: CFTypeRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CFTypeRef, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: CFTypeRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: CFTypeRef);
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

const HID_EVENT_TAP: u32 = 0;
const CLICK_STATE: u32 = 1;
const SCROLL_PIXELS: u32 = 0;
const BYTE_ORDER_MASK: u32 = 0x7000;
const BYTE_ORDER_32_LITTLE: u32 = 0x2000;

/// The Mac virtual key of a physical key (ANSI layout positions).
fn mac_key(code: &str) -> Option<u16> {
    Some(match code {
        "KeyA" => 0x00,
        "KeyS" => 0x01,
        "KeyD" => 0x02,
        "KeyF" => 0x03,
        "KeyH" => 0x04,
        "KeyG" => 0x05,
        "KeyZ" => 0x06,
        "KeyX" => 0x07,
        "KeyC" => 0x08,
        "KeyV" => 0x09,
        "IntlBackslash" => 0x0A,
        "KeyB" => 0x0B,
        "KeyQ" => 0x0C,
        "KeyW" => 0x0D,
        "KeyE" => 0x0E,
        "KeyR" => 0x0F,
        "KeyY" => 0x10,
        "KeyT" => 0x11,
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit6" => 0x16,
        "Digit5" => 0x17,
        "Equal" => 0x18,
        "Digit9" => 0x19,
        "Digit7" => 0x1A,
        "Minus" => 0x1B,
        "Digit8" => 0x1C,
        "Digit0" => 0x1D,
        "BracketRight" => 0x1E,
        "KeyO" => 0x1F,
        "KeyU" => 0x20,
        "BracketLeft" => 0x21,
        "KeyI" => 0x22,
        "KeyP" => 0x23,
        "Enter" => 0x24,
        "KeyL" => 0x25,
        "KeyJ" => 0x26,
        "Quote" => 0x27,
        "KeyK" => 0x28,
        "Semicolon" => 0x29,
        "Backslash" => 0x2A,
        "Comma" => 0x2B,
        "Slash" => 0x2C,
        "KeyN" => 0x2D,
        "KeyM" => 0x2E,
        "Period" => 0x2F,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backquote" => 0x32,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "MetaRight" => 0x36,
        "MetaLeft" => 0x37,
        "ShiftLeft" => 0x38,
        "CapsLock" => 0x39,
        "AltLeft" => 0x3A,
        "ControlLeft" => 0x3B,
        "ShiftRight" => 0x3C,
        "AltRight" => 0x3D,
        "ControlRight" => 0x3E,
        "F17" => 0x40,
        "NumpadDecimal" => 0x41,
        "NumpadMultiply" => 0x43,
        "NumpadAdd" => 0x45,
        "NumLock" => 0x47,
        "NumpadDivide" => 0x4B,
        "NumpadEnter" => 0x4C,
        "NumpadSubtract" => 0x4E,
        "F18" => 0x4F,
        "F19" => 0x50,
        "Numpad0" => 0x52,
        "Numpad1" => 0x53,
        "Numpad2" => 0x54,
        "Numpad3" => 0x55,
        "Numpad4" => 0x56,
        "Numpad5" => 0x57,
        "Numpad6" => 0x58,
        "Numpad7" => 0x59,
        "F20" => 0x5A,
        "Numpad8" => 0x5B,
        "Numpad9" => 0x5C,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F3" => 0x63,
        "F8" => 0x64,
        "F9" => 0x65,
        "F11" => 0x67,
        "F13" => 0x69,
        "F16" => 0x6A,
        "F14" => 0x6B,
        "F10" => 0x6D,
        "ContextMenu" => 0x6E,
        "F12" => 0x6F,
        "F15" => 0x71,
        "Insert" => 0x72,
        "Home" => 0x73,
        "PageUp" => 0x74,
        "Delete" => 0x75,
        "F4" => 0x76,
        "End" => 0x77,
        "F2" => 0x78,
        "PageDown" => 0x79,
        "F1" => 0x7A,
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "ArrowDown" => 0x7D,
        "ArrowUp" => 0x7E,
        _ => return None,
    })
}

/// The event flag a modifier key holds.
fn flag(code: &str) -> u64 {
    match code {
        "ShiftLeft" | "ShiftRight" => 0x0002_0000,
        "ControlLeft" | "ControlRight" => 0x0004_0000,
        "AltLeft" | "AltRight" => 0x0008_0000,
        "MetaLeft" | "MetaRight" => 0x0010_0000,
        _ => 0,
    }
}

/// A standard arrow, drawn: B black, W white, space clear.
const ARROW: [&str; 19] = [
    "B           ",
    "BB          ",
    "BWB         ",
    "BWWB        ",
    "BWWWB       ",
    "BWWWWB      ",
    "BWWWWWB     ",
    "BWWWWWWB    ",
    "BWWWWWWWB   ",
    "BWWWWWWWWB  ",
    "BWWWWWBBBBB ",
    "BWWBWWB     ",
    "BWB BWWB    ",
    "BB  BWWB    ",
    "B    BWWB   ",
    "     BWWB   ",
    "      BWWB  ",
    "      BWWB  ",
    "       BB   ",
];

fn arrow() -> Option<(Vec<u8>, u32, u32)> {
    let (width, height) = (ARROW[0].len() as u32, ARROW.len() as u32);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for row in ARROW {
        for pixel in row.bytes() {
            rgba.extend_from_slice(match pixel {
                b'B' => &[0, 0, 0, 255],
                b'W' => &[255, 255, 255, 255],
                _ => &[0, 0, 0, 0],
            });
        }
    }
    let image = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some((png, width, height))
}

fn displays() -> Vec<u32> {
    let mut list = [0u32; 16];
    let mut count = 0u32;
    unsafe { CGGetActiveDisplayList(16, list.as_mut_ptr(), &mut count) };
    list[..count as usize].to_vec()
}

/// The display's size in pixels (twice its points on most Retina displays).
fn pixel_size(display: u32) -> (u32, u32) {
    unsafe {
        let mode = CGDisplayCopyDisplayMode(display);
        if mode.is_null() {
            let b = CGDisplayBounds(display);
            return (b.size.width as u32, b.size.height as u32);
        }
        let size = (
            CGDisplayModeGetPixelWidth(mode) as u32,
            CGDisplayModeGetPixelHeight(mode) as u32,
        );
        CGDisplayModeRelease(mode);
        size
    }
}

/// What `--list-screens` prints: the displays (windows are not shared here).
pub fn screens() -> Value {
    let main = unsafe { CGMainDisplayID() };
    let monitors: Vec<Value> = displays()
        .into_iter()
        .enumerate()
        .map(|(index, display)| {
            let b = unsafe { CGDisplayBounds(display) };
            json!({"monitor": index, "display": display, "primary": display == main,
                   "left": b.origin.x, "top": b.origin.y, "width": b.size.width, "height": b.size.height,
                   "units": "points"})
        })
        .collect();
    json!({"monitors": monitors, "windows": []})
}

pub struct Mac {
    display: u32,
    /// The display's place on the desktop, in points, where events go.
    bounds: CGRect,
    /// Frames with what changed, where ScreenCaptureKit runs.
    stream: Option<Stream>,
    /// Why it does not, when it does not.
    no_stream: Option<String>,
    flags: u64,
    held_keys: Vec<(u16, String)>,
    held_buttons: Vec<u32>,
    cursor: Option<(i64, i64, bool)>,
    cursor_sent: Option<Instant>,
}

impl Mac {
    pub fn open(area: &Area) -> Result<Self> {
        let display = match area {
            Area::All => unsafe { CGMainDisplayID() },
            Area::Monitor(index) => {
                let all = displays();
                *all.get(*index).ok_or_else(|| {
                    format!(
                        "--monitor {index}: there are {} displays (see --list-screens)",
                        all.len()
                    )
                })?
            }
            Area::Window(window) => {
                return Err(format!(
                    "--window {window}: sharing one window is not available on macOS; use --monitor"
                ))
            }
        };
        let (stream, no_stream) = if !Stream::available() {
            (
                None,
                Some("ScreenCaptureKit needs macOS 12.3 or later".into()),
            )
        } else {
            let (width, height) = pixel_size(display);
            match Stream::open(display, width, height) {
                Ok(stream) => (Some(stream), None),
                Err(why) => (None, Some(why)),
            }
        };
        Ok(Self {
            display,
            bounds: unsafe { CGDisplayBounds(display) },
            stream,
            no_stream,
            flags: 0,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            cursor: None,
            cursor_sent: None,
        })
    }

    pub fn permissions(&self) -> Value {
        json!({"screen_recording": unsafe { CGPreflightScreenCaptureAccess() },
               "accessibility": unsafe { AXIsProcessTrusted() }})
    }

    /// How the display is captured, for `source`.
    pub fn capture_name(&self) -> serde_json::Map<String, Value> {
        let mut name = serde_json::Map::new();
        match &self.no_stream {
            None => {
                name.insert("capture".into(), json!("screencapturekit"));
            }
            Some(why) => {
                name.insert("capture".into(), json!("coregraphics"));
                name.insert("screencapturekit".into(), json!(why));
            }
        }
        name
    }

    /// The next capture repeats the picture even when nothing changed.
    pub fn refresh(&mut self) {
        if let Some(stream) = &mut self.stream {
            stream.refresh();
        }
    }

    /// The display as BGRA pixels when it changed (or a repeat was asked
    /// for). Should ScreenCaptureKit stop, the display is read whole from
    /// then on.
    pub fn capture(&mut self) -> Result<Option<Raw>> {
        self.bounds = unsafe { CGDisplayBounds(self.display) };
        if let Some(stream) = &mut self.stream {
            match stream.capture() {
                Ok(raw) => return Ok(raw),
                Err(why) => {
                    self.stream = None;
                    self.no_stream = Some(why);
                }
            }
        }
        self.capture_whole().map(Some)
    }

    /// The whole display as BGRA pixels, to be compared with the last.
    fn capture_whole(&mut self) -> Result<Raw> {
        unsafe {
            let image = CGDisplayCreateImage(self.display);
            if image.is_null() {
                return Err("the display could not be captured: give aad Screen Recording in System Settings, Privacy & Security".into());
            }
            let width = CGImageGetWidth(image);
            let height = CGImageGetHeight(image);
            let stride = CGImageGetBytesPerRow(image);
            let bits = CGImageGetBitsPerPixel(image);
            let order = CGImageGetBitmapInfo(image) & BYTE_ORDER_MASK;
            if bits != 32 || order != BYTE_ORDER_32_LITTLE {
                CFRelease(image);
                return Err(format!("the display's pixels are {bits}-bit in byte order {order:#x}; only 32-bit BGRA is shared"));
            }
            let data = CGDataProviderCopyData(CGImageGetDataProvider(image));
            if data.is_null() {
                CFRelease(image);
                return Err("the display's pixels could not be read".into());
            }
            let length = CFDataGetLength(data).max(0) as usize;
            let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), length);
            let mut pixels = Vec::with_capacity(width * height * 4);
            for row in 0..height {
                let start = row * stride;
                match bytes.get(start..start + width * 4) {
                    Some(line) => pixels.extend_from_slice(line),
                    None => break,
                }
            }
            CFRelease(data);
            CFRelease(image);
            if pixels.len() != width * height * 4 {
                return Err("the display's picture was cut short".into());
            }
            for alpha in pixels.iter_mut().skip(3).step_by(4) {
                *alpha = 255;
            }
            Ok(Raw {
                pixels: Arc::new(pixels),
                width: width as u32,
                height: height as u32,
                format: PixelFormat::Bgra,
                damage: None,
            })
        }
    }

    fn post(&self, event: CFTypeRef) -> Result<()> {
        if event.is_null() {
            return Err("macOS refused to make the input event".into());
        }
        unsafe {
            CGEventSetFlags(event, self.flags);
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event);
        }
        Ok(())
    }

    fn point(&self, fx: f64, fy: f64) -> CGPoint {
        CGPoint {
            x: self.bounds.origin.x + fx * self.bounds.size.width,
            y: self.bounds.origin.y + fy * self.bounds.size.height,
        }
    }

    /// Mouse buttons: 0 left, 1 right, 2 middle (other).
    pub fn mouse(&mut self, action: &str, button: u32, fx: f64, fy: f64, count: i64) -> Result<()> {
        let at = self.point(fx, fy);
        let (down, up, dragged) = match button {
            0 => (1, 2, 6),
            1 => (3, 4, 7),
            _ => (25, 26, 27),
        };
        let kind = match action {
            "down" => {
                self.held_buttons.retain(|b| *b != button);
                self.held_buttons.push(button);
                down
            }
            "up" => {
                self.held_buttons.retain(|b| *b != button);
                up
            }
            _ => match self.held_buttons.first() {
                Some(0) => 6,
                Some(1) => 7,
                Some(_) => dragged,
                None => 5,
            },
        };
        let event = unsafe { CGEventCreateMouseEvent(std::ptr::null(), kind, at, button) };
        if !event.is_null() && (action == "down" || action == "up") {
            unsafe { CGEventSetIntegerValueField(event, CLICK_STATE, count.clamp(1, 3)) };
        }
        self.post(event)
    }

    pub fn wheel(&self, dx: f64, dy: f64) -> Result<()> {
        // A page scrolls down for a positive delta; macOS wheels the other way.
        let event = unsafe {
            CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                SCROLL_PIXELS,
                2,
                -(dy.round() as i32),
                -(dx.round() as i32),
            )
        };
        self.post(event)
    }

    pub fn key(&mut self, key: keys::Key, down: bool) -> Result<()> {
        let code = mac_key(key.code).ok_or_else(|| format!("macOS has no key {}", key.code))?;
        let bit = flag(key.code);
        if down {
            self.flags |= bit;
            self.held_keys.retain(|(c, _)| *c != code);
            self.held_keys.push((code, key.code.to_string()));
        } else {
            self.flags &= !bit;
            self.held_keys.retain(|(c, _)| *c != code);
        }
        let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), code, down) };
        self.post(event)
    }

    pub fn text(&self, text: &str) -> Result<()> {
        let units: Vec<u16> = text.encode_utf16().collect();
        // macOS takes at most 20 UTF-16 units per event.
        for chunk in units.chunks(20) {
            for down in [true, false] {
                let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), 0, down) };
                if !event.is_null() {
                    unsafe { CGEventKeyboardSetUnicodeString(event, chunk.len(), chunk.as_ptr()) };
                }
                self.post(event)?;
            }
        }
        Ok(())
    }

    pub fn release(&mut self) {
        for (code, name) in std::mem::take(&mut self.held_keys).into_iter().rev() {
            self.flags &= !flag(&name);
            let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), code, false) };
            let _ = self.post(event);
        }
        self.flags = 0;
        let at = unsafe {
            let probe = CGEventCreate(std::ptr::null());
            let at = if probe.is_null() {
                CGPoint::default()
            } else {
                CGEventGetLocation(probe)
            };
            if !probe.is_null() {
                CFRelease(probe);
            }
            at
        };
        for button in std::mem::take(&mut self.held_buttons) {
            let up = match button {
                0 => 2,
                1 => 4,
                _ => 26,
            };
            let event = unsafe { CGEventCreateMouseEvent(std::ptr::null(), up, at, button) };
            let _ = self.post(event);
        }
    }

    /// A `cursor` message when the pointer moved, at most 60 a second;
    /// `fresh` sends the arrow's shape again.
    pub fn cursor(&mut self, fresh: bool) -> Option<Value> {
        if fresh {
            self.cursor = None;
            self.cursor_sent = None;
        }
        if self
            .cursor_sent
            .is_some_and(|at| at.elapsed() < Duration::from_millis(16))
        {
            return None;
        }
        let at = unsafe {
            let probe = CGEventCreate(std::ptr::null());
            if probe.is_null() {
                return None;
            }
            let at = CGEventGetLocation(probe);
            CFRelease(probe);
            at
        };
        let b = self.bounds;
        let (fx, fy) = (
            (at.x - b.origin.x) / b.size.width.max(1.0),
            (at.y - b.origin.y) / b.size.height.max(1.0),
        );
        let visible = (0.0..1.0).contains(&fx) && (0.0..1.0).contains(&fy);
        let state = ((fx * 10_000.0) as i64, (fy * 10_000.0) as i64, visible);
        let first = self.cursor.is_none();
        if self.cursor == Some(state) {
            return None;
        }
        self.cursor = Some(state);
        self.cursor_sent = Some(Instant::now());
        let mut message = json!({"type": "cursor", "visible": visible,
            "x": fx.clamp(0.0, 1.0), "y": fy.clamp(0.0, 1.0)});
        if first {
            if let Some((png, width, height)) = arrow() {
                use base64::Engine;
                // The arrow is in points; the screen it is measured against too.
                message["shape"] = json!({"png": base64::engine::general_purpose::STANDARD.encode(png),
                    "width": width, "height": height, "hotspot": [0, 0],
                    "screen": [b.size.width.round() as u32, b.size.height.round() as u32]});
            }
        }
        Some(message)
    }

    /// The pasteboard's text, as pbpaste gives it.
    pub fn clipboard(&self, max_characters: usize) -> Result<Value> {
        let output = std::process::Command::new("pbpaste")
            .output()
            .map_err(|e| format!("cannot run pbpaste: {e}"))?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let characters = text.chars().count();
        Ok(
            json!({"type": "clipboard", "text": (characters > 0).then(|| text.chars().take(max_characters).collect::<String>()),
                  "characters": characters, "truncated": characters > max_characters}),
        )
    }
}

impl Drop for Mac {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_physical_key_but_a_few_has_a_mac_key() {
        for code in [
            "KeyA",
            "KeyQ",
            "Digit5",
            "Slash",
            "ArrowLeft",
            "F1",
            "F12",
            "Numpad7",
            "ShiftLeft",
            "MetaLeft",
        ] {
            assert!(mac_key(code).is_some(), "{code}");
        }
        assert_eq!(mac_key("KeyA"), Some(0x00));
        assert_eq!(mac_key("Enter"), Some(0x24));
        assert_eq!(flag("MetaRight"), 0x0010_0000);
        let (png, width, height) = arrow().unwrap();
        assert_eq!((width, height), (12, 19));
        assert_eq!(&png[1..4], b"PNG");
    }
}
