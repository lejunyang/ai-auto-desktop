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

use std::sync::Arc;

use crate::backend::{
    ensure_hold_budget, ensure_input_deadline, wait_input_hold, Backend, CaptureLimits,
    CapturedTree, DriverError, Pixels, PointerGesture, Recognition, Result,
};
use crate::capture::{CapturedEvent, EventBuffer, EventKind};
use crate::model::{Bounds, Node, States, WindowInfo};
use aad_windows::capture::{crop, extended_frame, intersect, print_window};
use aad_windows::clipboard::Paste;
use aad_windows::com::ensure as ensure_com;
use aad_windows::input::{
    button_events, deliver_atomically, deliver_pointer, deliver_scroll, ensure_inputs_released,
    ensure_modifiers_released, modified_pointer_inputs, mouse_event, release_pressed_prefix,
    to_absolute, virtual_key_event, Chord, HeldInput, KeyTextSequence,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};
use windows::core::{implement, BSTR, PWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Foundation::{
    CloseHandle, BOOL, HWND, LPARAM, MAX_PATH, POINT, RECT, TRUE, WPARAM,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS, UOI_NAME,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation,
    IUIAutomation,
    IUIAutomationElement,
    IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl,
    IUIAutomationInvokePattern,
    IUIAutomationLegacyIAccessiblePattern,
    IUIAutomationPropertyChangedEventHandler,
    IUIAutomationPropertyChangedEventHandler_Impl,
    IUIAutomationTogglePattern,
    IUIAutomationValuePattern,
    IUIAutomationWindowPattern,
    SetWinEventHook,
    TreeScope_Children,
    TreeScope_Subtree,
    UIA_AppBarControlTypeId,
    UIA_ButtonControlTypeId,
    UIA_CalendarControlTypeId,
    UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId,
    UIA_CustomControlTypeId,
    UIA_DataGridControlTypeId,
    UIA_DataItemControlTypeId,
    UIA_DocumentControlTypeId,
    UIA_EditControlTypeId,
    UIA_GroupControlTypeId,
    UIA_HeaderControlTypeId,
    UIA_HeaderItemControlTypeId,
    UIA_HyperlinkControlTypeId,
    UIA_ImageControlTypeId,
    UIA_InvokePatternId,
    UIA_Invoke_InvokedEventId,
    UIA_LegacyIAccessiblePatternId,
    UIA_ListControlTypeId,
    UIA_ListItemControlTypeId,
    UIA_MenuBarControlTypeId,
    UIA_MenuControlTypeId,
    UIA_MenuItemControlTypeId,
    UIA_PaneControlTypeId,
    UIA_ProgressBarControlTypeId,
    UIA_RadioButtonControlTypeId,
    UIA_ScrollBarControlTypeId,
    UIA_SelectionItem_ElementSelectedEventId,
    UIA_SemanticZoomControlTypeId,
    UIA_SeparatorControlTypeId,
    UIA_SliderControlTypeId,
    UIA_SpinnerControlTypeId,
    UIA_SplitButtonControlTypeId,
    UIA_StatusBarControlTypeId,
    UIA_TabControlTypeId,
    UIA_TabItemControlTypeId,
    UIA_TableControlTypeId,
    UIA_TextControlTypeId,
    UIA_ThumbControlTypeId,
    UIA_TitleBarControlTypeId,
    UIA_TogglePatternId,
    UIA_ToggleToggleStatePropertyId,
    UIA_ToolBarControlTypeId,
    UIA_ToolTipControlTypeId,
    UIA_TreeControlTypeId,
    UIA_TreeItemControlTypeId,
    UIA_ValuePatternId,
    UIA_ValueValuePropertyId,
    UIA_WindowControlTypeId,
    UIA_WindowPatternId,
    UnhookWinEvent,
    WindowVisualState,
    WindowVisualState_Maximized,
    WindowVisualState_Minimized,
    WindowVisualState_Normal,
    HWINEVENTHOOK,
    // Control type ids. A role comes from these rather than from the localized
    // display string, which changes with the system language.
    UIA_CONTROLTYPE_ID,
    UIA_EVENT_ID,
    UIA_PROPERTY_ID,
};
use windows::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
};
use windows::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, VIRTUAL_KEY,
    VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_LWIN, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DispatchMessageW, EnumWindows, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetGUIThreadInfo, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, PeekMessageW, SendMessageW,
    SetForegroundWindow, GA_ROOT, GUITHREADINFO, MSG, PM_REMOVE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, IsWindow, IsZoomed, PostMessageW, ShowWindow, GWL_STYLE, SHOW_WINDOW_CMD,
    SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, WM_CLOSE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
};
use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_NORMAL, WM_PASTE};

/// Make this process per-monitor (v2) DPI aware, once.
///
/// Every coordinate this backend handles -- UIA bounding rectangles, window
/// rectangles, the virtual-screen extent SendInput normalises against, and the
/// pixels a capture reads -- must be in one space. A DPI-unaware process gets
/// each of those virtualised to 96 DPI by a different rule: on a scaled display
/// a point is quantised (one logical pixel covers several physical ones), and
/// with monitors of different scales the virtualised rectangles of a window on
/// a secondary monitor no longer line up with the virtualised screen. Physical
/// pixels everywhere are the only space in which bounds, input and pixels agree.
///
/// The process-wide call fails harmlessly when awareness was already fixed (an
/// application manifest, or a host that set it first); [`dpi_awareness`] reports
/// what is actually in force.
fn ensure_dpi_awareness() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

/// The DPI awareness this thread runs under, by name.
fn dpi_awareness() -> &'static str {
    let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) };
    match awareness {
        DPI_AWARENESS_PER_MONITOR_AWARE => "per_monitor",
        DPI_AWARENESS_SYSTEM_AWARE => "system",
        DPI_AWARENESS_UNAWARE => "unaware",
        _ => "unknown",
    }
}

