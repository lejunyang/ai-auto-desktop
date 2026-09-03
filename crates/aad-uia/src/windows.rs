//! The native Windows UI Automation backend.
//!
//! This is the only file that talks to COM.  It is compiled on Windows only;
//! every other part of the driver is portable and tested on all platforms.
//!
//! The backend is deliberately conservative: it reads element properties
//! through the cached-free live tree, refuses to synthesise input for elements
//! that report no bounding rectangle, and prefers UI Automation control
//! patterns over raw input whenever a pattern is available.

#![cfg(windows)]

use crate::backend::{Backend, CaptureLimits, CapturedTree, DriverError, Result};
use crate::model::{Bounds, Node, States, WindowInfo};
use windows::core::{BSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, MAX_PATH, RECT, TRUE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationLegacyIAccessiblePattern, IUIAutomationTogglePattern, IUIAutomationValuePattern,
    TreeScope_Children, UIA_InvokePatternId, UIA_LegacyIAccessiblePatternId,
    UIA_TogglePatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetSystemMetrics, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SetForegroundWindow, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

/// Initialise COM on the calling thread.
///
/// COM apartments are per-thread, not per-process, so this must run on every
/// thread that touches UI Automation.  Using a process-wide guard here would
/// leave later threads using COM in an uninitialised apartment, which fails in
/// ways that surface as access violations rather than errors.
fn ensure_com() {
    thread_local! {
        static INITIALISED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    INITIALISED.with(|initialised| {
        if !initialised.get() {
            unsafe {
                // An "already initialised" result is a success for our
                // purposes: the host may own the apartment.
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            initialised.set(true);
        }
    });
}

fn automation() -> Result<IUIAutomation> {
    ensure_com();
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.map_err(|error| {
        DriverError::unavailable(format!("UI Automation is unavailable: {error}"))
    })
}

/// The native Windows backend.
pub struct WindowsUiaBackend {
    automation: IUIAutomation,
}

// The UI Automation client object is agile and safe to use from the driver's
// serialised call path.
unsafe impl Send for WindowsUiaBackend {}
unsafe impl Sync for WindowsUiaBackend {}

impl WindowsUiaBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            automation: automation()?,
        })
    }

    fn element_for(&self, window_id: &str) -> Result<(HWND, IUIAutomationElement)> {
        let handle = parse_window_id(window_id)?;
        let element = unsafe { self.automation.ElementFromHandle(handle) }.map_err(|error| {
            DriverError::new(
                "DRIVER.WINDOW_NOT_FOUND",
                format!("window {window_id} could not be resolved: {error}"),
            )
        })?;
        Ok((handle, element))
    }

    /// Walk the element tree breadth-first within the configured limits.
    fn walk(&self, root: &IUIAutomationElement, limits: CaptureLimits) -> (Vec<Node>, bool) {
        let mut nodes = Vec::new();
        let mut truncated = false;
        // (element, depth, parent node id)
        let mut queue: std::collections::VecDeque<(IUIAutomationElement, u32, Option<String>)> =
            std::collections::VecDeque::new();
        queue.push_back((root.clone(), 0, None));

        let condition = unsafe { self.automation.CreateTrueCondition() }.ok();
        let mut counter = 0usize;

        while let Some((element, depth, parent_id)) = queue.pop_front() {
            if nodes.len() >= limits.max_nodes {
                truncated = true;
                break;
            }
            counter += 1;
            let node_id = format!("e{counter}");
            let mut node = describe_element(&element, &node_id, depth, parent_id.clone());

            if depth < limits.max_depth {
                if let Some(condition) = condition.as_ref() {
                    if let Ok(children) =
                        unsafe { element.FindAll(TreeScope_Children, condition) }
                    {
                        let count = unsafe { children.Length() }.unwrap_or(0);
                        for index in 0..count {
                            if let Ok(child) = unsafe { children.GetElement(index) } {
                                // Reserve the child's future id so the parent can
                                // list it without walking twice.
                                node.children
                                    .push(format!("e{}", counter + queue.len() + 1));
                                queue.push_back((child, depth + 1, Some(node_id.clone())));
                            }
                        }
                    }
                }
            } else if depth >= limits.max_depth {
                truncated = true;
            }

            nodes.push(node);
        }

        if !queue.is_empty() {
            truncated = true;
        }

        // The reserved child ids are predictions; rebuild them from the real
        // parent links so the tree we return is internally consistent.
        rebuild_children(&mut nodes);
        (nodes, truncated)
    }
}

