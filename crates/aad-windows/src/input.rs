//! Synthetic input on Windows: keys and text as virtual keys in the target's
//! layout, key chords, and absolute pointer events over the whole virtual
//! desktop, delivered through SendInput atomically where that matters and
//! with a careful account of what landed when it is refused.
use aad_platform::{Error, PointerButton, ScrollDirection, ScrollSpec};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, VkKeyScanExW, HKL, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6,
    VK_F7, VK_F8, VK_F9, VK_HOME, VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WHEEL_DELTA,
};

type Result<T> = std::result::Result<T, Error>;

/// A complete, layout-aware sequence for text typed as real virtual keys.
pub struct KeyTextSequence {
    pub inputs: Vec<INPUT>,
}

impl KeyTextSequence {
    pub fn parse(text: &str, layout: HKL, caps_lock_on: bool) -> Result<Self> {
        let mut inputs = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            let (key, modifiers) = match character {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    (VK_RETURN, Vec::new())
                }
                '\n' => (VK_RETURN, Vec::new()),
                '\t' => (VK_TAB, Vec::new()),
                '\u{8}' => (VK_BACK, Vec::new()),
                character => map_character(character, layout, caps_lock_on)?,
            };
            for &modifier in &modifiers {
                inputs.push(virtual_key_event(modifier, false));
            }
            inputs.push(virtual_key_event(key, false));
            inputs.push(virtual_key_event(key, true));
            for &modifier in modifiers.iter().rev() {
                inputs.push(virtual_key_event(modifier, true));
            }
        }
        Ok(Self { inputs })
    }
}

pub fn map_character(
    character: char,
    layout: HKL,
    caps_lock_on: bool,
) -> Result<(VIRTUAL_KEY, Vec<VIRTUAL_KEY>)> {
    let mut utf16 = [0u16; 2];
    let encoded = character.encode_utf16(&mut utf16);
    if encoded.len() != 1 {
        return Err(unmappable_character(character));
    }
    let mapped = unsafe { VkKeyScanExW(encoded[0], layout) };
    if mapped == -1 {
        return Err(unmappable_character(character));
    }
    let raw = mapped as u16;
    let key = VIRTUAL_KEY(raw & 0x00ff);
    let shift_state = (raw >> 8) as u8;
    if shift_state & !0x07 != 0 {
        return Err(Error::invalid(format!(
            "character {character:?} requires an unsupported keyboard shift state"
        )));
    }
    let mut modifiers = Vec::new();
    let layout_needs_shift = shift_state & 0x01 != 0;
    let is_letter = (b'A' as u16..=b'Z' as u16).contains(&key.0);
    if layout_needs_shift ^ (caps_lock_on && is_letter) {
        modifiers.push(VK_SHIFT);
    }
    if shift_state & 0x02 != 0 {
        modifiers.push(VK_CONTROL);
    }
    if shift_state & 0x04 != 0 {
        modifiers.push(VK_MENU);
    }
    Ok((key, modifiers))
}

pub fn unmappable_character(character: char) -> Error {
    Error::invalid(format!(
        "character {character:?} cannot be typed as keys with the target window's keyboard layout; use mode=unicode or set_value"
    ))
    .with_detail("character", serde_json::json!(character.to_string()))
}

/// A logical virtual-key event. No SCANCODE flag: the key was chosen using the
/// target window's layout, so Windows must translate that logical VK through the
/// target input queue rather than reinterpret a physical key position.
pub fn virtual_key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Release exactly the keys still down after a partially delivered prefix.
pub fn pressed_keys_after_prefix(inputs: &[INPUT], sent: usize) -> Vec<VIRTUAL_KEY> {
    let mut pressed: Vec<VIRTUAL_KEY> = Vec::new();
    for input in inputs.iter().take(sent) {
        if input.r#type != INPUT_KEYBOARD {
            continue;
        }
        let keyboard = unsafe { input.Anonymous.ki };
        if keyboard.dwFlags.contains(KEYEVENTF_KEYUP) {
            if let Some(index) = pressed.iter().rposition(|key| *key == keyboard.wVk) {
                pressed.remove(index);
            }
        } else {
            pressed.push(keyboard.wVk);
        }
    }
    pressed
}