fn automation() -> Result<IUIAutomation> {
    ensure_dpi_awareness();
    ensure_com();
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
        .map_err(|error| DriverError::unavailable(format!("UI Automation is unavailable: {error}")))
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
                    if let Ok(children) = unsafe { element.FindAll(TreeScope_Children, condition) }
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
    fn open_capture(
        &self,
        window_id: &str,
        limit: usize,
    ) -> Result<Box<dyn crate::driver::CaptureSource>> {
        Ok(Box::new(CaptureSession::start(window_id, limit)?))
    }

    fn open_observation(
        &self,
        limit: usize,
    ) -> Result<Box<dyn crate::observe_session::ObserveSource>> {
        Ok(Box::new(crate::observer_win::ObserverSession::start(
            limit,
        )?))
    }

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
        unsafe { element.SetFocus() }.map_err(|error| {
            DriverError::new("DRIVER.ACTION_FAILED", format!("focus failed: {error}"))
        })
    }

    fn invoke(&self, window_id: &str, node: &Node) -> Result<()> {
        let element = self.require(window_id, node)?;

        // Prefer a real control pattern: it is far more reliable than input
        // synthesis and does not depend on the window being frontmost.
        if let Ok(pattern) = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        } {
            return unsafe { pattern.Invoke() }.map_err(|error| {
                DriverError::new("DRIVER.ACTION_FAILED", format!("invoke failed: {error}"))
                    .with_effect("unknown")
            });
        }
        if let Ok(pattern) = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
        } {
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
                DriverError::new(
                    "DRIVER.ACTION_FAILED",
                    format!("default action failed: {error}"),
                )
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
        let pattern =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
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
        //
        // The read-back has to wait, though. A control does not have to publish
        // its new value synchronously: measured on this machine, a native Win32
        // edit takes about 160 ms and a WebView input about 510 ms. Reading
        // once, immediately, called every one of those writes a failure.
        //
        // Polling costs nothing when the write worked -- it returns as soon as
        // the value moves -- and the full wait is only paid when the value
        // really is stuck, which is the case worth being sure about.
        // Assigned on the first iteration before it is read, so it needs no
        // initial value of its own.
        let mut after;
        let deadline = Instant::now() + VALUE_SETTLE_TIMEOUT;
        loop {
            after = unsafe { pattern.CurrentValue() }
                .ok()
                .map(|value| value.to_string());
            if settled(
                before.as_deref(),
                after.as_deref(),
                Instant::now() >= deadline,
            ) {
                break;
            }
            std::thread::sleep(VALUE_POLL_INTERVAL);
        }

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

    fn secret_input(&self, window_id: &str, node: &Node, text: &str) -> Result<()> {
        let element = self.require(window_id, node)?;
        if !unsafe { element.CurrentIsPassword() }.is_ok_and(|value| value.as_bool())
            || !unsafe { element.CurrentIsEnabled() }.is_ok_and(|value| value.as_bool())
        {
            return Err(DriverError::new(
                "DRIVER.PROTECTED_ELEMENT",
                "secret input requires an enabled protected field",
            ));
        }
        if let Ok(pattern) =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        {
            if !unsafe { pattern.CurrentIsReadOnly() }.is_ok_and(|value| !value.as_bool()) {
                return Err(DriverError::new(
                    "DRIVER.PROTECTED_ELEMENT",
                    "secret field is not editable",
                ));
            }
            // Never read CurrentValue or attach the requested value to errors.
            unsafe { pattern.SetValue(&BSTR::from(text)) }.map_err(|_| {
                DriverError::new("DRIVER.SECRET_INPUT_FAILED", "secret input failed")
                    .with_effect("unknown")
            })?;
            return Ok(());
        }
        if unsafe { element.CurrentControlType() }.ok() != Some(UIA_EditControlTypeId)
            || !unsafe { element.CurrentIsKeyboardFocusable() }.is_ok_and(|value| value.as_bool())
        {
            return Err(DriverError::new(
                "DRIVER.ACTION_UNSUPPORTED",
                "protected field has no replacement input path",
            ));
        }
        focus_for_keys(&self.automation, parse_window_id(window_id)?, &element)?;
        ensure_modifiers_released(&["ctrl".into(), "shift".into(), "alt".into(), "meta".into()])?;
        let mut inputs = Chord::parse(&["ctrl".into(), "a".into()])?.to_inputs();
        // Deleting the selection also gives empty secret text replace semantics.
        inputs.push(virtual_key_event(VK_BACK, false));
        inputs.push(virtual_key_event(VK_BACK, true));
        for unit in text.encode_utf16() {
            for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        deliver_atomically(&inputs, "secret input").map_err(|error| {
            let sent = error
                .details
                .get("events_sent")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize;
            release_pressed_prefix(&inputs, sent);
            DriverError::new("DRIVER.SECRET_INPUT_FAILED", "secret input failed")
                .with_effect(error.effect)
        })
    }

    fn type_text(&self, window_id: &str, node: &Node, text: &str) -> Result<()> {
        let element = self.require(window_id, node)?;
        // Typing goes to whatever holds focus, so focus must be established --
        // and proven, window in front and element focused -- or the keystrokes
        // land in another application.
        focus_for_keys(&self.automation, parse_window_id(window_id)?, &element)?;

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
        Ok(deliver_atomically(&inputs, "keystrokes")?)
    }

    fn type_text_keys(&self, window_id: &str, node: &Node, text: &str) -> Result<()> {
        // Build the complete sequence before focusing or sending anything. An
        // unsupported character therefore fails with effect=not_applied rather
        // than leaving a valid prefix in the target.
        let top_level = parse_window_id(window_id)?;
        let element = self.require(window_id, node)?;
        let input_window = unsafe { element.CurrentNativeWindowHandle() }
            .ok()
            .filter(|handle| !handle.0.is_null())
            .unwrap_or(top_level);
        let thread_id = unsafe { GetWindowThreadProcessId(input_window, None) };
        if thread_id == 0 {
            return Err(DriverError::unavailable(
                "the target element has no owning input thread",
            ));
        }
        let layout = unsafe { GetKeyboardLayout(thread_id) };
        let caps_lock_on = unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 != 0;
        let sequence = KeyTextSequence::parse(text, layout, caps_lock_on)?;
        focus_for_keys(&self.automation, top_level, &element)?;

        // UIA elements in hosted controls often report the top-level HWND, not
        // the child that owns the caret. After SetFocus, GetGUIThreadInfo is the
        // authoritative source for which HWND will receive keyboard input.
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let focus_window = if unsafe { GetGUIThreadInfo(thread_id, &mut gui) }.is_ok()
            && !gui.hwndFocus.0.is_null()
        {
            gui.hwndFocus
        } else {
            input_window
        };
        if ime_is_open(focus_window) {
            return Err(DriverError::new(
                "DRIVER.INPUT_METHOD_ACTIVE",
                "keys mode cannot guarantee exact text while the target input method is open",
            )
            // Focus changed, but no key was sent.
            .with_effect("contextual")
            .with_detail(
                "remedy",
                serde_json::json!(
                    "switch the target input method to direct/English mode, or use mode=unicode/set_value"
                ),
            ));
        }

        deliver_atomically(&sequence.inputs, "layout-aware keystrokes").map_err(|mut error| {
            let sent = error
                .details
                .get("events_sent")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize;
            let released = release_pressed_prefix(&sequence.inputs, sent);
            error = error.with_detail("residual_state_cleared", serde_json::json!(released));
            DriverError::from(error)
        })
    }

    fn key_chord(&self, window_id: &str, node: &Node, keys: &[String]) -> Result<()> {
        self.key_chord_with_deadline(
            window_id,
            node,
            keys,
            Instant::now() + Duration::from_secs(30),
        )
    }

    fn key_chord_with_deadline(
        &self,
        window_id: &str,
        node: &Node,
        keys: &[String],
        deadline: Instant,
    ) -> Result<()> {
        // A chord lands on whatever holds focus, exactly like typing, so the
        // element is focused first. Without this a shortcut meant for one field
        // would fire against wherever focus happened to be.
        let element = self.require(window_id, node)?;
        let chord = Chord::parse(keys)?;
        focus_for_keys(&self.automation, parse_window_id(window_id)?, &element)?;
        let inputs = chord.to_inputs();
        ensure_input_deadline(deadline)?;

        // Press-and-release must arrive as one stream: splitting it lets another
        // source slip between a modifier going down and the key it modifies. And
        // if the batch is refused, releasing the modifiers already pressed is not
        // optional -- a latched Ctrl silently turns the user's next keystrokes
        // into shortcuts, an effect that outlives this action (measured: it does
        // not self-heal). So a refusal is followed by a best-effort release of
        // every modifier the chord holds, and reported with that residual noted.
        deliver_atomically(&inputs, "the key combination").map_err(|mut error| {
            let released = chord.release_modifiers();
            error = error.with_detail("residual_state_cleared", serde_json::json!(released));
            DriverError::from(error)
        })
    }

    fn key_chord_held(
        &self,
        window_id: &str,
        node: &Node,
        keys: &[String],
        hold_ms: Option<u64>,
        deadline: Instant,
    ) -> Result<()> {
        let Some(hold_ms) = hold_ms else {
            return self.key_chord_with_deadline(window_id, node, keys, deadline);
        };
        ensure_hold_budget(hold_ms, deadline)?;
        let element = self.require_hold_target(window_id, node)?;
        let handle = parse_window_id(window_id)?;
        let chord = Chord::parse(keys)?;
        let inputs = chord.to_inputs();
        ensure_inputs_released(&inputs)?;
        focus_for_keys(&self.automation, handle, &element)?;
        self.require_hold_target(window_id, node)?;
        self.ensure_held_key_target(&element)?;
        let focus = input_focus_handle(handle)?;
        ensure_hold_budget(hold_ms, deadline)?;
        let mut held =
            HeldInput::new(deadline.min(Instant::now() + Duration::from_millis(hold_ms + 250)))?;
        let result = (|| {
            held.send(&inputs[..chord.modifiers.len() + 1], false, "held key down")?;
            wait_input_hold(hold_ms, deadline, || {
                ensure_hold_focus(handle, Some(focus))?;
                self.ensure_held_key_target(&element)
            })?;
            ensure_hold_focus(handle, Some(focus))?;
            held.send(
                &inputs[chord.modifiers.len() + 1..],
                false,
                "held key release",
            )?;
            Ok(())
        })();
        finish_held_input(&mut held, result)
    }

    fn pointer_click(&self, window_id: &str, node: &Node) -> Result<()> {
        let (handle, _) = self.element_for(window_id)?;
        let (x, y) = crate::backend::click_point(node.bounds)?;

        // Clicking a background window would deliver the click to whatever is
        // actually in front, so raise the target first.
        bring_to_front(handle);
        self.ensure_hits(window_id, node, (x, y))?;

        // The same normalisation every other gesture uses. This path used to
        // scale by the full width, which lands one pixel up/left of the point.
        let absolute = to_absolute((x, y))?;
        let inputs = [
            mouse_event(absolute, MOUSEEVENTF_MOVE),
            mouse_event(absolute, MOUSEEVENTF_LEFTDOWN),
            mouse_event(absolute, MOUSEEVENTF_LEFTUP),
        ];
        // A click is a move, a press and a release: only their order matters, so
        // if the atomic batch is refused, sending them individually is a
        // faithful fallback. Verified to work on a machine where the batch was
        // refused by an input hook.
        Ok(deliver_pointer(&inputs, &[], "the click")?)
    }

    fn pointer_gesture(
        &self,
        window_id: &str,
        node: &Node,
        gesture: &PointerGesture,
    ) -> Result<()> {
        let deadline = gesture
            .deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        ensure_input_deadline(deadline)?;
        let (handle, _) = self.element_for(window_id)?;
        let point = element_point(node.bounds, gesture.fraction)?;

        // Same reasoning as pointer_click: a background window would misroute
        // the event, so raise the target first.
        bring_to_front(handle);
        self.ensure_hits(window_id, node, point)?;
        let absolute = to_absolute(point)?;
        ensure_input_deadline(deadline)?;

        if let Some(scroll) = gesture.scroll {
            return Ok(deliver_scroll(absolute, scroll)?);
        }
        if let Some(dwell_ms) = gesture.hover_ms {
            let dwell = Duration::from_millis(dwell_ms.min(MAX_HOVER_MS));
            if deadline.saturating_duration_since(Instant::now()) < dwell {
                return Err(DriverError::new(
                    "DRIVER.TIMEOUT",
                    "hover exceeds the remaining input deadline",
                ));
            }
            // Hover is a move and a wait: the point must be reached, then held
            // long enough for a tooltip or hover menu to appear.
            let move_to = [mouse_event(absolute, MOUSEEVENTF_MOVE)];
            deliver_atomically(&move_to, "the hover move")?;
            std::thread::sleep(dwell);
            return Ok(());
        }

        let (down, up) = button_events(gesture.button);
        let clicks = gesture.count.max(1);
        let mut inputs = vec![mouse_event(absolute, MOUSEEVENTF_MOVE)];
        for _ in 0..clicks {
            inputs.push(mouse_event(absolute, down));
            inputs.push(mouse_event(absolute, up));
        }
        // A click is a move plus ordered press/release pairs; if the atomic
        // batch is refused, sending the events in order is a faithful fallback,
        // as with pointer_click.
        ensure_pointer_modifier_focus(handle, &gesture.modifiers)?;
        ensure_input_deadline(deadline)?;
        if let Some(hold_ms) = gesture.hold_ms {
            if clicks != 1 {
                return Err(DriverError::invalid("hold_ms requires a single click"));
            }
            return self.held_pointer(window_id, node, None, gesture, &inputs, hold_ms, deadline);
        }
        Ok(deliver_pointer(
            &inputs,
            &gesture.modifiers,
            "the pointer gesture",
        )?)
    }

    fn drag(
        &self,
        window_id: &str,
        from: &Node,
        to: &Node,
        gesture: &PointerGesture,
    ) -> Result<()> {
        let deadline = gesture
            .deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        ensure_input_deadline(deadline)?;
        if !gesture.path.is_empty() {
            return self.path_drag(window_id, from, to, gesture, deadline);
        }
        let (handle, _) = self.element_for(window_id)?;
        let start_point = element_point(from.bounds, gesture.fraction)?;
        let end_point = element_point(to.bounds, gesture.to_fraction)?;
        let start = to_absolute(start_point)?;
        let end = to_absolute(end_point)?;

        bring_to_front(handle);
        self.ensure_hits(window_id, from, start_point)?;
        self.ensure_hits(window_id, to, end_point)?;
        let (down, up) = button_events(gesture.button);

        // Press at the start, glide across in interpolated steps, release at the
        // end. The intermediate points are derived here, never supplied: many
        // targets only recognise a drag if the pointer visibly travels rather
        // than teleporting from press to release.
        let mut inputs = vec![
            mouse_event(start, MOUSEEVENTF_MOVE),
            mouse_event(start, down),
        ];
        for step in 1..DRAG_STEPS {
            let t = step as f64 / DRAG_STEPS as f64;
            let x = start.0 + ((end.0 - start.0) as f64 * t).round() as i32;
            let y = start.1 + ((end.1 - start.1) as f64 * t).round() as i32;
            inputs.push(mouse_event((x, y), MOUSEEVENTF_MOVE));
        }
        inputs.push(mouse_event(end, MOUSEEVENTF_MOVE));
        inputs.push(mouse_event(end, up));

        // Prefer one atomic batch. On this machine an input hook rejects every
        // multi-event batch before delivering anything, so a clean refusal may
        // degrade to ordered single events. Any partial failure triggers a
        // best-effort button release and remains effect=unknown.
        ensure_pointer_modifier_focus(handle, &gesture.modifiers)?;
        ensure_input_deadline(deadline)?;
        if let Some(hold_ms) = gesture.hold_ms {
            return self.held_pointer(
                window_id,
                from,
                Some(to),
                gesture,
                &inputs,
                hold_ms,
                deadline,
            );
        }
        Ok(deliver_pointer(&inputs, &gesture.modifiers, "the drag")?)
    }

    fn capture_pixels(&self, window_id: &str, node: &Node, whole_window: bool) -> Result<Pixels> {
        let handle = parse_window_id(window_id)?;
        if unsafe { IsIconic(handle) }.as_bool() {
            return Err(DriverError::new(
                "DRIVER.WINDOW_MINIMIZED",
                "a minimized window has nothing on screen to capture; restore it first",
            ));
        }
        let element = self.require(window_id, node)?;
        if unsafe { element.CurrentIsPassword() }.is_ok_and(|value| value.as_bool()) {
            return Err(DriverError::new(
                "DRIVER.PROTECTED_ELEMENT",
                "a password or otherwise protected element is never captured",
            ));
        }
        if !whole_window
            && unsafe { element.CurrentIsOffscreen() }.is_ok_and(|value| value.as_bool())
        {
            return Err(DriverError::new(
                "DRIVER.ACTION_UNSUPPORTED",
                "the element is scrolled or hidden out of view; bring it into view and observe again",
            ));
        }
        let mut window = RECT::default();
        unsafe { GetWindowRect(handle, &mut window) }.map_err(|error| {
            DriverError::new(
                "DRIVER.WINDOW_NOT_FOUND",
                format!("the window rectangle could not be read: {error}"),
            )
        })?;
        // Bounds are read live: the window may have moved since the snapshot.
        let wanted = if whole_window {
            extended_frame(handle).unwrap_or(window)
        } else {
            unsafe { element.CurrentBoundingRectangle() }.map_err(|error| {
                DriverError::stale(format!("the element's bounds could not be read: {error}"))
            })?
        };
        let region = intersect(wanted, window).ok_or_else(|| {
            DriverError::new(
                "DRIVER.ACTION_UNSUPPORTED",
                "the element lies outside its window's area and shows nothing to capture",
            )
        })?;
        let full = print_window(handle, &window)?;
        Ok(crop(&full, &window, &region))
    }

    fn window_action(
        &self,
        window_id: &str,
        node: &Node,
        action: &str,
    ) -> Result<serde_json::Value> {
        self.act_on_window(window_id, node, action)
    }

    fn recognize_text(&self, pixels: &Pixels, language: Option<&str>) -> Result<Recognition> {
        Ok(aad_windows::ocr::recognize(pixels, language)?)
    }

    fn clipboard_text(&self, max_chars: usize) -> Result<(Option<String>, usize)> {
        Ok(aad_windows::clipboard::text(max_chars)?)
    }

    fn global_key(&self, keys: &[String]) -> Result<serde_json::Value> {
        let chord = Chord::parse(keys)?;
        refuse_reserved_chord(&chord)?;
        // UAC prompts and the lock screen run on the secure desktop, which
        // synthetic input cannot and must not reach.
        if !on_default_desktop() {
            return Err(DriverError::new(
                "DRIVER.SECURE_DESKTOP",
                "the input desktop is not the user's desktop (a UAC prompt or the lock screen); nothing was sent",
            ));
        }
        let foreground = unsafe { GetForegroundWindow() };
        let mut process = 0u32;
        unsafe { GetWindowThreadProcessId(foreground, Some(&mut process)) };
        // The same atomic chord path and modifier-leak cleanup as `key`.
        deliver_atomically(&chord.to_inputs(), "the global key combination").map_err(
            |mut error| {
                let released = chord.release_modifiers();
                error = error.with_detail("residual_state_cleared", serde_json::json!(released));
                error
            },
        )?;
        Ok(serde_json::json!({
            "foreground_process_id": process,
        }))
    }

    fn paste(&self, window_id: &str, node: &Node, text: &str) -> Result<serde_json::Value> {
        self.paste_text(window_id, node, text)
    }

    fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "backend": "windows-uia",
            "platform": "windows",
            "dpi_awareness": dpi_awareness(),
        })
    }
}

