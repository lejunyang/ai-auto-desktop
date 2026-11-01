//! The shared macOS desktop: one display.
use crate::macos::Mac;
use crate::Result;
use aad_platform::{Area, Button, Desktop, Input, MouseAction, Raw};
use serde_json::{Map, Value};

/// The displays that can be shared, as `--list-screens` prints (windows are
/// not shared on macOS).
pub fn screens() -> Result<Value> {
    Ok(crate::macos::screens())
}

/// Shares a display of the macOS desktop.
pub fn open(area: &Area) -> Result<Box<dyn Desktop>> {
    Ok(Box::new(Shared(Mac::open(area)?)))
}

struct Shared(Mac);

impl Desktop for Shared {
    fn name(&self) -> Map<String, Value> {
        let mut name = self.0.capture_name();
        name.insert("permissions".into(), self.0.permissions());
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
                count,
            } => {
                let button = match button {
                    Button::Left => 0,
                    Button::Right => 1,
                    Button::Middle => 2,
                };
                match action {
                    MouseAction::Wheel { dx, dy } => self.0.wheel(*dx, *dy),
                    MouseAction::Down => self.0.mouse("down", button, *x, *y, *count),
                    MouseAction::Up => self.0.mouse("up", button, *x, *y, *count),
                    MouseAction::Move => self.0.mouse("move", button, *x, *y, *count),
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