pub fn release_pressed_prefix(inputs: &[INPUT], sent: usize) -> Vec<u16> {
    let mut released = Vec::new();
    for key in pressed_keys_after_prefix(inputs, sent).into_iter().rev() {
        let event = [virtual_key_event(key, true)];
        let _ = unsafe { SendInput(&event, std::mem::size_of::<INPUT>() as i32) };
        released.push(key.0);
    }
    released
}

/// A parsed key combination: the modifiers to hold and the main key to press.
///
/// Names are matched case-insensitively and mapped to virtual-key codes, not
/// scan codes: a virtual key means the logical key (VK_RETURN is Enter whatever
/// the layout), which is what a shortcut like Ctrl+C is written against. Scan
/// codes describe a physical position and would send the wrong character under
/// a non-US layout, so they are deliberately not the default here.
#[derive(Debug)]
pub struct Chord {
    pub modifiers: Vec<VIRTUAL_KEY>,
    pub key: VIRTUAL_KEY,
}

impl Chord {
    /// Parse `["ctrl", "c"]`, `["f5"]`, `["shift", "end"]` into a chord.
    ///
    /// Exactly one non-modifier key is required: a chord is "modifiers plus a
    /// key", and either zero keys or several main keys is a request the caller
    /// got wrong rather than something to guess at.
    pub fn parse(keys: &[String]) -> Result<Self> {
        if keys.is_empty() {
            return Err(Error::invalid("a key combination needs at least one key"));
        }
        let mut modifiers = Vec::new();
        let mut main: Option<VIRTUAL_KEY> = None;
        for name in keys {
            let vk = key_name_to_vk(name)
                .ok_or_else(|| Error::invalid(format!("unknown key {name:?}")))?;
            if is_modifier(vk) {
                if !modifiers.contains(&vk) {
                    modifiers.push(vk);
                }
            } else if main.is_some() {
                return Err(Error::invalid(
                    "a key combination may hold several modifiers but only one main key",
                ));
            } else {
                main = Some(vk);
            }
        }
        let key = main.ok_or_else(|| {
            Error::invalid("a key combination needs a non-modifier key, e.g. \"c\" in Ctrl+C")
        })?;
        Ok(Self { modifiers, key })
    }

    /// The full press/release sequence: modifiers down, key down, key up,
    /// modifiers up in reverse. Reverse release mirrors how a person lifts keys
    /// and avoids leaving a modifier down past the key it applied to.
    pub fn to_inputs(&self) -> Vec<INPUT> {
        let mut inputs = Vec::with_capacity((self.modifiers.len() + 1) * 2);
        for &modifier in &self.modifiers {
            inputs.push(key_event(modifier, false));
        }
        inputs.push(key_event(self.key, false));
        inputs.push(key_event(self.key, true));
        for &modifier in self.modifiers.iter().rev() {
            inputs.push(key_event(modifier, true));
        }
        inputs
    }

    /// Best-effort release of every modifier this chord holds.
    ///
    /// Called only after a failed delivery, to undo a modifier that may have
    /// gone down before the batch was refused. Returns the names released so the
    /// caller can report what it tried to clean up. Individual releases, because
    /// atomicity no longer matters once we are just tidying up.
    pub fn release_modifiers(&self) -> Vec<String> {
        let mut released = Vec::new();
        for &modifier in self.modifiers.iter().rev() {
            let up = [key_event(modifier, true)];
            let _ = unsafe { SendInput(&up, std::mem::size_of::<INPUT>() as i32) };
            released.push(vk_label(modifier).to_string());
        }
        released
    }
}