/// How long to wait for a control to publish the value just written to it.
///
/// Measured on this machine: a native Win32 edit republishes in about 160 ms, a
/// WebView input in about 510 ms. A second is comfortably clear of both while
/// still bounding the wait for a control that genuinely ignored the write.
const VALUE_SETTLE_TIMEOUT: Duration = Duration::from_millis(1_000);

/// How often to re-read while waiting. Short enough that a fast control is not
/// held up, long enough not to spin.
const VALUE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Whether to stop waiting for a written value to appear.
///
/// Stop as soon as the value moves -- that is the answer, and a control that
/// republished quickly should not be made to wait out the timeout. Otherwise
/// keep looking until the deadline, because an unchanged value is only
/// meaningful once the control has had time to publish.
fn settled(before: Option<&str>, after: Option<&str>, expired: bool) -> bool {
    before != after || expired
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

fn ime_is_open(window: HWND) -> bool {
    // ImmGetContext cannot retrieve another process's context. The default IME
    // window is explicitly the cross-process query path: ask it for open status.
    const WM_IME_CONTROL: u32 = 0x0283;
    const IMC_GETOPENSTATUS: usize = 0x0005;
    let ime = unsafe { ImmGetDefaultIMEWnd(window) };
    if ime.0.is_null() {
        return false;
    }
    unsafe {
        SendMessageW(
            ime,
            WM_IME_CONTROL,
            windows::Win32::Foundation::WPARAM(IMC_GETOPENSTATUS),
            windows::Win32::Foundation::LPARAM(0),
        )
        .0 != 0
    }
}

/// How many interpolated moves a drag glides through between press and release.
/// Enough that targets see real travel, few enough to stay one tight batch.
const DRAG_STEPS: i32 = 12;

/// Upper bound on a hover dwell, so a caller cannot park the pointer for minutes.
const MAX_HOVER_MS: u64 = 5_000;

/// Compute a point inside the element's half-open bounding rectangle.
///
/// UI Automation bounds cover x..x+width and y..y+height, so the last pixel is
/// width-1 / height-1. Mapping 1.0 to the exclusive edge would target the next
/// control (or empty desktop) while still claiming the element was the anchor.
fn element_point(
    bounds: Option<crate::model::Bounds>,
    fraction: Option<(f64, f64)>,
) -> Result<(i32, i32)> {
    let bounds = bounds
        .ok_or_else(|| DriverError::invalid("the element has no bounding rectangle to act on"))?;
    if bounds.is_empty() {
        return Err(DriverError::invalid(
            "the element's bounding rectangle has no area",
        ));
    }
    match fraction {
        None => Ok(bounds.center()),
        Some((fx, fy)) => {
            let x = bounds.x + (((bounds.width - 1) as f64) * fx).round() as i32;
            let y = bounds.y + (((bounds.height - 1) as f64) * fy).round() as i32;
            Ok((x, y))
        }
    }
}

impl WindowsUiaBackend {
    fn path_drag(
        &self,
        window_id: &str,
        from: &Node,
        to: &Node,
        gesture: &PointerGesture,
        deadline: Instant,
    ) -> Result<()> {
        if gesture.path.len() > 32
            || from.node_id != to.node_id
            || gesture.fraction.is_none()
            || gesture.to_fraction.is_none()
        {
            return Err(DriverError::invalid(
                "drag path requires one surface and explicit endpoint fractions",
            ));
        }
        let mut fractions = vec![gesture.fraction.expect("explicit source")];
        fractions.extend_from_slice(&gesture.path);
        fractions.push(gesture.to_fraction.expect("explicit destination"));
        let handle = parse_window_id(window_id)?;
        bring_to_front(handle);
        let original = self.require_hold_target(window_id, from)?;
        let mut route = Vec::new();
        for fraction in fractions {
            let point = element_point(from.bounds, Some(fraction))?;
            self.ensure_hits(window_id, from, point)?;
            route.push((point, to_absolute(point)?));
        }
        let verify = |point| -> Result<()> {
            ensure_input_deadline(deadline)?;
            ensure_hold_focus(handle, None)?;
            ensure_pointer_modifier_focus(handle, &gesture.modifiers)?;
            let live = self.require_hold_target(window_id, from)?;
            if !unsafe { self.automation.CompareElements(&original, &live) }
                .is_ok_and(|same| same.as_bool())
            {
                return Err(DriverError::stale("drag path surface identity changed"));
            }
            self.ensure_hits(window_id, from, point)?;
            ensure_input_deadline(deadline)
        };
        verify(route[0].0)?;
        let dwell = gesture.hold_ms.unwrap_or(0);
        if dwell > 0 {
            ensure_hold_budget(dwell, deadline)?;
        }
        let budget = Duration::from_millis(dwell + (route.len() as u64) * 16 + 1000);
        let mut held = HeldInput::new(deadline.min(Instant::now() + budget))?;
        let (down, up) = button_events(gesture.button);
        let start = route[0].1;
        let prefix = modified_pointer_inputs(
            &[
                mouse_event(start, MOUSEEVENTF_MOVE),
                mouse_event(start, down),
            ],
            &gesture.modifiers,
        )?;
        // Refuse externally held input before entering the path; send checks
        // again under the watchdog lock immediately before each new down.
        ensure_inputs_released(&prefix[..gesture.modifiers.len() + 2])?;
        let result = (|| {
            held.send(
                &prefix[..gesture.modifiers.len() + 2],
                true,
                "drag path down",
            )?;
            if dwell > 0 {
                wait_input_hold(dwell, deadline, || verify(route[0].0))?;
            }
            for (point, absolute) in route.iter().skip(1) {
                verify(*point)?;
                held.send(
                    &[mouse_event(*absolute, MOUSEEVENTF_MOVE)],
                    false,
                    "drag path move",
                )?;
                wait_input_hold(16, deadline, || ensure_hold_focus(handle, None))?;
            }
            verify(route.last().expect("path endpoint").0)?;
            let suffix = modified_pointer_inputs(
                &[mouse_event(route.last().expect("path endpoint").1, up)],
                &gesture.modifiers,
            )?;
            held.send(
                &suffix[gesture.modifiers.len()..],
                true,
                "drag path release",
            )?;
            Ok(())
        })();
        finish_held_input(&mut held, result)
    }

    fn ensure_held_key_target(&self, element: &IUIAutomationElement) -> Result<()> {
        let focused = unsafe { self.automation.GetFocusedElement() }.map_err(|_| {
            DriverError::new("DRIVER.FOCUS_LOST", "could not verify the held key element")
        })?;
        if !unsafe { self.automation.CompareElements(element, &focused) }
            .is_ok_and(|same| same.as_bool())
            || !unsafe { element.CurrentIsEnabled() }.is_ok_and(|enabled| enabled.as_bool())
            || !unsafe { element.CurrentIsPassword() }.is_ok_and(|protected| !protected.as_bool())
        {
            return Err(DriverError::new(
                "DRIVER.FOCUS_LOST",
                "held key element lost focus or changed state",
            ));
        }
        Ok(())
    }

    fn require_hold_target(&self, window_id: &str, node: &Node) -> Result<IUIAutomationElement> {
        let element = self.require(window_id, node)?;
        let current = describe_element(&element, &node.node_id, node.depth, node.parent_id.clone());
        if current.role != node.role
            || current.name != node.name
            || current.automation_id != node.automation_id
            || current.class_name != node.class_name
            || current.bounds != node.bounds
            || current.states.enabled == Some(false)
            || current.states.protected == Some(true)
        {
            return Err(DriverError::stale(
                "held input target changed identity, geometry or state",
            ));
        }
        Ok(element)
    }

    #[allow(clippy::too_many_arguments)]
    fn held_pointer(
        &self,
        window_id: &str,
        from: &Node,
        to: Option<&Node>,
        gesture: &PointerGesture,
        inputs: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT],
        hold_ms: u64,
        deadline: Instant,
    ) -> Result<()> {
        ensure_hold_budget(hold_ms, deadline)?;
        let handle = parse_window_id(window_id)?;
        let source = self.require_hold_target(window_id, from)?;
        let destination = to
            .map(|node| self.require_hold_target(window_id, node))
            .transpose()?;
        let start = element_point(from.bounds, gesture.fraction)?;
        self.ensure_hits(window_id, from, start)?;
        let focus = if gesture.modifiers.is_empty() {
            None
        } else {
            Some(input_focus_handle(handle)?)
        };
        let mut rect = RECT::default();
        unsafe { GetWindowRect(handle, &mut rect) }
            .map_err(|_| DriverError::stale("held input window disappeared"))?;
        let combined = modified_pointer_inputs(inputs, &gesture.modifiers)?;
        ensure_inputs_released(&combined)?;
        ensure_hold_budget(hold_ms, deadline)?;
        // A stalled COM revalidation cannot prolong owned input indefinitely.
        // No later movement/down can pass the same lock after watchdog release.
        let mut held =
            HeldInput::new(deadline.min(Instant::now() + Duration::from_millis(hold_ms + 250)))?;
        let split = gesture.modifiers.len() + 2;
        let result = (|| {
            held.send(&combined[..split], true, "held pointer down")?;
            wait_input_hold(hold_ms, deadline, || {
                ensure_hold_focus(handle, focus)?;
                let mut current = RECT::default();
                unsafe { GetWindowRect(handle, &mut current) }
                    .map_err(|_| DriverError::stale("held input window disappeared"))?;
                if current != rect {
                    return Err(DriverError::stale("held input window geometry changed"));
                }
                Ok(())
            })?;
            let current = self.require_hold_target(window_id, from)?;
            if !unsafe { self.automation.CompareElements(&source, &current) }
                .is_ok_and(|same| same.as_bool())
            {
                return Err(DriverError::stale("held pointer source identity changed"));
            }
            self.ensure_hits(window_id, from, start)?;
            if let (Some(node), Some(original)) = (to, destination.as_ref()) {
                let current = self.require_hold_target(window_id, node)?;
                if !unsafe { self.automation.CompareElements(original, &current) }
                    .is_ok_and(|same| same.as_bool())
                {
                    return Err(DriverError::stale(
                        "held pointer destination identity changed",
                    ));
                }
                self.ensure_hits(
                    window_id,
                    node,
                    element_point(node.bounds, gesture.to_fraction)?,
                )?;
            }
            ensure_hold_focus(handle, focus)?;
            ensure_input_deadline(deadline)?;
            held.send(&combined[split..], true, "held pointer completion")?;
            Ok(())
        })();
        finish_held_input(&mut held, result)
    }

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
        self.locate(window_id, node)?
            .ok_or_else(|| DriverError::stale("the element is no longer present in the window"))
    }

    /// Refuse pointer input at `point` unless it would land on the element.
    ///
    /// Raising the window is best effort: Windows' foreground lock can refuse
    /// it, and then another window -- or a dialog, menu or overlay of the same
    /// application -- may cover the point. Sending the click anyway delivers
    /// it to whatever is there, which may be another application entirely
    /// (observed: a fixture click landed elsewhere after the foreground moved
    /// on). So the point is hit-tested first: what UIA finds there must be the
    /// element, something inside it, or a container of it (a label inside a
    /// button is often not hit-testable itself), never something beside it.
    fn ensure_hits(&self, window_id: &str, node: &Node, point: (i32, i32)) -> Result<()> {
        let target = self.require(window_id, node)?;
        let occluded = |what: &str| {
            DriverError::new(
                "DRIVER.OCCLUDED",
                format!("pointer input would land on {what}, not on the observed element"),
            )
            .with_detail("node_id", serde_json::json!(node.node_id))
        };
        let Ok(hit) = (unsafe {
            self.automation.ElementFromPoint(POINT {
                x: point.0,
                y: point.1,
            })
        }) else {
            return Err(occluded("nothing UI Automation can identify"));
        };
        let walker = unsafe { self.automation.RawViewWalker() }
            .map_err(|error| DriverError::unavailable(format!("no tree walker: {error}")))?;
        let same = |a: &IUIAutomationElement, b: &IUIAutomationElement| {
            unsafe { self.automation.CompareElements(a, b) }.is_ok_and(|same| same.as_bool())
        };
        // Is the hit the element or inside it?
        let mut current = Some(hit.clone());
        for _ in 0..CaptureLimits::MAX_DEPTH {
            let Some(element) = current else { break };
            if same(&element, &target) {
                return Ok(());
            }
            current = unsafe { walker.GetParentElement(&element) }.ok();
        }
        // Is the hit a container of the element (but not the desktop itself)?
        let desktop = unsafe { self.automation.GetRootElement() }.ok();
        let mut current = unsafe { walker.GetParentElement(&target) }.ok();
        for _ in 0..CaptureLimits::MAX_DEPTH {
            let Some(element) = current else { break };
            if desktop.as_ref().is_some_and(|root| same(&element, root)) {
                break;
            }
            if same(&element, &hit) {
                return Ok(());
            }
            current = unsafe { walker.GetParentElement(&element) }.ok();
        }
        // What covers it is not named: it may belong to another application.
        Err(occluded("another element"))
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
        result
            .ok()
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
    };
    let flag = |result: windows::core::Result<BOOL>| -> Option<bool> {
        result.ok().map(|value| value.as_bool())
    };

    // From the numeric control type, never the localized one.
    //
    // `CurrentLocalizedControlType` is a display string: it follows the system
    // language, and measurement on this machine caught it mid-flight -- one
    // snapshot reported 窗口 and 标题栏 for the frame while reporting edit,
    // button and text for the controls inside it, and an earlier run had those
    // same controls in Chinese. A locator holding "编辑" stops matching when the
    // provider decides to say "edit", so a recording made today fails tomorrow
    // for a reason nobody would think to look for.
    let role = control_type_name(unsafe { element.CurrentControlType() }.unwrap_or_default());

    let protected = flag(unsafe { element.CurrentIsPassword() });
    let value = if protected == Some(true) {
        None
    } else {
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
            .ok()
            .and_then(|pattern| unsafe { pattern.CurrentValue() }.ok())
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
    };

    let read_only =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
            .ok()
            .and_then(|pattern| unsafe { pattern.CurrentIsReadOnly() }.ok())
            .map(|value| value.as_bool());

    let bounds = unsafe { element.CurrentBoundingRectangle() }
        .ok()
        .and_then(|rect| {
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
        protected,
    };

    let mut actions = Vec::new();
    if states.focusable == Some(true) {
        actions.push("focus".to_string());
        // A key combination goes to whatever holds focus, so anything focusable
        // can receive one -- the same gate as ocus itself.
        if states.protected != Some(true) {
            actions.push("key_chord".to_string());
        }
    }
    let has = |id| unsafe { element.GetCurrentPattern(id) }.is_ok();
    if has(UIA_InvokePatternId) || has(UIA_TogglePatternId) || has(UIA_LegacyIAccessiblePatternId) {
        actions.push("invoke".to_string());
    }
    if states.protected == Some(true)
        && read_only != Some(true)
        && (has(UIA_ValuePatternId) || (role == "edit" && states.focusable == Some(true)))
    {
        actions.push("secret_input".to_string());
    }
    if has(UIA_ValuePatternId) && read_only != Some(true) && states.protected != Some(true) {
        actions.push("set_value".to_string());
        actions.push("type_text".to_string());
        // Pasting passes the text through the clipboard: never a password's.
        if states.focusable == Some(true) && states.protected != Some(true) {
            actions.push("paste".to_string());
        }
    }
    // The captured window's own node stands for the window. Its actions follow
    // what the window allows and the state it is in now.
    if depth == 0 && parent_id.is_none() {
        actions.extend(window_actions(element));
    }
    if bounds.is_some() && states.offscreen != Some(true) {
        actions.push("pointer_click".to_string());
        // Anything clickable can also take the richer pointer gestures (right
        // and middle click, double click, scroll, hover) and be a drag endpoint
        // -- all reach the same on-screen point the same way a click does.
        actions.push("pointer_gesture".to_string());
        actions.push("drag".to_string());
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

/// Modifier key events must reach the observed window too. A canvas need not
/// itself accept keyboard focus, but focus must be inside that same window.
fn ensure_pointer_modifier_focus(handle: HWND, modifiers: &[String]) -> Result<()> {
    if modifiers.is_empty() {
        return Ok(());
    }
    let foreground = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) };
    let thread = unsafe { GetWindowThreadProcessId(handle, None) };
    let mut gui = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let focused = unsafe { GetGUIThreadInfo(thread, &mut gui) }.is_ok()
        && !gui.hwndFocus.is_invalid()
        && unsafe { GetAncestor(gui.hwndFocus, GA_ROOT) } == handle;
    if foreground != handle || !focused {
        return Err(DriverError::new(
            "DRIVER.FOCUS_LOST",
            "pointer modifiers require keyboard focus in the observed window; no input was sent",
        ));
    }
    Ok(())
}

