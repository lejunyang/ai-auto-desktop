//! The keys a viewer presses, named by the physical key (the DOM `code`), so
//! shortcuts work whatever the viewer's keyboard layout: Ctrl+/ is the key
//! right of the full stop on a US layout, wherever the `/` character is.
//! Typed characters arrive as text instead and need no table.

/// One key as Windows and Chrome know it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    /// Windows virtual-key code; left and right modifiers kept apart.
    pub vk: u16,
    /// The DOM `code`.
    pub code: &'static str,
    /// Sent with the extended-key flag: the arrows and their block, the
    /// right-hand modifiers, numpad Enter and divide.
    pub extended: bool,
}

impl Key {
    /// The virtual-key code Chrome expects: modifiers without their side.
    pub fn chrome_vk(self) -> u16 {
        match self.vk {
            0xA0 | 0xA1 => 0x10,
            0xA2 | 0xA3 => 0x11,
            0xA4 | 0xA5 => 0x12,
            vk => vk,
        }
    }

    /// DOM `location`: 1 left, 2 right, 3 numpad.
    pub fn location(self) -> u8 {
        match self.code {
            c if c.ends_with("Left") && !c.starts_with("Arrow") && !c.starts_with("Bracket") => 1,
            c if c.ends_with("Right") && !c.starts_with("Arrow") && !c.starts_with("Bracket") => 2,
            c if c.starts_with("Numpad") => 3,
            _ => 0,
        }
    }

    /// The modifier bit Chrome uses for this key, if it is one.
    pub fn modifier(self) -> u32 {
        match self.code {
            "AltLeft" | "AltRight" => 1,
            "ControlLeft" | "ControlRight" => 2,
            "MetaLeft" | "MetaRight" => 4,
            "ShiftLeft" | "ShiftRight" => 8,
            _ => 0,
        }
    }
}

/// Every key but letters and digits: (code, virtual key, extended).
const TABLE: &[(&str, u16, bool)] = &[
    ("Escape", 0x1B, false),
    ("Backquote", 0xC0, false),
    ("Minus", 0xBD, false),
    ("Equal", 0xBB, false),
    ("Backspace", 0x08, false),
    ("Tab", 0x09, false),
    ("BracketLeft", 0xDB, false),
    ("BracketRight", 0xDD, false),
    ("Backslash", 0xDC, false),
    ("CapsLock", 0x14, false),
    ("Semicolon", 0xBA, false),
    ("Quote", 0xDE, false),
    ("Enter", 0x0D, false),
    ("ShiftLeft", 0xA0, false),
    ("ShiftRight", 0xA1, false),
    ("IntlBackslash", 0xE2, false),
    ("Comma", 0xBC, false),
    ("Period", 0xBE, false),
    ("Slash", 0xBF, false),
    ("ControlLeft", 0xA2, false),
    ("ControlRight", 0xA3, true),
    ("MetaLeft", 0x5B, true),
    ("MetaRight", 0x5C, true),
    ("AltLeft", 0xA4, false),
    ("AltRight", 0xA5, true),
    ("Space", 0x20, false),
    ("ContextMenu", 0x5D, true),
    ("PrintScreen", 0x2C, true),
    ("ScrollLock", 0x91, false),
    ("Pause", 0x13, false),
    ("Insert", 0x2D, true),
    ("Home", 0x24, true),
    ("PageUp", 0x21, true),
    ("Delete", 0x2E, true),
    ("End", 0x23, true),
    ("PageDown", 0x22, true),
    ("ArrowUp", 0x26, true),
    ("ArrowLeft", 0x25, true),
    ("ArrowDown", 0x28, true),
    ("ArrowRight", 0x27, true),
    ("NumLock", 0x90, true),
    ("NumpadDivide", 0x6F, true),
    ("NumpadMultiply", 0x6A, false),
    ("NumpadSubtract", 0x6D, false),
    ("NumpadAdd", 0x6B, false),
    ("NumpadEnter", 0x0D, true),
    ("NumpadDecimal", 0x6E, false),
];

const LETTERS: [&str; 26] = [
    "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK", "KeyL",
    "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV", "KeyW", "KeyX",
    "KeyY", "KeyZ",
];
const DIGITS: [&str; 10] = [
    "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
    "Digit9",
];
const NUMPAD: [&str; 10] = [
    "Numpad0", "Numpad1", "Numpad2", "Numpad3", "Numpad4", "Numpad5", "Numpad6", "Numpad7",
    "Numpad8", "Numpad9",
];
const FUNCTION: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