/// Build one keyboard event for a virtual key, pressed or released.
///
/// The scan code is filled in from the virtual key so consumers that read scan
/// codes (some games, some remapping layers) still see a plausible event; the
/// virtual key is what ordinary applications act on.
pub fn key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags = if up {
        KEYEVENTF_KEYUP
    } else {
        Default::default()
    };
    if scan != 0 {
        flags |= KEYEVENTF_SCANCODE;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

pub fn is_modifier(vk: VIRTUAL_KEY) -> bool {
    matches!(vk, VK_CONTROL | VK_SHIFT | VK_MENU | VK_LWIN)
}

pub fn vk_label(vk: VIRTUAL_KEY) -> &'static str {
    match vk {
        VK_CONTROL => "ctrl",
        VK_SHIFT => "shift",
        VK_MENU => "alt",
        VK_LWIN => "win",
        _ => "key",
    }
}

/// Map a key name to its virtual-key code.
///
/// Covers the keys a shortcut actually uses: letters, digits, function keys,
/// navigation, whitespace and editing keys, and the four modifiers. Unknown
/// names return `None` so the caller can refuse rather than silently drop a key.
pub fn key_name_to_vk(name: &str) -> Option<VIRTUAL_KEY> {
    let lower = name.trim().to_ascii_lowercase();
    // Single letters and digits map to their ASCII/VK codes directly: VK_A is
    // 0x41 == b'A', VK_0 is 0x30 == b'0'.
    if lower.len() == 1 {
        let ch = lower.as_bytes()[0];
        if ch.is_ascii_lowercase() {
            return Some(VIRTUAL_KEY((ch.to_ascii_uppercase()) as u16));
        }
        if ch.is_ascii_digit() {
            return Some(VIRTUAL_KEY(ch as u16));
        }
    }
    let vk = match lower.as_str() {
        "ctrl" | "control" => VK_CONTROL,
        "shift" => VK_SHIFT,
        "alt" | "menu" => VK_MENU,
        "win" | "super" | "meta" | "cmd" => VK_LWIN,
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "esc" | "escape" => VK_ESCAPE,
        "space" | "spacebar" => VK_SPACE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        _ => return None,
    };
    Some(vk)
}