/// Focus `element` for keystrokes and prove they would reach it.
///
/// SendInput keystrokes go to the foreground window's focus, whatever UIA's
/// SetFocus did: if the foreground lock kept `handle` in the background, they
/// would land in another application. So the window is brought forward, the
/// element focused, and then both are checked before a single key is sent.
fn input_focus_handle(handle: HWND) -> Result<HWND> {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let thread = unsafe { GetWindowThreadProcessId(handle, None) };
    unsafe { GetGUIThreadInfo(thread, &mut info) }
        .map_err(|_| DriverError::new("DRIVER.FOCUS_LOST", "could not verify held input focus"))?;
    if info.hwndFocus.is_invalid() {
        return Err(DriverError::new(
            "DRIVER.FOCUS_LOST",
            "held input target has no native focus",
        ));
    }
    Ok(info.hwndFocus)
}

fn ensure_hold_focus(handle: HWND, focus: Option<HWND>) -> Result<()> {
    if unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) } != handle
        || focus.is_some_and(|expected| input_focus_handle(handle).ok() != Some(expected))
    {
        return Err(DriverError::new(
            "DRIVER.FOCUS_LOST",
            "held input target lost foreground or keyboard focus",
        ));
    }
    Ok(())
}

fn finish_held_input(held: &mut HeldInput, result: Result<()>) -> Result<()> {
    let submitted = held.submitted();
    let expired = held.expired();
    let released = held.release();
    let result = result.and_then(|()| {
        if expired {
            Err(DriverError::new(
                "DRIVER.TIMEOUT",
                "input watchdog released an expired hold",
            ))
        } else if !released {
            Err(DriverError::new(
                "DRIVER.ACTION_FAILED",
                "held input release was refused",
            ))
        } else {
            Ok(())
        }
    });
    result.map_err(|error| {
        error
            .with_effect(if submitted { "unknown" } else { "not_applied" })
            .with_detail("residual_state_cleared", serde_json::json!(released))
            .with_detail("hold_watchdog_expired", serde_json::json!(expired))
    })
}