/// The key with this DOM `code`.
pub fn by_code(code: &str) -> Option<Key> {
    let key = |vk: u16, code: &'static str, extended: bool| Some(Key { vk, code, extended });
    if let Some(i) = LETTERS.iter().position(|c| *c == code) {
        return key(0x41 + i as u16, LETTERS[i], false);
    }
    if let Some(i) = DIGITS.iter().position(|c| *c == code) {
        return key(0x30 + i as u16, DIGITS[i], false);
    }
    if let Some(i) = NUMPAD.iter().position(|c| *c == code) {
        return key(0x60 + i as u16, NUMPAD[i], false);
    }
    if let Some(i) = FUNCTION.iter().position(|c| *c == code) {
        return key(0x70 + i as u16, FUNCTION[i], false);
    }
    TABLE
        .iter()
        .find(|(c, _, _)| *c == code)
        .and_then(|(c, vk, extended)| key(*vk, c, *extended))
}

/// The key a DOM `key` value most likely came from, for a viewer that sends
/// no `code`: named keys, and letters, digits and US punctuation.
pub fn by_name(name: &str) -> Option<Key> {
    let code = match name {
        "Shift" => "ShiftLeft",
        "Control" => "ControlLeft",
        "Alt" => "AltLeft",
        "Meta" | "OS" => "MetaLeft",
        " " | "Spacebar" => "Space",
        "Esc" => "Escape",
        "Left" => "ArrowLeft",
        "Right" => "ArrowRight",
        "Up" => "ArrowUp",
        "Down" => "ArrowDown",
        "Del" => "Delete",
        "Apps" => "ContextMenu",
        "`" | "~" => "Backquote",
        "-" | "_" => "Minus",
        "=" | "+" => "Equal",
        "[" | "{" => "BracketLeft",
        "]" | "}" => "BracketRight",
        "\\" | "|" => "Backslash",
        ";" | ":" => "Semicolon",
        "'" | "\"" => "Quote",
        "," | "<" => "Comma",
        "." | ">" => "Period",
        "/" | "?" => "Slash",
        one if one.len() == 1 && one.as_bytes()[0].is_ascii_alphabetic() => {
            return by_code(LETTERS[(one.as_bytes()[0].to_ascii_uppercase() - b'A') as usize]);
        }
        one if one.len() == 1 && one.as_bytes()[0].is_ascii_digit() => {
            return by_code(DIGITS[(one.as_bytes()[0] - b'0') as usize]);
        }
        other => other,
    };
    by_code(code)
}

/// The key a viewer's `key` message means: its `code` when known, else its `key`.
pub fn of(event: &serde_json::Value) -> Option<Key> {
    event["code"]
        .as_str()
        .and_then(by_code)
        .or_else(|| event["key"].as_str().and_then(by_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn physical_keys_map_to_their_virtual_keys() {
        assert_eq!(by_code("KeyA").map(|k| k.vk), Some(0x41));
        assert_eq!(by_code("Digit7").map(|k| k.vk), Some(0x37));
        assert_eq!(by_code("F24").map(|k| k.vk), Some(0x87));
        assert_eq!(by_code("Slash").map(|k| k.vk), Some(0xBF));
        assert_eq!(by_code("Numpad5").map(|k| k.vk), Some(0x65));
        let arrow = by_code("ArrowLeft").unwrap();
        assert!(arrow.extended);
        assert_eq!(arrow.location(), 0);
        let control = by_code("ControlRight").unwrap();
        assert!(control.extended);
        assert_eq!(
            (control.chrome_vk(), control.location(), control.modifier()),
            (0x11, 2, 2)
        );
        assert_eq!(
            by_code("NumpadEnter").map(|k| (k.vk, k.extended, k.location())),
            Some((0x0D, true, 3))
        );
        assert_eq!(by_code("BracketLeft").unwrap().location(), 0);
        assert!(by_code("Hyper").is_none());
    }

    #[test]
    fn a_key_without_its_code_is_found_by_name() {
        assert_eq!(by_name(" ").map(|k| k.code), Some("Space"));
        assert_eq!(by_name("q").map(|k| k.code), Some("KeyQ"));
        assert_eq!(by_name("?").map(|k| k.code), Some("Slash"));
        assert_eq!(by_name("Control").map(|k| k.code), Some("ControlLeft"));
        assert_eq!(by_name("F5").map(|k| k.vk), Some(0x74));
        // The code wins over the key: a layout where Q types "a".
        assert_eq!(
            of(&json!({"key": "a", "code": "KeyQ"})).map(|k| k.vk),
            Some(0x51)
        );
        assert_eq!(of(&json!({"key": "Process"})), None);
    }
}