/// Recompute each node's children from the recorded parent links.
fn rebuild_children(nodes: &mut [Node]) {
    let links: Vec<(usize, String)> = nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| node.parent_id.clone().map(|parent| (index, parent)))
        .collect();
    for node in nodes.iter_mut() {
        node.children.clear();
    }
    for (index, parent_id) in links {
        let child_id = nodes[index].node_id.clone();
        if let Some(parent) = nodes.iter_mut().find(|node| node.node_id == parent_id) {
            parent.children.push(child_id);
        }
    }
}

impl Backend for WindowsUiaBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let mut handles: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(collect_window),
                LPARAM(&mut handles as *mut Vec<HWND> as isize),
            );
        }

        let foreground = unsafe { GetForegroundWindow() };
        let mut windows = Vec::new();
        for handle in handles {
            let title = window_text(handle);
            // A window with no title and no visible area is scaffolding, not
            // something a user or an agent can act on.
            if title.is_empty() {
                continue;
            }
            let mut process_id = 0u32;
            unsafe { GetWindowThreadProcessId(handle, Some(&mut process_id)) };

            windows.push(WindowInfo {
                window_id: format!("hwnd:{}", handle.0 as isize),
                title,
                process_id,
                process_name: process_name(process_id),
                class_name: Some(class_name(handle)).filter(|name| !name.is_empty()),
                bounds: window_bounds(handle),
                is_foreground: handle == foreground,
                is_minimized: unsafe { IsIconic(handle) }.as_bool(),
            });
        }
        Ok(windows)
    }

    fn capture(&self, window_id: &str, limits: CaptureLimits) -> Result<CapturedTree> {
        let (handle, root) = self.element_for(window_id)?;
        let (nodes, truncated) = self.walk(&root, limits);

        let foreground = unsafe { GetForegroundWindow() };
        let mut process_id = 0u32;
        unsafe { GetWindowThreadProcessId(handle, Some(&mut process_id)) };

        Ok(CapturedTree {
            window: WindowInfo {
                window_id: window_id.to_string(),
                title: window_text(handle),
                process_id,
                process_name: process_name(process_id),
                class_name: Some(class_name(handle)).filter(|name| !name.is_empty()),
                bounds: window_bounds(handle),
                is_foreground: handle == foreground,
                is_minimized: unsafe { IsIconic(handle) }.as_bool(),
            },
            root_id: nodes.first().map(|node| node.node_id.clone()),
            truncated,
            nodes,
        })
    }

    fn verify(&self, window_id: &str, node: &Node) -> Result<bool> {
        let Some(live) = self.locate(window_id, node)? else {
            return Ok(false);
        };
        // Identity is re-established from the properties an agent reasoned
        // about, not from a pointer that may have been recycled.
        let current = describe_element(&live, &node.node_id, node.depth, node.parent_id.clone());
        Ok(current.role == node.role
            && current.name == node.name
            && current.automation_id == node.automation_id
            && current.class_name == node.class_name)
    }

    fn focus(&self, window_id: &str, node: &Node) -> Result<()> {
        let element = self.require(window_id, node)?;
        unsafe { element.SetFocus() }
            .map_err(|error| DriverError::new("DRIVER.ACTION_FAILED", format!("focus failed: {error}")))
    }

    fn invoke(&self, window_id: &str, node: &Node) -> Result<()> {
        let element = self.require(window_id, node)?;

        // Prefer a real control pattern: it is far more reliable than input
        // synthesis and does not depend on the window being frontmost.
        if let Ok(pattern) = unsafe { element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) } {
            return unsafe { pattern.Invoke() }.map_err(|error| {
                DriverError::new("DRIVER.ACTION_FAILED", format!("invoke failed: {error}"))
                    .with_effect("unknown")
            });
        }
        if let Ok(pattern) = unsafe { element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) } {
            return unsafe { pattern.Toggle() }.map_err(|error| {
                DriverError::new("DRIVER.ACTION_FAILED", format!("toggle failed: {error}"))
                    .with_effect("unknown")
            });
        }
        if let Ok(pattern) = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                UIA_LegacyIAccessiblePatternId,
            )
        } {
            return unsafe { pattern.DoDefaultAction() }.map_err(|error| {
                DriverError::new("DRIVER.ACTION_FAILED", format!("default action failed: {error}"))
                    .with_effect("unknown")
            });
        }

        Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "the element exposes no invokable pattern",
        ))
    }

    fn set_value(&self, window_id: &str, node: &Node, value: &str) -> Result<()> {
        let element = self.require(window_id, node)?;
        let pattern = unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
            .map_err(|_| {
                DriverError::new(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "the element does not expose a value pattern",
                )
            })?;

        let before = unsafe { pattern.CurrentValue() }
            .ok()
            .map(|value| value.to_string());

        unsafe { pattern.SetValue(&BSTR::from(value)) }.map_err(|error| {
            DriverError::new("DRIVER.ACTION_FAILED", format!("set_value failed: {error}"))
                .with_effect("unknown")
        })?;

        // SetValue returning S_OK is not evidence the value changed. Measured
        // against an HTML <select> in a WebView: it succeeds and the control
        // keeps its old value. Reporting that as applied is worse than an
        // error, because an error can be handled and a false success cannot.
        let after = unsafe { pattern.CurrentValue() }
            .ok()
            .map(|value| value.to_string());

        if write_was_refused(before.as_deref(), after.as_deref(), value) {
            return Err(DriverError::new(
                "DRIVER.ACTION_FAILED",
                "the element reported success but its value did not change",
            )
            // Nothing landed, so retrying is safe -- but repeating the same
            // call will fail the same way; the caller needs another route.
            .with_effect("not_applied")
            .with_detail("requested", serde_json::json!(value))
            .with_detail("value", serde_json::json!(after))
            .with_detail(
                "likely_cause",
                serde_json::json!(
                    "the control accepts the value pattern but ignores writes, \
                     as a web <select> does"
                ),
            )
            .with_detail(
                "remedy",
                serde_json::json!(
                    "drive this control the way a person would: focus it and \
                     invoke, or click the option"
                ),
            ));
        }
        Ok(())
    }

    fn type_text(&self, window_id: &str, node: &Node, text: &str) -> Result<()> {
        let element = self.require(window_id, node)?;
        // Typing goes to whatever holds focus, so focus must be established
        // first or the keystrokes land in the wrong place.
        unsafe { element.SetFocus() }.map_err(|error| {
            DriverError::new("DRIVER.ACTION_FAILED", format!("focus failed: {error}"))
        })?;

        let mut inputs: Vec<INPUT> = Vec::new();
        for unit in text.encode_utf16() {
            for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        if inputs.is_empty() {
            return Ok(());
        }
        // Keystrokes must arrive as one uninterrupted stream. If the batch is
        // refused, splitting it corrupts the text (verified), so this fails
        // rather than typing something the caller did not ask for. The error
        // points at set_value, which writes the value without synthetic input.
        deliver_atomically(&inputs, "keystrokes")
    }

    fn pointer_click(&self, window_id: &str, node: &Node) -> Result<()> {
        let (handle, _) = self.element_for(window_id)?;
        let (x, y) = crate::backend::click_point(node.bounds)?;

        // Clicking a background window would deliver the click to whatever is
        // actually in front, so raise the target first.
        unsafe {
            let _ = SetForegroundWindow(handle);
        }

        let (origin_x, origin_y, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if width <= 0 || height <= 0 {
            return Err(DriverError::unavailable("the virtual screen has no extent"));
        }
        let absolute_x = ((x - origin_x) as f64 * 65_535.0 / width as f64).round() as i32;
        let absolute_y = ((y - origin_y) as f64 * 65_535.0 / height as f64).round() as i32;

        let mouse = |flags| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                mi: MOUSEINPUT {
                    dx: absolute_x,
                    dy: absolute_y,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let inputs = [
            mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
            mouse(MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
            mouse(MOUSEEVENTF_LEFTUP | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
        ];
        // A click is a move, a press and a release: only their order matters, so
        // if the atomic batch is refused, sending them individually is a
        // faithful fallback. Verified to work on a machine where the batch was
        // refused by an input hook.
        deliver_atomically(&inputs, "the click")
            .or_else(|_| deliver_individually(&inputs, "the click"))
    }

    fn describe(&self) -> serde_json::Value {
        serde_json::json!({"backend": "windows-uia", "platform": "windows"})
    }
}

/// Whether a write left the control untouched.
///
/// Only an unchanged value counts as a refusal. Controls legitimately normalise
/// what they are given -- trimming, reformatting, clamping -- and those did
/// accept the write, so demanding that the value read back exactly would report
/// them as failures.
///
/// Two cases are deliberately not refusals:
///
/// - the control already held the requested value, so there was nothing to
///   change and nothing went wrong;
/// - the value could not be read before or after, so there is no evidence
///   either way and inventing a failure would be as wrong as inventing a
///   success.
fn write_was_refused(before: Option<&str>, after: Option<&str>, requested: &str) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => before == after && before != requested,
        _ => false,
    }
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
fn deliver_atomically(inputs: &[INPUT], what: &str) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(input_rejected(sent as usize, inputs.len(), what));
    }
    Ok(())
}

/// Deliver events one at a time, accepting the loss of atomicity.
///
/// Only for gestures where a split is harmless and refusing outright would mean
/// losing the capability entirely. A pointer click qualifies: it is three events
/// whose order is all that matters, and it was verified to work this way on a
/// machine where the batch was refused. Keystrokes do not qualify.
fn deliver_individually(inputs: &[INPUT], what: &str) -> Result<()> {
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
fn input_rejected(sent: usize, expected: usize, what: &str) -> DriverError {
    let error = std::io::Error::last_os_error();
    let code = error.raw_os_error().unwrap_or(0);

    if sent == 0 {
        let mut refused = DriverError::new(
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

    DriverError::new(
        "DRIVER.ACTION_FAILED",
        format!("{what} was only partly delivered ({sent} of {expected} events)"),
    )
    // Part of the gesture landed, so repeating it could act twice.
    .with_effect("unknown")
    .with_detail("events_sent", serde_json::json!(sent))
    .with_detail("events_expected", serde_json::json!(expected))
    .with_detail("os_error", serde_json::json!(code))
}

impl WindowsUiaBackend {
    /// Find the live element corresponding to a captured node.
    fn locate(&self, window_id: &str, node: &Node) -> Result<Option<IUIAutomationElement>> {
        let (_, root) = self.element_for(window_id)?;
        let Ok(condition) = (unsafe { self.automation.CreateTrueCondition() }) else {
            return Ok(None);
        };

        // Re-walk the tree in the same order as the capture, so the recorded
        // node id addresses the same element.
        let mut queue: std::collections::VecDeque<(IUIAutomationElement, u32)> =
            std::collections::VecDeque::new();
        queue.push_back((root, 0));
        let mut counter = 0usize;
        let wanted = node.node_id.trim_start_matches('e').parse::<usize>().ok();

        while let Some((element, depth)) = queue.pop_front() {
            counter += 1;
            if Some(counter) == wanted {
                return Ok(Some(element));
            }
            if counter > CaptureLimits::MAX_NODES {
                break;
            }
            if depth < CaptureLimits::MAX_DEPTH {
                if let Ok(children) = unsafe { element.FindAll(TreeScope_Children, &condition) } {
                    let count = unsafe { children.Length() }.unwrap_or(0);
                    for index in 0..count {
                        if let Ok(child) = unsafe { children.GetElement(index) } {
                            queue.push_back((child, depth + 1));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn require(&self, window_id: &str, node: &Node) -> Result<IUIAutomationElement> {
        self.locate(window_id, node)?.ok_or_else(|| {
            DriverError::stale("the element is no longer present in the window")
        })
    }
}

// ---------------------------------------------------------------------------
// Property extraction
// ---------------------------------------------------------------------------

fn describe_element(
    element: &IUIAutomationElement,
    node_id: &str,
    depth: u32,
    parent_id: Option<String>,
) -> Node {
    let text = |result: windows::core::Result<BSTR>| -> Option<String> {
        result.ok().map(|value| value.to_string()).filter(|value| !value.is_empty())
    };
    let flag = |result: windows::core::Result<BOOL>| -> Option<bool> {
        result.ok().map(|value| value.as_bool())
    };

    let role = unsafe { element.CurrentLocalizedControlType() }
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());

    let value = unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        .ok()
        .and_then(|pattern| unsafe { pattern.CurrentValue() }.ok())
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty());

    let read_only = unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        .ok()
        .and_then(|pattern| unsafe { pattern.CurrentIsReadOnly() }.ok())
        .map(|value| value.as_bool());

    let bounds = unsafe { element.CurrentBoundingRectangle() }.ok().and_then(|rect| {
        let bounds = Bounds {
            x: rect.left,
            y: rect.top,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        };
        (!bounds.is_empty()).then_some(bounds)
    });

    let states = States {
        enabled: flag(unsafe { element.CurrentIsEnabled() }),
        offscreen: flag(unsafe { element.CurrentIsOffscreen() }),
        focusable: flag(unsafe { element.CurrentIsKeyboardFocusable() }),
        focused: flag(unsafe { element.CurrentHasKeyboardFocus() }),
        read_only,
        // Recorded so the absent value above is explained rather than looking
        // like a capture failure. UIA withholds a password field's text on its
        // own; this only reports that it did.
        protected: flag(unsafe { element.CurrentIsPassword() }),
    };

    let mut actions = Vec::new();
    if states.focusable == Some(true) {
        actions.push("focus".to_string());
    }
    let has = |id| unsafe { element.GetCurrentPattern(id) }.is_ok();
    if has(UIA_InvokePatternId) || has(UIA_TogglePatternId) || has(UIA_LegacyIAccessiblePatternId) {
        actions.push("invoke".to_string());
    }
    if has(UIA_ValuePatternId) && read_only != Some(true) {
        actions.push("set_value".to_string());
        actions.push("type_text".to_string());
    }
    if bounds.is_some() && states.offscreen != Some(true) {
        actions.push("pointer_click".to_string());
    }

    Node {
        node_id: node_id.to_string(),
        role,
        name: text(unsafe { element.CurrentName() }),
        value,
        automation_id: text(unsafe { element.CurrentAutomationId() }),
        class_name: text(unsafe { element.CurrentClassName() }),
        framework_id: text(unsafe { element.CurrentFrameworkId() }),
        bounds,
        states,
        actions,
        depth,
        parent_id,
        children: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

unsafe extern "system" fn collect_window(handle: HWND, param: LPARAM) -> BOOL {
    if IsWindowVisible(handle).as_bool() {
        let handles = &mut *(param.0 as *mut Vec<HWND>);
        handles.push(handle);
    }
    TRUE
}

fn parse_window_id(window_id: &str) -> Result<HWND> {
    let raw = window_id.strip_prefix("hwnd:").unwrap_or(window_id);
    raw.parse::<isize>()
        .map(|value| HWND(value as *mut std::ffi::c_void))
        .map_err(|_| DriverError::invalid(format!("{window_id:?} is not a window id")))
}

fn window_text(handle: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(handle) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(handle, &mut buffer) };
    String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
}

fn class_name(handle: HWND) -> String {
    let mut buffer = [0u16; 256];
    let copied = unsafe { GetClassNameW(handle, &mut buffer) };
    String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
}

fn window_bounds(handle: HWND) -> Option<Bounds> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(handle, &mut rect) }.ok()?;
    Some(Bounds {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

fn process_name(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut buffer = [0u16; MAX_PATH as usize];
        let mut size = buffer.len() as u32;
        let query = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        query.ok()?;
        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // -----------------------------------------------------------------------
    // Confirming a write actually landed
    //
    // Measured: SetValue on an HTML <select> in a WebView returns S_OK and
    // changes nothing. Reporting that as applied is worse than an error --
    // an error can be handled, a false success cannot.
    // -----------------------------------------------------------------------

    #[test]
    fn a_value_that_did_not_move_is_a_refusal() {
        // The measured case: asked for one thing, still holds the other.
        assert!(write_was_refused(
            Some("something appears"),
            Some("something appears"),
            "the value is exactly"
        ));
    }

    #[test]
    fn a_value_that_changed_is_accepted() {
        assert!(!write_was_refused(Some("old"), Some("new"), "new"));
    }

    #[test]
    fn a_control_that_already_held_the_value_has_not_refused_anything() {
        // Nothing needed to change. Calling this a failure would make writing
        // the same value twice fail the second time.
        assert!(!write_was_refused(Some("ready"), Some("ready"), "ready"));
    }

    #[test]
    fn a_control_that_normalises_the_value_is_not_treated_as_refusing() {
        // Trimming, reformatting and clamping are acceptance, not refusal.
        // Requiring an exact read-back would fail every such control.
        assert!(!write_was_refused(Some(""), Some("42"), "  42  "));
        assert!(!write_was_refused(Some("0"), Some("100"), "9999"));
    }

    #[test]
    fn an_unreadable_value_is_not_evidence_of_refusal() {
        // No before or no after means no evidence. Inventing a failure here
        // would be as wrong as inventing a success -- and would break every
        // write-only control.
        assert!(!write_was_refused(None, Some("x"), "x"));
        assert!(!write_was_refused(Some("x"), None, "y"));
        assert!(!write_was_refused(None, None, "y"));
    }

    #[test]
    fn a_window_id_round_trips_through_its_text_form() {
        let handle = parse_window_id("hwnd:12345").unwrap();
        assert_eq!(handle.0 as isize, 12345);
        // The bare numeric form is accepted too.
        assert_eq!(parse_window_id("12345").unwrap().0 as isize, 12345);
    }

    #[test]
    fn a_malformed_window_id_is_rejected() {
        assert!(parse_window_id("hwnd:not-a-number").is_err());
    }

    #[test]
    fn children_are_rebuilt_from_parent_links() {
        let make = |id: &str, parent: Option<&str>| Node {
            node_id: id.into(),
            role: "Pane".into(),
            name: None,
            value: None,
            automation_id: None,
            class_name: None,
            framework_id: None,
            bounds: None,
            states: States::default(),
            actions: Vec::new(),
            depth: 0,
            parent_id: parent.map(str::to_string),
            // Deliberately wrong, to prove it gets rebuilt.
            children: vec!["stale".into()],
        };
        let mut nodes = vec![
            make("e1", None),
            make("e2", Some("e1")),
            make("e3", Some("e1")),
            make("e4", Some("e2")),
        ];

        rebuild_children(&mut nodes);

        assert_eq!(nodes[0].children, vec!["e2".to_string(), "e3".to_string()]);
        assert_eq!(nodes[1].children, vec!["e4".to_string()]);
        assert!(nodes[2].children.is_empty());
    }

    #[test]
    fn the_backend_can_be_created_on_this_machine() {
        // UI Automation is a core OS component; if this fails the environment
        // is not one where the driver can work at all.
        let backend = WindowsUiaBackend::new();
        assert!(backend.is_ok(), "{:?}", backend.err());
    }

    #[test]
    fn listing_windows_finds_at_least_the_desktop_shell() {
        let Ok(backend) = WindowsUiaBackend::new() else {
            return;
        };
        let windows = backend.list_windows().expect("windows can be enumerated");

        // A live Windows session always has at least one titled window.
        assert!(!windows.is_empty(), "no windows were discovered");
        for window in &windows {
            assert!(window.window_id.starts_with("hwnd:"));
            assert!(!window.title.is_empty());
        }
    }

    /// Find a window this machine will actually let us inspect.
    ///
    /// Not every visible window can be resolved: some belong to protected or
    /// elevated processes, and some vanish between enumeration and capture.
    /// A test that assumed the first window was usable would be flaky, so we
    /// look for one that works and skip only if the whole session refuses.
    fn capturable(
        backend: &WindowsUiaBackend,
        limits: CaptureLimits,
    ) -> Option<(WindowInfo, CapturedTree)> {
        for window in backend.list_windows().ok()? {
            if let Ok(captured) = backend.capture(&window.window_id, limits) {
                if !captured.nodes.is_empty() {
                    return Some((window, captured));
                }
            }
        }
        None
    }

    #[test]
    fn a_discovered_window_can_be_captured() {
        let Ok(backend) = WindowsUiaBackend::new() else {
            return;
        };
        let Some((_, captured)) = capturable(&backend, CaptureLimits::default()) else {
            panic!("no window on this desktop could be captured");
        };

        assert_eq!(captured.root_id.as_deref(), Some("e1"));
        // Parent links must always point at a node in the same capture.
        let ids: Vec<&str> = captured.nodes.iter().map(|node| node.node_id.as_str()).collect();
        for node in &captured.nodes {
            if let Some(parent) = node.parent_id.as_deref() {
                assert!(ids.contains(&parent), "dangling parent {parent}");
            }
        }
        // Every child link must resolve too, or an agent cannot walk the tree.
        for node in &captured.nodes {
            for child in &node.children {
                assert!(ids.contains(&child.as_str()), "dangling child {child}");
            }
        }
    }

    #[test]
    fn a_captured_element_verifies_against_the_live_tree() {
        let Ok(backend) = WindowsUiaBackend::new() else {
            return;
        };
        let Some((window, captured)) = capturable(&backend, CaptureLimits::default()) else {
            panic!("no window on this desktop could be captured");
        };
        let root = captured.nodes.first().expect("the capture has a root");

        // The root was just read from this window, so it must still match.
        let verified = backend
            .verify(&window.window_id, root)
            .expect("verification completes");
        assert!(verified, "a freshly captured root must verify");

        // A node whose identity does not match the live element must not.
        let mut impostor = root.clone();
        impostor.name = Some("\u{1}a name no real element has".to_string());
        assert!(
            !backend.verify(&window.window_id, &impostor).unwrap_or(true),
            "a mismatched element must not verify"
        );
    }

    #[test]
    fn capture_respects_the_node_limit() {
        let Ok(backend) = WindowsUiaBackend::new() else {
            return;
        };
        let limits = CaptureLimits { max_depth: 4, max_nodes: 5 };
        let Some((_, captured)) = capturable(&backend, limits) else {
            panic!("no window on this desktop could be captured");
        };

        assert!(captured.nodes.len() <= 5);
        assert!(captured.nodes.iter().all(|node| node.depth <= 4));
    }

    #[test]
    fn an_unresolvable_window_is_reported_clearly() {
        let Ok(backend) = WindowsUiaBackend::new() else {
            return;
        };
        // A handle that cannot belong to a live window.
        let error = backend
            .capture("hwnd:1", CaptureLimits::default())
            .expect_err("a bogus handle cannot be captured");

        assert_eq!(error.code, "DRIVER.WINDOW_NOT_FOUND");
        assert_eq!(error.effect, "not_applied");
    }
}