fn focus_for_keys(
    automation: &IUIAutomation,
    handle: HWND,
    element: &IUIAutomationElement,
) -> Result<()> {
    bring_to_front(handle);
    unsafe { element.SetFocus() }.map_err(|error| {
        DriverError::new("DRIVER.ACTION_FAILED", format!("focus failed: {error}"))
    })?;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let foreground = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) };
        // Focus on the element or inside it (a container hands focus to a
        // child); UIA that cannot say where focus is is not held against it.
        let focused = match unsafe { automation.GetFocusedElement() } {
            Ok(mut current) => {
                let walker = unsafe { automation.RawViewWalker() }.ok();
                let mut inside = false;
                for _ in 0..CaptureLimits::MAX_DEPTH {
                    if unsafe { automation.CompareElements(&current, element) }
                        .is_ok_and(|same| same.as_bool())
                    {
                        inside = true;
                        break;
                    }
                    match walker
                        .as_ref()
                        .and_then(|walker| unsafe { walker.GetParentElement(&current) }.ok())
                    {
                        Some(parent) => current = parent,
                        None => break,
                    }
                }
                inside
            }
            Err(_) => true,
        };
        if foreground == handle && focused {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(DriverError::new(
                "DRIVER.FOCUS_LOST",
                "the element's window could not take keyboard focus; no key was sent",
            )
            .with_detail("window_in_front", serde_json::json!(foreground == handle)));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Chords a global key must never press: Ctrl+Alt+Del is the secure attention
/// sequence (Windows ignores it from SendInput, so pressing it would only
/// mislead), and Win+L locks the workstation, after which nothing -- not
/// even the person -- can be driven until someone signs in again.
fn refuse_reserved_chord(chord: &Chord) -> Result<()> {
    let has = |vk: VIRTUAL_KEY| chord.modifiers.contains(&vk);
    let reserved = (has(VK_CONTROL) && has(VK_MENU) && chord.key == VK_DELETE)
        || (has(VK_LWIN) && chord.key == VIRTUAL_KEY(b'L' as u16));
    if reserved {
        return Err(DriverError::new(
            "DRIVER.KEY_RESERVED",
            "Ctrl+Alt+Del and Win+L are never pressed",
        ));
    }
    Ok(())
}

/// Whether input currently goes to the interactive user's own desktop.
fn on_default_desktop() -> bool {
    let Ok(desktop) =
        (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) })
    else {
        // The secure desktop cannot be opened from a user process.
        return false;
    };
    let mut name = [0u16; 64];
    let mut needed = 0u32;
    let read = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    let _ = unsafe { CloseDesktop(desktop) };
    read.is_ok() && String::from_utf16_lossy(&name).trim_end_matches('\0') == "Default"
}

/// Make `handle` the foreground window, reporting whether it is.
///
/// `SetForegroundWindow` alone is refused by the foreground lock whenever this
/// process did not receive the last input event -- for instance after another
/// window closed and activation moved to some other application. Attaching to
/// the foreground thread's input state for the duration of the call is the
/// documented way a process that legitimately acts for the user hands focus
/// over; it sends no input. It can still fail (another integrity level, a
/// full-screen exclusive application), which the caller must handle.
fn bring_to_front(handle: HWND) -> bool {
    unsafe {
        if GetForegroundWindow() == handle {
            return true;
        }
        if SetForegroundWindow(handle).as_bool() && GetForegroundWindow() == handle {
            return true;
        }
        let foreground = GetForegroundWindow();
        let theirs = GetWindowThreadProcessId(foreground, None);
        let ours = GetCurrentThreadId();
        let attached =
            theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, TRUE).as_bool();
        let _ = BringWindowToTop(handle);
        let _ = SetForegroundWindow(handle);
        if attached {
            let _ = AttachThreadInput(ours, theirs, BOOL(0));
        }
        GetForegroundWindow() == handle
    }
}

