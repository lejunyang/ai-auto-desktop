//! The shared Windows desktop: pictures from [`Screen`], input through
//! SendInput, mapped from the shared part onto the whole virtual desktop.
use crate::screen::{self, Pointer, Screen};
use crate::Result;
use aad_platform::{keys, Area, Button, Desktop, Input, MouseAction, Raw};
use serde_json::{json, Map, Value};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    VIRTUAL_KEY,
};

struct Shared {
    screen: Screen,
    pointer: Pointer,
    /// Keys and mouse buttons (1 left, 2 right, 4 middle) held down.
    held: Vec<keys::Key>,
    buttons: u8,
}

/// The displays and windows that can be shared, as `--list-screens` prints.
pub fn screens() -> Result<Value> {
    Ok(screen::screens())
}

/// Shares `area` of the desktop, in physical pixels.
pub fn open(area: &Area) -> Result<Box<dyn Desktop>> {
    screen::dpi_aware();
    Ok(Box::new(Shared {
        screen: Screen::open(area)?,
        pointer: Pointer::default(),
        held: Vec::new(),
        buttons: 0,
    }))
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err("Windows refused the input (a higher-privileged window may be in front)".into())
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A physical key going down or up, with its scan code, which some
/// programs read instead of the virtual key.
fn press(key: keys::Key, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(u32::from(key.vk), MAPVK_VK_TO_VSC) } as u16;
    let mut flags = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    if key.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    self::key(key.vk, scan, flags)
}

fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

impl Desktop for Shared {
    fn name(&self) -> Map<String, Value> {
        let capture = if self.screen.duplicating() {
            "dxgi"
        } else {
            "gdi"
        };
        let mut name = Map::new();
        name.insert("capture".into(), json!(capture));
        if let Some(reason) = &self.screen.fallback {
            name.insert("fallback".into(), json!(reason));
        }
        name
    }

    fn capture(&mut self) -> Result<Option<Raw>> {
        self.screen.capture()
    }

    fn cursor(&mut self, fresh: bool) -> Option<Value> {
        self.pointer.message(fresh, self.screen.shown.0)
    }

    fn refresh(&mut self) {
        self.screen.refresh();
    }

    fn input(&mut self, input: &Input) -> Result<()> {
        match input {
            Input::Mouse {
                action,
                button,
                x,
                y,
                ..
            } => {
                // From the shared part to the whole desktop's 0..65535.
                let (at, desktop) = self.screen.shown;
                let across = |f: f64, start: i32, size: u32, origin: i32, whole: u32| {
                    let pixel = f64::from(start - origin) + f * f64::from(size.max(1) - 1);
                    (pixel / f64::from(whole.max(2) - 1) * 65535.0).round() as i32
                };
                let x = across(*x, at.0, at.2, desktop.0, desktop.2);
                let y = across(*y, at.1, at.3, desktop.1, desktop.3);
                let at = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                let (down, up, mask) = match button {
                    Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 2),
                    Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 4),
                    Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 1),
                };
                let mut inputs = vec![mouse(x, y, 0, at)];
                match action {
                    MouseAction::Down => {
                        self.buttons |= mask;
                        inputs.push(mouse(x, y, 0, at | down));
                    }
                    MouseAction::Up => {
                        self.buttons &= !mask;
                        inputs.push(mouse(x, y, 0, at | up));
                    }
                    MouseAction::Wheel { dx, dy } => {
                        // Browsers report about 100 pixels per notch; Windows 120 units.
                        let notch = |pixels: f64| (-pixels / 100.0 * 120.0).round() as i32;
                        if *dy != 0.0 {
                            inputs.push(mouse(0, 0, notch(*dy), MOUSEEVENTF_WHEEL));
                        }
                        if *dx != 0.0 {
                            inputs.push(mouse(0, 0, -notch(*dx), MOUSEEVENTF_HWHEEL));
                        }
                    }
                    MouseAction::Move => {}
                }
                send(&inputs)
            }
            Input::Key { key, down } => {
                self.held.retain(|held| held.code != key.code);
                if *down {
                    self.held.push(*key);
                }
                send(&[press(*key, !down)])
            }
            Input::Text(text) => {
                let inputs: Vec<INPUT> = text
                    .encode_utf16()
                    .flat_map(|unit| {
                        [
                            key(0, unit, KEYEVENTF_UNICODE),
                            key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                        ]
                    })
                    .collect();
                send(&inputs)
            }
        }
    }

    fn release(&mut self) {
        let mut inputs: Vec<INPUT> = std::mem::take(&mut self.held)
            .into_iter()
            .rev()
            .map(|key| press(key, true))
            .collect();
        for (mask, up) in [
            (1, MOUSEEVENTF_LEFTUP),
            (2, MOUSEEVENTF_RIGHTUP),
            (4, MOUSEEVENTF_MIDDLEUP),
        ] {
            if self.buttons & mask != 0 {
                inputs.push(mouse(0, 0, 0, up));
            }
        }
        self.buttons = 0;
        if !inputs.is_empty() {
            let _ = send(&inputs);
        }
    }

    fn clipboard(&mut self, max_characters: usize) -> Result<Value> {
        let (text, characters) =
            crate::clipboard::text(max_characters).map_err(|e| e.to_string())?;
        Ok(
            json!({"type": "clipboard", "text": text, "characters": characters,
                  "truncated": characters > max_characters}),
        )
    }
}