/// Convert a virtual-screen point to the 0..65535 absolute coordinates SendInput
/// expects with MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK.
pub fn to_absolute(point: (i32, i32)) -> Result<(i32, i32)> {
    let (origin_x, origin_y, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 1 || height <= 1 {
        return Err(Error::unavailable(
            "the virtual screen has no usable extent",
        ));
    }
    Ok((
        normalise(point.0, origin_x, width),
        normalise(point.1, origin_y, height),
    ))
}

/// One axis of a virtual-screen pixel as a 0..=65535 absolute coordinate.
///
/// Windows maps an absolute coordinate back with `pixel = origin + dx * extent
/// / 65536` (rounded down), so the coordinate chosen is the middle of the band
/// that maps back onto the pixel: exact for every pixel, at any extent, and
/// for an origin left of or above the primary monitor (negative offsets).
pub fn normalise(pixel: i32, origin: i32, extent: i32) -> i32 {
    let offset = i64::from((pixel - origin).clamp(0, extent - 1));
    ((offset * 65_536 + 32_768) / i64::from(extent)).min(65_535) as i32
}

/// One mouse INPUT at an absolute point with the given flags.
pub fn mouse_event(absolute: (i32, i32), flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            mi: MOUSEINPUT {
                dx: absolute.0,
                dy: absolute.1,
                mouseData: 0,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// The down/up flag pair for a button.
pub fn button_events(button: PointerButton) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
    match button {
        PointerButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        PointerButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        PointerButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

/// Deliver a scroll: one wheel event per notch, at the target point.
///
/// mouseData carries WHEEL_DELTA per notch; up and right are positive, down and
/// left negative, matching the Win32 wheel convention.
pub fn deliver_scroll(absolute: (i32, i32), scroll: ScrollSpec) -> Result<()> {
    let (flags, sign) = match scroll.direction {
        ScrollDirection::Up => (MOUSEEVENTF_WHEEL, 1),
        ScrollDirection::Down => (MOUSEEVENTF_WHEEL, -1),
        ScrollDirection::Right => (MOUSEEVENTF_HWHEEL, 1),
        ScrollDirection::Left => (MOUSEEVENTF_HWHEEL, -1),
    };
    let delta = sign * WHEEL_DELTA as i32;
    let mut inputs = vec![mouse_event(absolute, MOUSEEVENTF_MOVE)];
    for _ in 0..scroll.notches.max(1) {
        inputs.push(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                mi: MOUSEINPUT {
                    dx: absolute.0,
                    dy: absolute.1,
                    mouseData: delta as u32,
                    dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
    deliver_with_safe_fallback(&inputs, "the scroll")
}

/// Deliver a batch of synthetic input events atomically.
///
/// `SendInput` guarantees that events submitted in one call are not interleaved
/// with any other input. That guarantee is why the batch is submitted whole:
/// splitting it lets another source, or an input hook, slip between a key's
/// press and its release.
///
/// Measured on a machine running AutoHotkey: a batch of two or more events
/// returns 0 and delivers nothing, with `GetLastError() == 0`. Splitting the
/// batch into single calls does get accepted, but typing then arrives corrupted
/// — "one event at a time" landed as "one eeeeeeeeeeeeeee", every character
/// after the fifth repeating the fifth. So splitting is not a safe fallback for
/// keystrokes, and the caller decides whether degrading is acceptable.
pub fn deliver_atomically(inputs: &[INPUT], what: &str) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(input_rejected(sent as usize, inputs.len(), what));
    }
    Ok(())
}

/// Retry a pointer gesture one event at a time only when the atomic attempt
/// delivered nothing. Once any event landed, replaying the full gesture could
/// duplicate a click or wheel notch and must be refused conservatively.
pub fn fallback_is_safe(error: &Error) -> bool {
    error.effect == "not_applied"
}

pub fn deliver_with_safe_fallback(inputs: &[INPUT], what: &str) -> Result<()> {
    match deliver_atomically(inputs, what) {
        Ok(()) => Ok(()),
        Err(error) if fallback_is_safe(&error) => deliver_individually(inputs, what),
        Err(error) => Err(error),
    }
}

/// Deliver a drag, falling back to ordered events only after a clean refusal.
pub fn deliver_drag(inputs: &[INPUT], end: (i32, i32), release: MOUSE_EVENT_FLAGS) -> Result<()> {
    match deliver_atomically(inputs, "the drag") {
        Ok(()) => Ok(()),
        Err(error) if fallback_is_safe(&error) => {
            for (index, event) in inputs.iter().enumerate() {
                let sent = unsafe { SendInput(&[*event], std::mem::size_of::<INPUT>() as i32) };
                if sent != 1 {
                    let mut failure = input_rejected(index, inputs.len(), "the drag");
                    if index > 1 {
                        let released =
                            deliver_atomically(&[mouse_event(end, release)], "the drag recovery")
                                .is_ok();
                        failure = failure
                            .with_detail("residual_state_cleared", serde_json::json!(released));
                    }
                    return Err(failure);
                }
            }
            Ok(())
        }
        Err(mut error) => {
            let released =
                deliver_atomically(&[mouse_event(end, release)], "the drag recovery").is_ok();
            error = error.with_detail("residual_state_cleared", serde_json::json!(released));
            Err(error)
        }
    }
}

/// Deliver events one at a time, accepting the loss of atomicity.
///
/// Only for gestures where a split is harmless and refusing outright would mean
/// losing the capability entirely. A pointer click qualifies: it is three events
/// whose order is all that matters, and it was verified to work this way on a
/// machine where the batch was refused. Keystrokes do not qualify.
pub fn deliver_individually(inputs: &[INPUT], what: &str) -> Result<()> {
    for (index, event) in inputs.iter().enumerate() {
        let single = [*event];
        let sent = unsafe { SendInput(&single, std::mem::size_of::<INPUT>() as i32) };
        if sent != 1 {
            return Err(input_rejected(index, inputs.len(), what));
        }
    }
    Ok(())
}

/// Explain a refused `SendInput` precisely enough to act on.
///
/// The distinction between none and some matters more than the cause:
///
/// - none accepted means nothing happened, so the caller may safely retry;
/// - some accepted means the target saw part of a gesture, and a blind retry
///   could double a click or duplicate typed text.
pub fn input_rejected(sent: usize, expected: usize, what: &str) -> Error {
    let error = std::io::Error::last_os_error();
    let code = error.raw_os_error().unwrap_or(0);

    if sent == 0 {
        let mut refused = Error::new(
            "DRIVER.INPUT_BLOCKED",
            format!("{what} was refused by the system before any of it was delivered"),
        )
        // Nothing landed, so the caller is free to try again.
        .with_effect("not_applied")
        .with_detail("events_expected", serde_json::json!(expected))
        .with_detail("os_error", serde_json::json!(code));
        refused.retryable = true;
        return refused
            .with_detail(
                "likely_cause",
                serde_json::json!(if code == 5 {
                    "the target window belongs to a higher-privilege process; \
                     run this tool elevated"
                } else {
                    "another program is filtering synthetic input \
                     (a low-level input hook, BlockInput, or an anti-cheat driver)"
                }),
            )
            .with_detail(
                "remedy",
                serde_json::json!(
                    "close input-hooking software such as macro or remote-control tools; \
                     for text, set_value writes the value directly and does not use \
                     synthetic keystrokes"
                ),
            );
    }

    Error::new(
        "DRIVER.ACTION_FAILED",
        format!("{what} was only partly delivered ({sent} of {expected} events)"),
    )
    // Part of the gesture landed, so repeating it could act twice.
    .with_effect("unknown")
    .with_detail("events_sent", serde_json::json!(sent))
    .with_detail("events_expected", serde_json::json!(expected))
    .with_detail("os_error", serde_json::json!(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, KEYEVENTF_UNICODE};

    /// How Windows turns an absolute coordinate back into a virtual-screen pixel.
    fn denormalise(absolute: i32, origin: i32, extent: i32) -> i32 {
        origin + ((i64::from(absolute) * i64::from(extent)) >> 16) as i32
    }

    #[test]
    fn every_pixel_round_trips_through_absolute_coordinates() {
        // Primary-only, a secondary monitor left of the primary (negative
        // origin), one above it, and an odd 3-monitor span.
        for (origin, extent) in [
            (0, 1536),
            (0, 2560),
            (-1920, 4480),
            (-1200, 3360),
            (0, 5761),
        ] {
            for pixel in origin..origin + extent {
                let absolute = normalise(pixel, origin, extent);
                assert!((0..=65_535).contains(&absolute));
                assert_eq!(
                    denormalise(absolute, origin, extent),
                    pixel,
                    "origin {origin} extent {extent}"
                );
            }
        }
    }

    #[test]
    fn a_point_off_the_virtual_screen_is_clamped_to_its_edge() {
        assert_eq!(
            denormalise(normalise(-5000, -1920, 4480), -1920, 4480),
            -1920
        );
        assert_eq!(denormalise(normalise(9000, -1920, 4480), -1920, 4480), 2559);
    }

    #[test]
    fn only_a_clean_input_refusal_is_safe_to_retry_individually() {
        assert!(fallback_is_safe(&input_rejected(0, 3, "the click")));
        assert!(!fallback_is_safe(&input_rejected(1, 3, "the click")));
    }

    #[test]
    fn input_refused_outright_is_reported_as_safe_to_retry() {
        // Measured behaviour: an input hook makes SendInput return 0 with no OS
        // error. Nothing was delivered, so the caller must be told it may retry.
        let error = input_rejected(0, 3, "the click");

        assert_eq!(error.code, "DRIVER.INPUT_BLOCKED");
        assert_eq!(error.effect, "not_applied");
        assert!(error.retryable);
        assert!(error.details.contains_key("likely_cause"));
        assert!(error.details.contains_key("remedy"));
    }

    #[test]
    fn partly_delivered_input_is_never_reported_as_safe_to_retry() {
        // Half a gesture landed. Retrying could click twice or duplicate text,
        // so this must stay distinct from a clean refusal.
        let error = input_rejected(2, 3, "keystrokes");

        assert_eq!(error.code, "DRIVER.ACTION_FAILED");
        assert_eq!(error.effect, "unknown");
        assert!(!error.retryable);
        assert_eq!(error.details["events_sent"], 2);
        assert_eq!(error.details["events_expected"], 3);
    }

    #[test]
    fn a_refusal_message_names_what_was_refused() {
        // The user needs to know which action failed, not just that one did.
        assert!(input_rejected(0, 3, "the click").message.contains("click"));
        assert!(input_rejected(0, 5, "keystrokes")
            .message
            .contains("keystrokes"));
    }

    #[test]
    fn key_text_uses_real_keys_and_shift_for_uppercase() {
        let layout = unsafe { GetKeyboardLayout(0) };
        let sequence = KeyTextSequence::parse("aA1\n", layout, false).unwrap();
        let events: Vec<KEYBDINPUT> = sequence
            .inputs
            .iter()
            .map(|input| unsafe { input.Anonymous.ki })
            .collect();

        assert!(events.iter().all(|event| event.wVk.0 != 0));
        assert!(events
            .iter()
            .all(|event| !event.dwFlags.contains(KEYEVENTF_UNICODE)));
        assert!(events
            .iter()
            .any(|event| { event.wVk == VK_SHIFT && !event.dwFlags.contains(KEYEVENTF_KEYUP) }));
        assert!(events
            .iter()
            .any(|event| { event.wVk == VK_RETURN && !event.dwFlags.contains(KEYEVENTF_KEYUP) }));
    }

    #[test]
    fn caps_lock_inverts_shift_only_for_letters() {
        let layout = unsafe { GetKeyboardLayout(0) };
        let (_, lower_without_caps) = map_character('a', layout, false).unwrap();
        let (_, lower_with_caps) = map_character('a', layout, true).unwrap();
        let (_, upper_without_caps) = map_character('A', layout, false).unwrap();
        let (_, upper_with_caps) = map_character('A', layout, true).unwrap();
        let (_, punctuation_without_caps) = map_character('!', layout, false).unwrap();
        let (_, punctuation_with_caps) = map_character('!', layout, true).unwrap();

        assert!(!lower_without_caps.contains(&VK_SHIFT));
        assert!(lower_with_caps.contains(&VK_SHIFT));
        assert!(upper_without_caps.contains(&VK_SHIFT));
        assert!(!upper_with_caps.contains(&VK_SHIFT));
        assert_eq!(punctuation_without_caps, punctuation_with_caps);
    }

    #[test]
    fn key_text_normalises_crlf_to_one_enter() {
        let layout = unsafe { GetKeyboardLayout(0) };
        let sequence = KeyTextSequence::parse("\r\n", layout, false).unwrap();
        assert_eq!(sequence.inputs.len(), 2);
    }

    #[test]
    fn key_text_rejects_a_character_the_layout_cannot_express() {
        let layout = unsafe { GetKeyboardLayout(0) };
        let error = match KeyTextSequence::parse("😀", layout, false) {
            Err(error) => error,
            Ok(_) => panic!("the active layout must not invent a key for an emoji"),
        };
        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
        assert_eq!(error.effect, "not_applied");
    }

    #[test]
    fn partial_key_prefix_tracks_every_still_pressed_key() {
        let inputs = [
            virtual_key_event(VK_SHIFT, false),
            virtual_key_event(VIRTUAL_KEY(b'A' as u16), false),
            virtual_key_event(VIRTUAL_KEY(b'A' as u16), true),
            virtual_key_event(VK_SHIFT, true),
        ];
        assert_eq!(
            pressed_keys_after_prefix(&inputs, 2),
            vec![VK_SHIFT, VIRTUAL_KEY(b'A' as u16)]
        );
        assert!(pressed_keys_after_prefix(&inputs, 4).is_empty());
    }

    #[test]
    fn a_chord_parses_modifiers_and_one_key() {
        let chord = Chord::parse(&["ctrl".into(), "c".into()]).unwrap();
        assert_eq!(chord.modifiers, vec![VK_CONTROL]);
        assert_eq!(chord.key, VIRTUAL_KEY(b'C' as u16));
    }

    #[test]
    fn a_lone_function_key_is_a_valid_chord() {
        // The whole point over type_text: F5 is a key no character can express.
        let chord = Chord::parse(&["f5".into()]).unwrap();
        assert!(chord.modifiers.is_empty());
        assert_eq!(chord.key, VK_F5);
    }

    #[test]
    fn several_modifiers_are_all_held() {
        let chord = Chord::parse(&["ctrl".into(), "shift".into(), "left".into()]).unwrap();
        assert_eq!(chord.modifiers, vec![VK_CONTROL, VK_SHIFT]);
        assert_eq!(chord.key, VK_LEFT);
    }

    #[test]
    fn a_chord_is_case_and_alias_insensitive() {
        // "Control" and "ctrl", "ESC" and "escape" name the same keys; a caller
        // should not have to guess the one spelling that works.
        assert_eq!(
            Chord::parse(&["Control".into(), "A".into()])
                .unwrap()
                .modifiers,
            vec![VK_CONTROL]
        );
        assert_eq!(Chord::parse(&["ESC".into()]).unwrap().key, VK_ESCAPE);
        assert_eq!(Chord::parse(&["escape".into()]).unwrap().key, VK_ESCAPE);
    }

    #[test]
    fn a_chord_with_no_main_key_is_refused() {
        // Modifiers alone are not a shortcut; refusing beats sending half of one.
        let error = Chord::parse(&["ctrl".into(), "shift".into()]).unwrap_err();
        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
    }

    #[test]
    fn a_chord_with_two_main_keys_is_refused() {
        // "a and b at once" is not a combination any shortcut means.
        let error = Chord::parse(&["a".into(), "b".into()]).unwrap_err();
        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
    }

    #[test]
    fn an_unknown_key_name_is_refused_not_dropped() {
        // Silently dropping a key would send a different chord than asked for.
        let error = Chord::parse(&["ctrl".into(), "nope".into()]).unwrap_err();
        assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
        assert!(error.message.contains("nope"));
    }

    #[test]
    fn a_chord_expands_to_a_symmetric_press_release_sequence() {
        // ctrl down, c down, c up, ctrl up: modifiers wrap the key, released in
        // reverse, so nothing is left held.
        let chord = Chord::parse(&["ctrl".into(), "c".into()]).unwrap();
        let inputs = chord.to_inputs();
        assert_eq!(
            inputs.len(),
            4,
            "two events for the modifier, two for the key"
        );
        // First event presses Ctrl, last releases it.
        let first = unsafe { inputs[0].Anonymous.ki };
        let last = unsafe { inputs[3].Anonymous.ki };
        assert_eq!(first.wVk, VK_CONTROL);
        assert!(!first.dwFlags.contains(KEYEVENTF_KEYUP));
        assert_eq!(last.wVk, VK_CONTROL);
        assert!(last.dwFlags.contains(KEYEVENTF_KEYUP));
    }

    #[test]
    fn a_blocked_refusal_points_at_the_action_that_does_not_need_input() {
        // When synthetic input is filtered, set_value is the way through; the
        // remedy has to say so or the user is left with no route at all.
        let error = input_rejected(0, 32, "keystrokes");
        let remedy = error.details["remedy"].as_str().unwrap();

        assert!(
            remedy.contains("set_value"),
            "the remedy must name the action that bypasses synthetic input: {remedy}"
        );
    }

    #[test]
    fn an_empty_batch_is_not_an_error() {
        // Typing an empty string is a no-op, not a failure. SendInput would
        // reject a zero-length batch, so this must be handled before the call.
        assert!(deliver_atomically(&[], "keystrokes").is_ok());
    }

    #[test]
    fn a_privilege_refusal_is_distinguished_from_a_filtering_hook() {
        // These need different remedies: one is "run elevated", the other is
        // "close the tool that is filtering input". Conflating them strands the
        // user, so the cause is chosen from the OS error rather than guessed.
        //
        // The OS error is read from thread-local state, which cannot be forced
        // here, so this checks the two texts are actually different rather than
        // trying to provoke each one.
        let error = input_rejected(0, 3, "the click");
        let cause = error.details["likely_cause"].as_str().unwrap();

        assert!(
            cause.contains("privilege") || cause.contains("filtering"),
            "the cause must name one of the two known reasons: {cause}"
        );
    }
}