// ---------------------------------------------------------------------------
// Screenshots
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Text recognition
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Paste (the clipboard itself is aad-windows')
// ---------------------------------------------------------------------------

/// How long the target may take to ask for the pasted text.
const PASTE_CONSUME_TIMEOUT: Duration = Duration::from_millis(2_000);

impl WindowsUiaBackend {
    /// Paste `text` into the element through the clipboard, then put the
    /// clipboard back as it was.
    ///
    /// The text is offered by delayed rendering: our window owns the
    /// clipboard and supplies the text only when someone asks for it. That
    /// request is the target pasting, so restoring waits for it (and for the
    /// target to close the clipboard) instead of guessing at a delay -- an
    /// early restore would paste the user's old clipboard instead. Anyone but
    /// the target process asking for the text -- clipboard history, managers,
    /// sync tools -- is refused, so the text reaches only the target.
    fn paste_text(&self, window_id: &str, node: &Node, text: &str) -> Result<serde_json::Value> {
        let element = self.require(window_id, node)?;
        if unsafe { element.CurrentIsPassword() }.is_ok_and(|value| value.as_bool()) {
            return Err(DriverError::new(
                "DRIVER.PROTECTED_ELEMENT",
                "a password field is never pasted into: the text would pass through the clipboard",
            ));
        }
        let chord = Chord::parse(&["ctrl".to_string(), "v".to_string()])?;
        let handle = parse_window_id(window_id)?;
        let mut target_process = 0u32;
        unsafe { GetWindowThreadProcessId(handle, Some(&mut target_process)) };
        let paste = Paste::prepare(text, target_process)?;
        // From the moment the clipboard is emptied, every way out restores it.
        let restore_on_failure = |error: DriverError| {
            let restored = paste.abandon();
            error.with_detail("clipboard_restored", serde_json::json!(restored))
        };
        if let Err(error) = paste.offer() {
            return Err(restore_on_failure(error.into()));
        }
        // A classic Win32 edit or rich edit is told to paste with WM_PASTE on
        // its own window: a message to the anchored control, which keyboard
        // hooks cannot swallow and which needs neither the foreground nor a
        // modifier key. (Measured: on a machine running keyboard-hooking
        // tools, Ctrl+V reached a focused fixture edit only one time in
        // three; WM_PASTE every time.) Everything else gets Ctrl+V.
        let native = unsafe { element.CurrentNativeWindowHandle() }.unwrap_or_default();
        let classic = !native.0.is_null() && {
            let class = class_name(native).to_ascii_lowercase();
            class == "edit" || class.starts_with("richedit")
        };
        let via = if classic {
            let _ = unsafe { element.SetFocus() };
            paste.asked();
            let mut result = 0usize;
            // While this waits, the edit's request for the text is answered:
            // a thread blocked in SendMessage still handles sent messages.
            let delivered = unsafe {
                SendMessageTimeoutW(
                    native,
                    WM_PASTE,
                    WPARAM(0),
                    LPARAM(0),
                    SMTO_NORMAL,
                    PASTE_CONSUME_TIMEOUT.as_millis() as u32,
                    Some(&mut result),
                )
            };
            if delivered.0 == 0 {
                return Err(restore_on_failure(
                    DriverError::new(
                        "DRIVER.ACTION_FAILED",
                        "the edit did not answer the paste request in time",
                    )
                    .with_effect("unknown"),
                ));
            }
            "wm_paste"
        } else {
            if let Err(error) = focus_for_keys(&self.automation, handle, &element) {
                return Err(restore_on_failure(error));
            }
            if let Err(error) = deliver_atomically(&chord.to_inputs(), "the paste shortcut") {
                let released = chord.release_modifiers();
                return Err(restore_on_failure(
                    error
                        .with_detail("residual_state_cleared", serde_json::json!(released))
                        .into(),
                ));
            }
            paste.asked();
            "ctrl_v"
        };
        let consumed = paste.consumed(PASTE_CONSUME_TIMEOUT);
        let refused = paste.refused();
        let restored = paste.restore();
        if !consumed {
            return Err(DriverError::new(
                "DRIVER.PASTE_NOT_CONSUMED",
                "Ctrl+V was sent but the element never asked for the clipboard's text",
            )
            // The shortcut landed on the element; what it did there is unknown.
            .with_effect("unknown")
            .with_detail("clipboard_restored", serde_json::json!(restored))
            .with_detail("via", serde_json::json!(via))
            .with_detail("other_readers_refused", serde_json::json!(refused)));
        }
        Ok(serde_json::json!({
            "consumed": true,
            "clipboard_restored": restored,
            "clipboard_formats_restored": paste.saved_formats(),
            "via": via,
            "other_readers_refused": refused,
        }))
    }
}

// ---------------------------------------------------------------------------
// Window actions
// ---------------------------------------------------------------------------

/// What a top-level window can do and where it stands, from UIA's
/// WindowPattern where the window has one and its Win32 styles otherwise.
struct WindowAbilities {
    can_minimize: bool,
    can_maximize: bool,
    state: WindowVisualState,
}

fn window_abilities(element: &IUIAutomationElement, handle: HWND) -> WindowAbilities {
    let pattern =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationWindowPattern>(UIA_WindowPatternId) }
            .ok();
    let style = unsafe { GetWindowLongW(handle, GWL_STYLE) } as u32;
    let state = if unsafe { IsIconic(handle) }.as_bool() {
        WindowVisualState_Minimized
    } else if unsafe { IsZoomed(handle) }.as_bool() {
        WindowVisualState_Maximized
    } else {
        WindowVisualState_Normal
    };
    let pattern_says = |read: &dyn Fn(&IUIAutomationWindowPattern) -> Option<bool>| {
        pattern.as_ref().and_then(read)
    };
    WindowAbilities {
        can_minimize: pattern_says(&|p| {
            unsafe { p.CurrentCanMinimize() }.ok().map(|v| v.as_bool())
        })
        .unwrap_or(style & WS_MINIMIZEBOX.0 != 0),
        can_maximize: pattern_says(&|p| {
            unsafe { p.CurrentCanMaximize() }.ok().map(|v| v.as_bool())
        })
        .unwrap_or(style & WS_MAXIMIZEBOX.0 != 0),
        state,
    }
}

/// The window actions a captured window's root node offers.
fn window_actions(element: &IUIAutomationElement) -> Vec<String> {
    let Ok(handle) = (unsafe { element.CurrentNativeWindowHandle() }) else {
        return Vec::new();
    };
    if handle.0.is_null() || unsafe { GetAncestor(handle, GA_ROOT) } != handle {
        return Vec::new();
    }
    let abilities = window_abilities(element, handle);
    let mut actions = vec!["activate".to_string(), "close_window".to_string()];
    if abilities.can_minimize && abilities.state != WindowVisualState_Minimized {
        actions.push("minimize".into());
    }
    if abilities.can_maximize && abilities.state != WindowVisualState_Maximized {
        actions.push("maximize".into());
    }
    if abilities.state != WindowVisualState_Normal {
        actions.push("restore".into());
    }
    actions
}

fn window_state_name(handle: HWND) -> &'static str {
    unsafe {
        if !IsWindow(handle).as_bool() {
            "closed"
        } else if IsIconic(handle).as_bool() {
            "minimized"
        } else if IsZoomed(handle).as_bool() {
            "maximized"
        } else {
            "normal"
        }
    }
}

/// How long a window action may take to show in the window's state.
const WINDOW_SETTLE: Duration = Duration::from_millis(1_500);

