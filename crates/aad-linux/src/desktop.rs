//! The shared X11 desktop.
use crate::x11::X11;
use crate::Result;
use aad_platform::{Area, Button, Desktop, Input, MouseAction, Raw};
use serde_json::{json, Map, Value};

/// The displays and windows that can be shared, as `--list-screens` prints.
pub fn screens() -> Result<Value> {
    Ok(X11::open(&Area::All)?.screens())
}

/// Shares `area` of the X11 desktop in `DISPLAY`.
pub fn open(area: &Area) -> Result<Box<dyn Desktop>> {
    Ok(Box::new(Shared(X11::open(area)?)))
}

struct Shared(X11);

impl Desktop for Shared {
    fn name(&self) -> Map<String, Value> {
        let capture = if self.0.damage_known() {
            "x11-damage"
        } else {
            "x11"
        };
        let mut name = Map::new();
        name.insert("capture".into(), json!(capture));
        name
    }

    fn capture(&mut self) -> Result<Option<Raw>> {
        self.0.capture()
    }

    fn refresh(&mut self) {
        self.0.refresh();
    }

    fn cursor(&mut self, fresh: bool) -> Option<Value> {
        self.0.cursor(fresh)
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
                self.0.point(*x, *y)?;
                let button = match button {
                    Button::Right => 3,
                    Button::Middle => 2,
                    Button::Left => 1,
                };
                match action {
                    MouseAction::Down => self.0.button(button, true),
                    MouseAction::Up => self.0.button(button, false),
                    MouseAction::Wheel { dx, dy } => self.0.wheel(*dx, *dy),
                    MouseAction::Move => Ok(()),
                }
            }
            Input::Key { key, down } => self.0.key(*key, *down),
            Input::Text(text) => self.0.text(text),
        }
    }

    fn release(&mut self) {
        self.0.release();
    }

    fn clipboard(&mut self, max_characters: usize) -> Result<Value> {
        self.0.clipboard(max_characters)
    }
}