impl WindowsUiaBackend {
    fn act_on_window(
        &self,
        window_id: &str,
        node: &Node,
        action: &str,
    ) -> Result<serde_json::Value> {
        let handle = parse_window_id(window_id)?;
        let element = self.require(window_id, node)?;
        let live = unsafe { element.CurrentNativeWindowHandle() }.unwrap_or_default();
        if live != handle {
            return Err(DriverError::stale(
                "the observed node no longer stands for its window",
            ));
        }
        let pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationWindowPattern>(UIA_WindowPatternId)
        }
        .ok();
        let failed = |what: &str, error: windows::core::Error| {
            DriverError::new("DRIVER.ACTION_FAILED", format!("{what} failed: {error}"))
                .with_effect("unknown")
        };
        // UIA's WindowPattern first, as the window's own accessibility
        // contract; Win32 on the anchored HWND for windows without one.
        let visual = |state: WindowVisualState, command: SHOW_WINDOW_CMD| -> Result<()> {
            match &pattern {
                Some(pattern) => unsafe { pattern.SetWindowVisualState(state) }
                    .map_err(|error| failed("changing the window state", error)),
                None => {
                    unsafe {
                        let _ = ShowWindow(handle, command);
                    }
                    Ok(())
                }
            }
        };
        let reached: Box<dyn Fn() -> bool> = match action {
            "minimize" => {
                visual(WindowVisualState_Minimized, SW_MINIMIZE)?;
                Box::new(|| unsafe { IsIconic(handle) }.as_bool())
            }
            "maximize" => {
                visual(WindowVisualState_Maximized, SW_MAXIMIZE)?;
                Box::new(|| unsafe { IsZoomed(handle) }.as_bool())
            }
            "restore" => {
                visual(WindowVisualState_Normal, SW_RESTORE)?;
                Box::new(|| unsafe { !IsIconic(handle).as_bool() && !IsZoomed(handle).as_bool() })
            }
            "close_window" => {
                match &pattern {
                    Some(pattern) => unsafe { pattern.Close() }
                        .map_err(|error| failed("closing the window", error))?,
                    None => unsafe { PostMessageW(handle, WM_CLOSE, WPARAM(0), LPARAM(0)) }
                        .map_err(|error| failed("closing the window", error))?,
                }
                // A window may legitimately stay: "save changes?" is the
                // application's answer, not a failure of the request.
                let deadline = Instant::now() + WINDOW_SETTLE;
                while unsafe { IsWindow(handle) }.as_bool() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(25));
                }
                let closed = !unsafe { IsWindow(handle) }.as_bool();
                return Ok(serde_json::json!({
                    "state": window_state_name(handle),
                    "closed": closed,
                    "note": if closed { serde_json::Value::Null } else {
                        serde_json::json!("the window is still open; it may be asking to save or confirm -- observe it")
                    },
                }));
            }
            "activate" => {
                if unsafe { IsIconic(handle) }.as_bool() {
                    visual(WindowVisualState_Normal, SW_RESTORE)?;
                }
                if !bring_to_front(handle) {
                    return Err(DriverError::new(
                        "DRIVER.ACTIVATION_REFUSED",
                        "Windows did not let this process bring the window to the foreground",
                    )
                    // Restoring from minimized may have happened.
                    .with_effect("unknown")
                    .with_detail("state", serde_json::json!(window_state_name(handle))));
                }
                Box::new(|| unsafe { GetForegroundWindow() } == handle)
            }
            other => {
                return Err(DriverError::invalid(format!(
                    "unknown window action {other:?}"
                )))
            }
        };
        let deadline = Instant::now() + WINDOW_SETTLE;
        while !reached() {
            if Instant::now() >= deadline {
                return Err(DriverError::new(
                    "DRIVER.ACTION_FAILED",
                    format!("the window did not reach the requested state after {action}"),
                )
                .with_effect("unknown")
                .with_detail("state", serde_json::json!(window_state_name(handle))));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(serde_json::json!({
            "state": window_state_name(handle),
            "foreground": unsafe { GetForegroundWindow() } == handle,
        }))
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

// ---------------------------------------------------------------------------
// Watching a window for interactions
//
// Both native mechanisms are subscribed, because measurement showed each is
// blind where the other works (docs/plan/rust-port-status.md §2.17-2.18):
//
//   WinForms   UIA handlers saw 0 of 2 confirmed clicks; the hook saw 12 events
//              and resolved each to the exact control.
//   WebView    UIA handlers reported the button and the edit by name; the hook
//              could only reach the "Chrome Legacy Window" container.
//
// Subscribing to one would silently miss every interaction in a whole class of
// applications -- the failure this project exists to prevent.
// ---------------------------------------------------------------------------

/// MSAA event constants, from winuser.h.
const EVENT_OBJECT_FOCUS: u32 = 0x8005;
const EVENT_OBJECT_INVOKED: u32 = 0x8012;
const EVENT_OBJECT_VALUECHANGE: u32 = 0x800E;
const EVENT_OBJECT_STATECHANGE: u32 = 0x800A;
const EVENT_OBJECT_SELECTION: u32 = 0x8006;

/// Object ids worth reporting.
///
/// Measured: WinForms controls raise their events against OBJID_WINDOW, not
/// OBJID_CLIENT. A probe that accepted only OBJID_CLIENT discarded every event
/// and looked exactly like a window nobody had touched.
const OBJID_WINDOW: i32 = 0;
const OBJID_CLIENT: i32 = -4;

/// How often the pump wakes to check whether it has been asked to stop.
const PUMP_TICK: Duration = Duration::from_millis(20);

/// The UI Automation client object is documented as agile, so it may be used
/// from the callback threads as well as the one that created it.
struct AgileAutomation(IUIAutomation);
unsafe impl Send for AgileAutomation {}
unsafe impl Sync for AgileAutomation {}

/// What a hook callback needs in order to report an event.
///
/// A WinEvent hook is a bare function pointer with no user data, so the
/// callback reaches its session through a thread-local -- set on the pump
/// thread, which is the only thread the callback runs on.
struct HookContext {
    buffer: Arc<EventBuffer>,
    automation: AgileAutomation,
    target: isize,
}

thread_local! {
    static HOOK_CONTEXT: std::cell::RefCell<Option<HookContext>> =
        const { std::cell::RefCell::new(None) };
}

/// Translate an MSAA event into the vocabulary the recorder speaks.
///
/// Returns `None` for events that are not interactions. Being explicit about
/// which are ignored keeps the noise out of recordings: a single click can
/// raise several focus and repaint notifications.
fn msaa_event_kind(event: u32) -> Option<EventKind> {
    match event {
        EVENT_OBJECT_INVOKED => Some(EventKind::Invoked),
        EVENT_OBJECT_VALUECHANGE => Some(EventKind::ValueChanged),
        EVENT_OBJECT_STATECHANGE => Some(EventKind::StateChanged),
        EVENT_OBJECT_SELECTION => Some(EventKind::StateChanged),
        EVENT_OBJECT_FOCUS => Some(EventKind::Focused),
        _ => None,
    }
}

/// Whether an event belongs to the window being watched.
///
/// The hook is desktop-wide -- there is no per-window variant that also covers
/// child controls -- so everything else has to be filtered out here.
fn belongs_to_window(hwnd: HWND, target: isize) -> bool {
    if hwnd.0 as isize == target {
        return true;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    root.0 as isize == target
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // Never let a panic cross back into the operating system's callback.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if id_object != OBJID_WINDOW && id_object != OBJID_CLIENT {
            return;
        }
        let Some(kind) = msaa_event_kind(event) else {
            return;
        };
        HOOK_CONTEXT.with(|slot| {
            let borrowed = slot.borrow();
            let Some(context) = borrowed.as_ref() else {
                return;
            };
            if !belongs_to_window(hwnd, context.target) {
                return;
            }
            // The hook hands over a window handle, not an element. For real
            // windowed controls this resolves to the control itself; for web
            // content it reaches only the container, which is why the UIA
            // handlers are subscribed as well.
            let node = unsafe { context.automation.0.ElementFromHandle(hwnd) }
                .ok()
                .map(|element| describe_element(&element, "captured", 0, None));
            context.buffer.push(kind, node, "win_event");
        });
    }));
}

#[implement(IUIAutomationEventHandler)]
struct UiaEventHandler {
    buffer: Arc<EventBuffer>,
}

impl IUIAutomationEventHandler_Impl for UiaEventHandler_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        id: UIA_EVENT_ID,
    ) -> windows::core::Result<()> {
        let kind = match id {
            UIA_Invoke_InvokedEventId => EventKind::Invoked,
            UIA_SelectionItem_ElementSelectedEventId => EventKind::StateChanged,
            _ => return Ok(()),
        };
        let node = sender.map(|element| describe_element(element, "captured", 0, None));
        self.buffer.push(kind, node, "uia");
        Ok(())
    }
}

#[implement(IUIAutomationPropertyChangedEventHandler)]
struct UiaPropertyHandler {
    buffer: Arc<EventBuffer>,
}

impl IUIAutomationPropertyChangedEventHandler_Impl for UiaPropertyHandler_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        property: UIA_PROPERTY_ID,
        _value: &windows::core::VARIANT,
    ) -> windows::core::Result<()> {
        let kind = match property {
            UIA_ValueValuePropertyId => EventKind::ValueChanged,
            UIA_ToggleToggleStatePropertyId => EventKind::StateChanged,
            _ => return Ok(()),
        };
        let node = sender.map(|element| describe_element(element, "captured", 0, None));
        self.buffer.push(kind, node, "uia");
        Ok(())
    }
}

/// A live subscription to one window's interactions.
///
/// Owns a thread because a WinEvent hook is delivered to the message queue of
/// the thread that installed it, so something has to pump messages for the
/// lifetime of the subscription. The UIA handlers do not need the pump, but
/// keeping every subscription on one thread means one place to tear them all
/// down.
pub struct CaptureSession {
    buffer: Arc<EventBuffer>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// What the subscription achieved, so a caller can be told when only half
    /// of it worked rather than being left to wonder why events are missing.
    pub sources: Vec<&'static str>,
}

impl CaptureSession {
    /// Begin watching a window.
    ///
    /// Fails only if neither mechanism could be installed. If one succeeds the
    /// session starts and `sources` says which -- partial coverage is worth
    /// having, but the caller is told, because a missing source means a whole
    /// class of interaction will go unrecorded.
    pub fn start(window_id: &str, limit: usize) -> Result<Self> {
        let hwnd = parse_window_id(window_id)?;
        let target = hwnd.0 as isize;
        let buffer = EventBuffer::new(limit);
        let stop = Arc::new(AtomicBool::new(false));

        let (ready, started) = channel::<std::result::Result<Vec<&'static str>, String>>();
        let worker_buffer = Arc::clone(&buffer);
        let worker_stop = Arc::clone(&stop);

        let worker = std::thread::Builder::new()
            .name("aad-capture".to_string())
            .spawn(move || {
                pump(target, worker_buffer, worker_stop, ready);
            })
            .map_err(|error| {
                DriverError::unavailable(format!("could not start a capture thread: {error}"))
            })?;

        // Wait for the subscription to be established before returning, so a
        // caller that starts capturing and immediately interacts does not race
        // the setup and silently lose those first events.
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(sources)) => Ok(Self {
                buffer,
                stop,
                worker: Some(worker),
                sources,
            }),
            Ok(Err(message)) => {
                stop.store(true, Ordering::SeqCst);
                let _ = worker.join();
                Err(DriverError::unavailable(message))
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                Err(DriverError::unavailable(
                    "the capture thread did not start watching within five seconds",
                ))
            }
        }
    }

    /// Take the interactions observed so far, oldest first.
    pub fn drain(&self, max: usize) -> (Vec<CapturedEvent>, u64) {
        self.buffer.drain(max)
    }

    pub fn pending(&self) -> usize {
        self.buffer.pending()
    }
}

impl crate::driver::CaptureSource for CaptureSession {
    fn drain(&self, max: usize) -> (Vec<CapturedEvent>, u64) {
        self.buffer.drain(max)
    }

    fn sources(&self) -> Vec<String> {
        self.sources.iter().map(|name| name.to_string()).collect()
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            // The pump wakes every PUMP_TICK, so this returns promptly. Joining
            // matters because the hook must be removed before the process that
            // owns the callback goes away.
            let _ = worker.join();
        }
    }
}

/// Install both subscriptions, then pump messages until asked to stop.
fn pump(
    target: isize,
    buffer: Arc<EventBuffer>,
    stop: Arc<AtomicBool>,
    ready: Sender<std::result::Result<Vec<&'static str>, String>>,
) {
    ensure_com();
    let automation = match automation() {
        Ok(automation) => automation,
        Err(error) => {
            let _ = ready.send(Err(format!("UI Automation is unavailable: {error:?}")));
            return;
        }
    };

    let hwnd = HWND(target as *mut core::ffi::c_void);
    let mut sources: Vec<&'static str> = Vec::new();

    // --- UI Automation handlers: the only ones that see inside web content ---
    // Held for the whole function so the subscriptions outlive setup.
    let mut uia_handlers: Option<(
        IUIAutomationElement,
        IUIAutomationEventHandler,
        IUIAutomationPropertyChangedEventHandler,
    )> = None;

    if let Ok(root) = unsafe { automation.ElementFromHandle(hwnd) } {
        let event_handler: IUIAutomationEventHandler = UiaEventHandler {
            buffer: Arc::clone(&buffer),
        }
        .into();
        let property_handler: IUIAutomationPropertyChangedEventHandler = UiaPropertyHandler {
            buffer: Arc::clone(&buffer),
        }
        .into();

        let mut installed = true;
        for id in [
            UIA_Invoke_InvokedEventId,
            UIA_SelectionItem_ElementSelectedEventId,
        ] {
            if unsafe {
                automation.AddAutomationEventHandler(
                    id,
                    &root,
                    TreeScope_Subtree,
                    None,
                    &event_handler,
                )
            }
            .is_err()
            {
                installed = false;
            }
        }
        if unsafe {
            automation.AddPropertyChangedEventHandlerNativeArray(
                &root,
                TreeScope_Subtree,
                None,
                &property_handler,
                &[UIA_ValueValuePropertyId, UIA_ToggleToggleStatePropertyId],
            )
        }
        .is_err()
        {
            installed = false;
        }

        if installed {
            sources.push("uia");
        }
        uia_handlers = Some((root, event_handler, property_handler));
    }

    // --- WinEvent hook: the only one that sees WinForms clicks ---------------
    HOOK_CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(HookContext {
            buffer: Arc::clone(&buffer),
            automation: AgileAutomation(automation.clone()),
            target,
        });
    });

    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_INVOKED,
            None,
            Some(on_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if !hook.is_invalid() {
        sources.push("win_event");
    }

    if sources.is_empty() {
        let _ = ready.send(Err(
            "neither UI Automation events nor WinEvent hooks could be installed for this window"
                .to_string(),
        ));
        return;
    }
    let _ = ready.send(Ok(sources));

    // WinEvents are posted to this thread's queue; without a pump they are
    // never delivered.
    while !stop.load(Ordering::SeqCst) {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe { DispatchMessageW(&message) };
        }
        std::thread::sleep(PUMP_TICK);
    }

    if !hook.is_invalid() {
        let _ = unsafe { UnhookWinEvent(hook) };
    }
    if let Some((root, event_handler, property_handler)) = uia_handlers {
        for id in [
            UIA_Invoke_InvokedEventId,
            UIA_SelectionItem_ElementSelectedEventId,
        ] {
            let _ = unsafe { automation.RemoveAutomationEventHandler(id, &root, &event_handler) };
        }
        let _ = unsafe { automation.RemovePropertyChangedEventHandler(&root, &property_handler) };
    }
    HOOK_CONTEXT.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

/// The stable name of a UIA control type.
///
/// These names are the contract a saved locator relies on, so they are spelled
/// out here rather than taken from whatever the platform feels like displaying.
/// An unrecognised type keeps its number, which is still stable and still
/// comparable -- much better than collapsing everything unknown into one name
/// that then matches unrelated elements.
fn control_type_name(control_type: UIA_CONTROLTYPE_ID) -> String {
    let name = match control_type {
        UIA_ButtonControlTypeId => "button",
        UIA_CalendarControlTypeId => "calendar",
        UIA_CheckBoxControlTypeId => "check_box",
        UIA_ComboBoxControlTypeId => "combo_box",
        UIA_EditControlTypeId => "edit",
        UIA_HyperlinkControlTypeId => "hyperlink",
        UIA_ImageControlTypeId => "image",
        UIA_ListItemControlTypeId => "list_item",
        UIA_ListControlTypeId => "list",
        UIA_MenuControlTypeId => "menu",
        UIA_MenuBarControlTypeId => "menu_bar",
        UIA_MenuItemControlTypeId => "menu_item",
        UIA_ProgressBarControlTypeId => "progress_bar",
        UIA_RadioButtonControlTypeId => "radio_button",
        UIA_ScrollBarControlTypeId => "scroll_bar",
        UIA_SliderControlTypeId => "slider",
        UIA_SpinnerControlTypeId => "spinner",
        UIA_StatusBarControlTypeId => "status_bar",
        UIA_TabControlTypeId => "tab",
        UIA_TabItemControlTypeId => "tab_item",
        UIA_TextControlTypeId => "text",
        UIA_ToolBarControlTypeId => "tool_bar",
        UIA_ToolTipControlTypeId => "tool_tip",
        UIA_TreeControlTypeId => "tree",
        UIA_TreeItemControlTypeId => "tree_item",
        UIA_CustomControlTypeId => "custom",
        UIA_GroupControlTypeId => "group",
        UIA_ThumbControlTypeId => "thumb",
        UIA_DataGridControlTypeId => "data_grid",
        UIA_DataItemControlTypeId => "data_item",
        UIA_DocumentControlTypeId => "document",
        UIA_SplitButtonControlTypeId => "split_button",
        UIA_WindowControlTypeId => "window",
        UIA_PaneControlTypeId => "pane",
        UIA_HeaderControlTypeId => "header",
        UIA_HeaderItemControlTypeId => "header_item",
        UIA_TableControlTypeId => "table",
        UIA_TitleBarControlTypeId => "title_bar",
        UIA_SeparatorControlTypeId => "separator",
        UIA_SemanticZoomControlTypeId => "semantic_zoom",
        UIA_AppBarControlTypeId => "app_bar",
        _ => return format!("control_type_{}", control_type.0),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_one_stays_inside_the_element() {
        let bounds = Bounds {
            x: 100,
            y: 200,
            width: 10,
            height: 20,
        };

        assert_eq!(
            element_point(Some(bounds), Some((0.0, 0.0))).unwrap(),
            (100, 200)
        );
        assert_eq!(
            element_point(Some(bounds), Some((1.0, 1.0))).unwrap(),
            (109, 219)
        );
    }

    #[test]
    fn the_secure_attention_sequence_and_locking_are_never_pressed() {
        let chord = |keys: &[&str]| {
            Chord::parse(&keys.iter().map(|k| k.to_string()).collect::<Vec<_>>()).unwrap()
        };
        for reserved in [&["ctrl", "alt", "delete"][..], &["win", "l"], &["Win", "L"]] {
            assert_eq!(
                refuse_reserved_chord(&chord(reserved)).unwrap_err().code,
                "DRIVER.KEY_RESERVED"
            );
        }
        for allowed in [
            &["win", "d"][..],
            &["alt", "tab"],
            &["ctrl", "delete"],
            &["l"],
        ] {
            assert!(refuse_reserved_chord(&chord(allowed)).is_ok());
        }
    }

    #[test]
    fn the_backend_runs_per_monitor_dpi_aware() {
        // Bounds, input and captured pixels only agree in physical pixels.
        let _backend = WindowsUiaBackend::new().expect("UIA is available");
        assert_eq!(dpi_awareness(), "per_monitor");
    }

    // -----------------------------------------------------------------------
    // Confirming a write actually landed
    //
    // Measured: SetValue on an HTML <select> in a WebView returns S_OK and
    // changes nothing. Reporting that as applied is worse than an error --
    // an error can be handled, a false success cannot.
    // -----------------------------------------------------------------------

    #[test]
    fn waiting_stops_the_moment_the_value_changes() {
        // A control that republishes quickly must not be held to the timeout.
        assert!(settled(Some("old"), Some("new"), false));
    }

    #[test]
    fn waiting_continues_while_the_value_has_not_moved_yet() {
        // The bug this fixes: reading once, immediately, called a good write a
        // failure. Measured here, a WebView takes about 510 ms to republish.
        assert!(!settled(Some("old"), Some("old"), false));
    }

    #[test]
    fn waiting_gives_up_once_the_deadline_passes() {
        // Otherwise a control that genuinely ignores writes would hang forever.
        assert!(settled(Some("old"), Some("old"), true));
    }

    #[test]
    fn a_value_that_appears_from_nothing_counts_as_movement() {
        // An empty field that becomes readable has changed, even though the
        // first read returned nothing.
        assert!(settled(None, Some("new"), false));
    }

    #[test]
    fn an_unreadable_value_waits_rather_than_deciding_early() {
        // No evidence yet is not the same as no change; let the deadline
        // decide, and `write_was_refused` treats it as not-a-refusal anyway.
        assert!(!settled(None, None, false));
    }

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
    fn a_role_never_depends_on_the_display_language() {
        // Roles used to come from CurrentLocalizedControlType, and measurement
        // caught that changing under us: one snapshot of the same window
        // reported 窗口 and 标题栏 for the frame while reporting edit, button
        // and text for the controls inside it, and an earlier run had those
        // controls in Chinese too. A saved locator holding "编辑" silently stops
        // matching when the provider switches to "edit".
        for (control_type, expected) in [
            (UIA_ButtonControlTypeId, "button"),
            (UIA_EditControlTypeId, "edit"),
            (UIA_CheckBoxControlTypeId, "check_box"),
            (UIA_WindowControlTypeId, "window"),
            (UIA_TitleBarControlTypeId, "title_bar"),
            (UIA_TextControlTypeId, "text"),
        ] {
            let name = control_type_name(control_type);
            assert_eq!(name, expected);
            assert!(
                name.is_ascii(),
                "a role has to be language-independent, got {name:?}"
            );
        }
    }

    #[test]
    fn an_unknown_control_type_keeps_its_identity() {
        // Collapsing everything unrecognised into one name would make unrelated
        // elements match each other, which is worse than an ugly name: a
        // locator would start selecting the wrong control rather than failing.
        let first = control_type_name(UIA_CONTROLTYPE_ID(59999));
        let second = control_type_name(UIA_CONTROLTYPE_ID(59998));

        assert_ne!(first, second);
        assert!(first.contains("59999"), "{first}");
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
        let ids: Vec<&str> = captured
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect();
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
        let limits = CaptureLimits {
            max_depth: 4,
            max_nodes: 5,
        };
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
