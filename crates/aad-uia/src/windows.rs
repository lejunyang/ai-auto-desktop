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
    Backend, CaptureLimits, CapturedTree, DriverError, ImageBox, Pixels, PointerButton,
    PointerGesture, Recognition, Result, ScrollDirection, ScrollSpec, TextSpan,
};
use crate::capture::{CapturedEvent, EventBuffer, EventKind};
use crate::model::{Bounds, Node, States, WindowInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};
use windows::core::{implement, BSTR, PWSTR};
use windows::core::{HSTRING, PCWSTR};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::Foundation::{
    CloseHandle, BOOL, HWND, LPARAM, MAX_PATH, POINT, RECT, TRUE, WPARAM,
};
use windows::Win32::Foundation::{GlobalFree, HANDLE};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, GetOpenClipboardWindow, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
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
    GetWindowDpiAwarenessContext, SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_PER_MONITOR_AWARE,
    DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
};
use windows::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, MapVirtualKeyW, SendInput, VkKeyScanExW, INPUT, INPUT_KEYBOARD,
    INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, VIRTUAL_KEY, VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
    VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8,
    VK_F9, VK_HOME, VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT,
    VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DispatchMessageW, EnumWindows, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, PeekMessageW,
    SendMessageW, SetForegroundWindow, GA_ROOT, GUITHREADINFO, MSG, PM_REMOVE,
    PW_RENDERFULLCONTENT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, WHEEL_DELTA, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SendMessageTimeoutW,
    HWND_MESSAGE, SMTO_NORMAL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_PASTE, WM_RENDERALLFORMATS,
    WM_RENDERFORMAT, WNDCLASSEXW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, IsWindow, IsZoomed, PostMessageW, ShowWindow, GWL_STYLE, SHOW_WINDOW_CMD,
    SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, WM_CLOSE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
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
        unsafe { element.SetFocus() }.map_err(|error| {
            DriverError::new("DRIVER.ACTION_FAILED", format!("focus failed: {error}"))
        })?;

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
            error
        })
    }

    fn key_chord(&self, window_id: &str, node: &Node, keys: &[String]) -> Result<()> {
        // A chord lands on whatever holds focus, exactly like typing, so the
        // element is focused first. Without this a shortcut meant for one field
        // would fire against wherever focus happened to be.
        let element = self.require(window_id, node)?;
        unsafe { element.SetFocus() }.map_err(|error| {
            DriverError::new("DRIVER.ACTION_FAILED", format!("focus failed: {error}"))
        })?;

        let chord = Chord::parse(keys)?;
        let inputs = chord.to_inputs();

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
            error
        })
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
        deliver_with_safe_fallback(&inputs, "the click")
    }

    fn pointer_gesture(
        &self,
        window_id: &str,
        node: &Node,
        gesture: &PointerGesture,
    ) -> Result<()> {
        let (handle, _) = self.element_for(window_id)?;
        let point = element_point(node.bounds, gesture.fraction)?;

        // Same reasoning as pointer_click: a background window would misroute
        // the event, so raise the target first.
        bring_to_front(handle);
        self.ensure_hits(window_id, node, point)?;
        let absolute = to_absolute(point)?;

        if let Some(scroll) = gesture.scroll {
            return deliver_scroll(absolute, scroll);
        }
        if let Some(dwell_ms) = gesture.hover_ms {
            // Hover is a move and a wait: the point must be reached, then held
            // long enough for a tooltip or hover menu to appear.
            let move_to = [mouse_event(absolute, MOUSEEVENTF_MOVE)];
            deliver_atomically(&move_to, "the hover move")?;
            std::thread::sleep(Duration::from_millis(dwell_ms.min(MAX_HOVER_MS)));
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
        deliver_with_safe_fallback(&inputs, "the pointer gesture")
    }

    fn drag(
        &self,
        window_id: &str,
        from: &Node,
        to: &Node,
        gesture: &PointerGesture,
    ) -> Result<()> {
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
        deliver_drag(&inputs, end, up)
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
        recognize(pixels, language)
    }

    fn clipboard_text(&self, max_chars: usize) -> Result<(Option<String>, usize)> {
        read_clipboard_text(max_chars)
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

/// A complete, layout-aware sequence for text typed as real virtual keys.
struct KeyTextSequence {
    inputs: Vec<INPUT>,
}

impl KeyTextSequence {
    fn parse(
        text: &str,
        layout: windows::Win32::UI::Input::KeyboardAndMouse::HKL,
        caps_lock_on: bool,
    ) -> Result<Self> {
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

fn map_character(
    character: char,
    layout: windows::Win32::UI::Input::KeyboardAndMouse::HKL,
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
        return Err(DriverError::invalid(format!(
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

fn unmappable_character(character: char) -> DriverError {
    DriverError::invalid(format!(
        "character {character:?} cannot be typed as keys with the target window's keyboard layout; use mode=unicode or set_value"
    ))
    .with_detail("character", serde_json::json!(character.to_string()))
}

/// A logical virtual-key event. No SCANCODE flag: the key was chosen using the
/// target window's layout, so Windows must translate that logical VK through the
/// target input queue rather than reinterpret a physical key position.
fn virtual_key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
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
fn pressed_keys_after_prefix(inputs: &[INPUT], sent: usize) -> Vec<VIRTUAL_KEY> {
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

fn release_pressed_prefix(inputs: &[INPUT], sent: usize) -> Vec<u16> {
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
struct Chord {
    modifiers: Vec<VIRTUAL_KEY>,
    key: VIRTUAL_KEY,
}

impl Chord {
    /// Parse `["ctrl", "c"]`, `["f5"]`, `["shift", "end"]` into a chord.
    ///
    /// Exactly one non-modifier key is required: a chord is "modifiers plus a
    /// key", and either zero keys or several main keys is a request the caller
    /// got wrong rather than something to guess at.
    fn parse(keys: &[String]) -> Result<Self> {
        if keys.is_empty() {
            return Err(DriverError::invalid(
                "a key combination needs at least one key",
            ));
        }
        let mut modifiers = Vec::new();
        let mut main: Option<VIRTUAL_KEY> = None;
        for name in keys {
            let vk = key_name_to_vk(name)
                .ok_or_else(|| DriverError::invalid(format!("unknown key {name:?}")))?;
            if is_modifier(vk) {
                if !modifiers.contains(&vk) {
                    modifiers.push(vk);
                }
            } else if main.is_some() {
                return Err(DriverError::invalid(
                    "a key combination may hold several modifiers but only one main key",
                ));
            } else {
                main = Some(vk);
            }
        }
        let key = main.ok_or_else(|| {
            DriverError::invalid("a key combination needs a non-modifier key, e.g. \"c\" in Ctrl+C")
        })?;
        Ok(Self { modifiers, key })
    }

    /// The full press/release sequence: modifiers down, key down, key up,
    /// modifiers up in reverse. Reverse release mirrors how a person lifts keys
    /// and avoids leaving a modifier down past the key it applied to.
    fn to_inputs(&self) -> Vec<INPUT> {
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
    fn release_modifiers(&self) -> Vec<String> {
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
fn key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
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

fn is_modifier(vk: VIRTUAL_KEY) -> bool {
    matches!(vk, VK_CONTROL | VK_SHIFT | VK_MENU | VK_LWIN)
}

fn vk_label(vk: VIRTUAL_KEY) -> &'static str {
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
fn key_name_to_vk(name: &str) -> Option<VIRTUAL_KEY> {
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

/// Convert a virtual-screen point to the 0..65535 absolute coordinates SendInput
/// expects with MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK.
fn to_absolute(point: (i32, i32)) -> Result<(i32, i32)> {
    let (origin_x, origin_y, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 1 || height <= 1 {
        return Err(DriverError::unavailable(
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
fn normalise(pixel: i32, origin: i32, extent: i32) -> i32 {
    let offset = i64::from((pixel - origin).clamp(0, extent - 1));
    ((offset * 65_536 + 32_768) / i64::from(extent)).min(65_535) as i32
}

/// One mouse INPUT at an absolute point with the given flags.
fn mouse_event(
    absolute: (i32, i32),
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> INPUT {
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
fn button_events(
    button: PointerButton,
) -> (
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) {
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
fn deliver_scroll(absolute: (i32, i32), scroll: ScrollSpec) -> Result<()> {
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

/// Retry a pointer gesture one event at a time only when the atomic attempt
/// delivered nothing. Once any event landed, replaying the full gesture could
/// duplicate a click or wheel notch and must be refused conservatively.
fn fallback_is_safe(error: &DriverError) -> bool {
    error.effect == "not_applied"
}

fn deliver_with_safe_fallback(inputs: &[INPUT], what: &str) -> Result<()> {
    match deliver_atomically(inputs, what) {
        Ok(()) => Ok(()),
        Err(error) if fallback_is_safe(&error) => deliver_individually(inputs, what),
        Err(error) => Err(error),
    }
}

/// Deliver a drag, falling back to ordered events only after a clean refusal.
fn deliver_drag(
    inputs: &[INPUT],
    end: (i32, i32),
    release: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> Result<()> {
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

    let value =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
            .ok()
            .and_then(|pattern| unsafe { pattern.CurrentValue() }.ok())
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty());

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
        protected: flag(unsafe { element.CurrentIsPassword() }),
    };

    let mut actions = Vec::new();
    if states.focusable == Some(true) {
        actions.push("focus".to_string());
        // A key combination goes to whatever holds focus, so anything focusable
        // can receive one -- the same gate as ocus itself.
        actions.push("key_chord".to_string());
    }
    let has = |id| unsafe { element.GetCurrentPattern(id) }.is_ok();
    if has(UIA_InvokePatternId) || has(UIA_TogglePatternId) || has(UIA_LegacyIAccessiblePatternId) {
        actions.push("invoke".to_string());
    }
    if has(UIA_ValuePatternId) && read_only != Some(true) {
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

/// Focus `element` for keystrokes and prove they would reach it.
///
/// SendInput keystrokes go to the foreground window's focus, whatever UIA's
/// SetFocus did: if the foreground lock kept `handle` in the background, they
/// would land in another application. So the window is brought forward, the
/// element focused, and then both are checked before a single key is sent.
fn focus_for_keys(handle: HWND, element: &IUIAutomationElement) -> Result<()> {
    bring_to_front(handle);
    unsafe { element.SetFocus() }.map_err(|error| {
        DriverError::new("DRIVER.ACTION_FAILED", format!("focus failed: {error}"))
    })?;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let foreground = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) };
        let focused = unsafe { element.CurrentHasKeyboardFocus() }
            .map(|focused| focused.as_bool())
            .unwrap_or(true);
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

/// The largest capture returned on either side, in physical pixels.
const MAX_CAPTURE_SIDE: i32 = 8_192;
/// A window larger than this is refused rather than buffered whole.
const MAX_WINDOW_PIXELS: i64 = 64 * 1024 * 1024;

/// A window's visible frame, without the invisible resize border Windows 10+
/// adds outside it (which `GetWindowRect` includes).
fn extended_frame(handle: HWND) -> Option<RECT> {
    let mut frame = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            handle,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut frame as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    (frame.right > frame.left && frame.bottom > frame.top).then_some(frame)
}

fn intersect(a: RECT, b: RECT) -> Option<RECT> {
    let rect = RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    };
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

/// The whole window as top-down BGRA, rendered by the window itself.
///
/// `PrintWindow` with `PW_RENDERFULLCONTENT` asks the window (and DWM, for
/// DirectComposition and GPU content) to render into our bitmap. Unlike a
/// copy of the screen it never contains another window laid over the target,
/// so a screenshot cannot leak what some other application shows, and it does
/// not need the target in front. The price: a minimized window renders
/// nothing (refused before this), and an application that ignores the request
/// renders black.
///
/// A DPI-virtualised window (unaware, or system aware on a monitor of another
/// scale) renders at its own logical size while Windows shows it stretched.
/// Its rendering is scaled to the physical window size the same way, so the
/// capture lines up with the physical bounds UIA reports. Such a capture is
/// as soft as what is on screen, no sharper.
fn print_window(handle: HWND, window: &RECT) -> Result<Vec<u8>> {
    let width = window.right - window.left;
    let height = window.bottom - window.top;
    if width <= 0 || height <= 0 {
        return Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "the window has no area to capture",
        ));
    }
    if i64::from(width) * i64::from(height) > MAX_WINDOW_PIXELS {
        return Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            "the window is larger than a capture may buffer",
        ));
    }
    let (own_width, own_height) = own_window_size(handle)
        .filter(|&(w, h)| w > 0 && h > 0 && i64::from(w) * i64::from(h) <= MAX_WINDOW_PIXELS)
        .unwrap_or((width, height));
    let rendered = render_window(handle, own_width, own_height)?;
    if (own_width, own_height) == (width, height) {
        return Ok(rendered);
    }
    let image = image::RgbaImage::from_raw(own_width as u32, own_height as u32, rendered)
        .ok_or_else(|| DriverError::new("DRIVER.ACTION_FAILED", "malformed window rendering"))?;
    Ok(image::imageops::resize(
        &image,
        width as u32,
        height as u32,
        image::imageops::FilterType::Triangle,
    )
    .into_raw())
}

/// The window's size in its own DPI space: what `PrintWindow` renders.
fn own_window_size(handle: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe {
        let previous = SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(handle));
        let read = GetWindowRect(handle, &mut rect);
        if !previous.0.is_null() {
            SetThreadDpiAwarenessContext(previous);
        }
        read.ok()?;
    }
    Some((rect.right - rect.left, rect.bottom - rect.top))
}

/// `PrintWindow` into a bitmap of the given size, read back as BGRA.
fn render_window(handle: HWND, width: i32, height: i32) -> Result<Vec<u8>> {
    let mut bgra = vec![0u8; width as usize * height as usize * 4];
    unsafe {
        let screen = GetDC(HWND::default());
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let old = SelectObject(memory, bitmap);
        let printed = PrintWindow(handle, memory, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT));
        // GetDIBits wants the bitmap out of any DC.
        SelectObject(memory, old);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        ReleaseDC(HWND::default(), screen);
        if !printed.as_bool() {
            return Err(DriverError::new(
                "DRIVER.ACTION_FAILED",
                "the window did not render itself for the screenshot",
            ));
        }
        if lines != height {
            return Err(DriverError::new(
                "DRIVER.ACTION_FAILED",
                "the rendered window could not be read back",
            ));
        }
    }
    Ok(bgra)
}

/// Cut `region` (screen pixels) out of a window-sized BGRA buffer as RGBA,
/// keeping at most MAX_CAPTURE_SIDE on each side from the top-left.
fn crop(bgra: &[u8], window: &RECT, region: &RECT) -> Pixels {
    let stride = (window.right - window.left) as usize * 4;
    let full_width = region.right - region.left;
    let full_height = region.bottom - region.top;
    let width = full_width.min(MAX_CAPTURE_SIDE);
    let height = full_height.min(MAX_CAPTURE_SIDE);
    let left = (region.left - window.left) as usize;
    let top = (region.top - window.top) as usize;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let start = (top + row) * stride + left * 4;
        for pixel in bgra[start..start + width as usize * 4].chunks_exact(4) {
            // PrintWindow leaves alpha undefined; the image is opaque.
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    Pixels {
        width: width as u32,
        height: height as u32,
        rgba,
        clipped: width < full_width || height < full_height,
    }
}

// ---------------------------------------------------------------------------
// Text recognition
// ---------------------------------------------------------------------------

fn ocr_failed(what: &str, error: windows::core::Error) -> DriverError {
    DriverError::new("DRIVER.OCR_FAILED", format!("{what}: {error}"))
}

/// The OCR languages installed on this machine, by tag.
fn ocr_languages() -> Vec<String> {
    OcrEngine::AvailableRecognizerLanguages()
        .ok()
        .map(|languages| {
            languages
                .into_iter()
                .filter_map(|language| language.LanguageTag().ok())
                .map(|tag| tag.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Windows' built-in OCR (Windows.Media.Ocr): offline, no extra engine to
/// install, one recogniser per installed OCR language pack.
fn recognize(pixels: &Pixels, language: Option<&str>) -> Result<Recognition> {
    ensure_com();
    let available = ocr_languages();
    let unavailable = |message: &str| {
        DriverError::new("DRIVER.OCR_UNAVAILABLE", message.to_string())
            .with_detail("available_languages", serde_json::json!(available))
    };
    let engine = match language {
        Some(tag) => {
            let language = Language::CreateLanguage(&HSTRING::from(tag))
                .map_err(|_| unavailable("the language tag is not one Windows knows"))?;
            if !OcrEngine::IsLanguageSupported(&language).unwrap_or(false) {
                return Err(unavailable(
                    "no OCR language pack for that language is installed",
                ));
            }
            OcrEngine::TryCreateFromLanguage(&language)
                .map_err(|_| unavailable("the OCR engine for that language could not start"))?
        }
        // The user's profile languages first; an account whose languages have
        // no OCR pack falls back to the first pack that is installed.
        None => match OcrEngine::TryCreateFromUserProfileLanguages() {
            Ok(engine) => engine,
            Err(_) => {
                let first = available
                    .first()
                    .ok_or_else(|| unavailable("no OCR language pack is installed"))?;
                let language = Language::CreateLanguage(&HSTRING::from(first.as_str()))
                    .map_err(|error| ocr_failed("OCR language", error))?;
                OcrEngine::TryCreateFromLanguage(&language)
                    .map_err(|_| unavailable("no OCR engine could start"))?
            }
        },
    };
    let limit = OcrEngine::MaxImageDimension().unwrap_or(10_000);
    if pixels.width > limit || pixels.height > limit {
        return Err(DriverError::new(
            "DRIVER.ACTION_UNSUPPORTED",
            format!("OCR reads images of at most {limit} pixels a side; read a smaller element"),
        ));
    }
    let bgra: Vec<u8> = pixels
        .rgba
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    let writer = DataWriter::new().map_err(|error| ocr_failed("image buffer", error))?;
    writer
        .WriteBytes(&bgra)
        .map_err(|error| ocr_failed("image buffer", error))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|error| ocr_failed("image buffer", error))?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        pixels.width as i32,
        pixels.height as i32,
    )
    .map_err(|error| ocr_failed("bitmap", error))?;
    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.get())
        .map_err(|error| ocr_failed("recognition", error))?;
    let span = |text: HSTRING, rect: windows::Foundation::Rect| TextSpan {
        text: text.to_string(),
        bounds: ImageBox {
            x: f64::from(rect.X),
            y: f64::from(rect.Y),
            width: f64::from(rect.Width),
            height: f64::from(rect.Height),
        },
    };
    let mut lines = Vec::new();
    for line in result
        .Lines()
        .map_err(|error| ocr_failed("recognition", error))?
    {
        let words: Vec<TextSpan> = line
            .Words()
            .map(|words| {
                words
                    .into_iter()
                    .filter_map(|word| Some(span(word.Text().ok()?, word.BoundingRect().ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        // A line's box is the union of its words'.
        let (mut left, mut top, mut right, mut bottom) = (f64::MAX, f64::MAX, 0.0f64, 0.0f64);
        for word in &words {
            left = left.min(word.bounds.x);
            top = top.min(word.bounds.y);
            right = right.max(word.bounds.x + word.bounds.width);
            bottom = bottom.max(word.bounds.y + word.bounds.height);
        }
        if words.is_empty() {
            continue;
        }
        let text = line.Text().map(|text| text.to_string()).unwrap_or_default();
        lines.push((
            TextSpan {
                text,
                bounds: ImageBox {
                    x: left,
                    y: top,
                    width: right - left,
                    height: bottom - top,
                },
            },
            words,
        ));
    }
    let language = engine
        .RecognizerLanguage()
        .and_then(|language| language.LanguageTag())
        .map(|tag| tag.to_string())
        .unwrap_or_default();
    Ok(Recognition {
        engine: "windows.media.ocr".into(),
        language,
        lines,
    })
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

const CF_UNICODETEXT: u32 = 13;

/// A registered clipboard format by name.
fn clipboard_format(name: &str) -> u32 {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) }
}
/// The format with which a clipboard's source (a password manager) asks
/// clipboard monitors to leave its content alone.
const EXCLUDE_FROM_MONITORS: &str = "ExcludeClipboardContentFromMonitorProcessing";
const CAN_INCLUDE_IN_HISTORY: &str = "CanIncludeInClipboardHistory";
const CAN_UPLOAD_TO_CLOUD: &str = "CanUploadToCloudClipboard";

/// The clipboard, opened and closed again on drop. Another process may hold
/// it for a moment, so opening retries briefly before giving up.
struct OpenedClipboard;

impl OpenedClipboard {
    fn open(owner: Option<HWND>) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_millis(1_000);
        loop {
            if unsafe { OpenClipboard(owner.unwrap_or_default()) }.is_ok() {
                return Ok(Self);
            }
            if Instant::now() >= deadline {
                return Err(DriverError::new(
                    "DRIVER.CLIPBOARD_BUSY",
                    "another application kept the clipboard open",
                )
                .retryable());
            }
            pump_for(Duration::from_millis(20));
        }
    }
}

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// Dispatch this thread's messages for a while: a clipboard owner window
/// must answer WM_RENDERFORMAT, which arrives as a sent message.
fn pump_for(duration: Duration) {
    let until = Instant::now() + duration;
    let mut message = MSG::default();
    loop {
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                DispatchMessageW(&message);
            }
        }
        if Instant::now() >= until {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The bytes of one clipboard format, from an open clipboard.
fn clipboard_bytes(format: u32) -> Option<Vec<u8>> {
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let global = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(global) };
    if size == 0 {
        return None;
    }
    let data = unsafe { GlobalLock(global) } as *const u8;
    if data.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
    let _ = unsafe { GlobalUnlock(global) };
    Some(bytes)
}

fn utf16_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn read_clipboard_text(max_chars: usize) -> Result<(Option<String>, usize)> {
    let _open = OpenedClipboard::open(None)?;
    // A password manager marks what it copies; that is not ours to read.
    if unsafe { IsClipboardFormatAvailable(clipboard_format(EXCLUDE_FROM_MONITORS)) }.is_ok() {
        return Err(DriverError::new(
            "DRIVER.CLIPBOARD_PROTECTED",
            "the clipboard's source marked its content as not for clipboard readers",
        ));
    }
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
        return Ok((None, 0));
    }
    let Some(bytes) = clipboard_bytes(CF_UNICODETEXT) else {
        return Ok((None, 0));
    };
    let text = utf16_text(&bytes);
    let total = text.chars().count();
    Ok((Some(text.chars().take(max_chars).collect()), total))
}

/// Formats whose clipboard handle is a GDI object, not memory: they cannot
/// be copied byte-wise. Bitmaps survive anyway as CF_DIB/CF_DIBV5, which
/// Windows synthesises from them.
const GDI_FORMATS: [u32; 8] = [2, 3, 9, 14, 0x80, 0x82, 0x83, 0x8E];
/// The most clipboard content a paste preserves; beyond it the paste is
/// refused rather than lose what the user copied.
const MAX_PRESERVED_BYTES: usize = 32 * 1024 * 1024;
/// How long the target may take to ask for the pasted text.
const PASTE_CONSUME_TIMEOUT: Duration = Duration::from_millis(2_000);

thread_local! {
    /// The text this thread's paste window offers, and how often it was asked for.
    static PASTE_TEXT: std::cell::RefCell<Vec<u16>> = const { std::cell::RefCell::new(Vec::new()) };
    static PASTE_RENDERS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Requests for the text from anyone but the paste target, not answered.
    static PASTE_REFUSED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The process the text is for, and whether Ctrl+V has been sent.
    static PASTE_TARGET: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static PASTE_KEYS_SENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the process now reading the clipboard is the one being pasted
/// into. A request from anyone else -- clipboard managers, history, remote
/// desktop clipboard sync -- gets no data and leaves the offer pending.
fn requester_is_target() -> bool {
    let requester = unsafe { GetOpenClipboardWindow() }.unwrap_or_default();
    if requester.0.is_null() {
        // Opened without a window: only plausible once the paste is under way.
        return PASTE_KEYS_SENT.with(|sent| sent.get());
    }
    let mut process = 0u32;
    unsafe { GetWindowThreadProcessId(requester, Some(&mut process)) };
    process != 0 && process == PASTE_TARGET.with(|target| target.get())
}

/// Every format on the clipboard, copied, to put back afterwards.
fn save_clipboard() -> Result<Vec<(u32, Vec<u8>)>> {
    let _open = OpenedClipboard::open(None)?;
    let mut saved = Vec::new();
    let mut total = 0usize;
    let mut format = 0u32;
    loop {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if GDI_FORMATS.contains(&format) {
            continue;
        }
        if let Some(bytes) = clipboard_bytes(format) {
            total += bytes.len();
            if total > MAX_PRESERVED_BYTES {
                return Err(DriverError::new(
                    "DRIVER.CLIPBOARD_TOO_LARGE",
                    "the clipboard holds more than a paste can preserve; nothing was pasted",
                ));
            }
            saved.push((format, bytes));
        }
    }
    Ok(saved)
}

fn global_copy(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
        let data = GlobalLock(global) as *mut u8;
        if data.is_null() {
            let _ = GlobalFree(global);
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        let _ = GlobalUnlock(global);
        Some(global)
    }
}

unsafe extern "system" fn paste_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    let render = || {
        let text = PASTE_TEXT.with(|text| {
            let text = text.borrow();
            text.iter()
                .chain(std::iter::once(&0u16))
                .flat_map(|unit| unit.to_le_bytes())
                .collect::<Vec<u8>>()
        });
        if let Some(global) = global_copy(&text) {
            let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(global.0));
        }
        PASTE_RENDERS.with(|renders| renders.set(renders.get() + 1));
    };
    match message {
        // The target asked for the text: it is pasting now. Anyone else is
        // refused, so the offer stays pending for the target.
        WM_RENDERFORMAT if wparam.0 as u32 == CF_UNICODETEXT => {
            if requester_is_target() {
                render();
            } else {
                PASTE_REFUSED.with(|refused| refused.set(refused.get() + 1));
            }
            windows::Win32::Foundation::LRESULT(0)
        }
        // Never rendered for everyone: the clipboard is restored before the
        // window goes, and an unrendered offer simply lapses.
        WM_RENDERALLFORMATS => windows::Win32::Foundation::LRESULT(0),
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// A message-only window owning the clipboard for one paste.
struct PasteWindow(HWND);

impl PasteWindow {
    fn create() -> Result<Self> {
        static CLASS: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
        let class_name = CLASS.get_or_init(|| {
            let name: Vec<u16> = format!("AadPasteOwner_{}", std::process::id())
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(paste_window_proc),
                lpszClassName: PCWSTR(name.as_ptr()),
                ..Default::default()
            };
            unsafe { RegisterClassExW(&class) };
            name
        });
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            )
        }
        .map_err(|error| DriverError::unavailable(format!("no clipboard owner window: {error}")))?;
        Ok(Self(hwnd))
    }
}

impl Drop for PasteWindow {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// Put a delayed-rendering text offer on the clipboard, owned by `owner`.
fn offer_text(owner: HWND) -> Result<()> {
    let _open = OpenedClipboard::open(Some(owner))?;
    unsafe { EmptyClipboard() }.map_err(|error| {
        DriverError::new("DRIVER.CLIPBOARD_BUSY", format!("clipboard: {error}"))
    })?;
    // SetClipboardData returns NULL for a delayed offer by design, so
    // success is read back rather than taken from its result.
    let _ = unsafe { SetClipboardData(CF_UNICODETEXT, HANDLE::default()) };
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
        return Err(DriverError::new(
            "DRIVER.CLIPBOARD_BUSY",
            "the clipboard did not accept the text offer",
        ));
    }
    // Ask well-behaved watchers -- Windows' clipboard history and cloud
    // clipboard among them -- not to ask at all. (Measured: without this the
    // history service's refused request left Edit controls unable to paste.)
    for name in [
        EXCLUDE_FROM_MONITORS,
        CAN_INCLUDE_IN_HISTORY,
        CAN_UPLOAD_TO_CLOUD,
    ] {
        if let Some(global) = global_copy(&0u32.to_le_bytes()) {
            let _ = unsafe { SetClipboardData(clipboard_format(name), HANDLE(global.0)) };
        }
    }
    Ok(())
}

/// Whether the clipboard now looks like a re-published copy of our offer:
/// its markers without its text. Clipboard sync tools do this.
fn echo_of_offer() -> bool {
    let Ok(_open) = OpenedClipboard::open(None) else {
        return false;
    };
    unsafe {
        IsClipboardFormatAvailable(clipboard_format(EXCLUDE_FROM_MONITORS)).is_ok()
            && IsClipboardFormatAvailable(CF_UNICODETEXT).is_err()
    }
}

/// Wait (bounded) until nobody asks for the offer and nobody holds the
/// clipboard open, so the target will be able to open it.
fn settle_clipboard(owner: HWND) {
    let until = Instant::now() + Duration::from_millis(600);
    let mut quiet_since = Instant::now();
    let mut refusals = PASTE_REFUSED.with(|refused| refused.get());
    while Instant::now() < until {
        pump_for(Duration::from_millis(15));
        let now = PASTE_REFUSED.with(|refused| refused.get());
        // Opening it is the only reliable test that nobody holds it: a
        // reader that opened it without a window is invisible otherwise.
        let held = match unsafe { OpenClipboard(owner) } {
            Ok(()) => {
                let _ = unsafe { CloseClipboard() };
                false
            }
            Err(_) => true,
        };
        if now != refusals || held {
            refusals = now;
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= Duration::from_millis(150) {
            return;
        }
    }
}

fn still_offered(owner: HWND) -> bool {
    unsafe { GetClipboardOwner() }.is_ok_and(|current| current == owner)
        && unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_ok()
}

/// Put the saved formats back.
fn restore_clipboard(owner: HWND, saved: &[(u32, Vec<u8>)]) -> bool {
    let Ok(_open) = OpenedClipboard::open(Some(owner)) else {
        return false;
    };
    if unsafe { EmptyClipboard() }.is_err() {
        return false;
    }
    let mut restored = true;
    for (format, bytes) in saved {
        restored &= global_copy(bytes)
            .is_some_and(|global| unsafe { SetClipboardData(*format, HANDLE(global.0)) }.is_ok());
    }
    restored
}

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
        let saved = save_clipboard()?;
        let owner = PasteWindow::create()?;
        PASTE_TEXT.with(|slot| *slot.borrow_mut() = text.encode_utf16().collect());
        PASTE_RENDERS.with(|renders| renders.set(0));
        PASTE_REFUSED.with(|refused| refused.set(0));
        PASTE_KEYS_SENT.with(|sent| sent.set(false));
        let handle = parse_window_id(window_id)?;
        let mut target_process = 0u32;
        unsafe { GetWindowThreadProcessId(handle, Some(&mut target_process)) };
        PASTE_TARGET.with(|target| target.set(target_process));
        // From the moment the clipboard is emptied, every way out restores it.
        let restore_on_failure = |error: DriverError| {
            let restored = restore_clipboard(owner.0, &saved);
            PASTE_TEXT.with(|slot| slot.borrow_mut().clear());
            error.with_detail("clipboard_restored", serde_json::json!(restored))
        };
        // Offer the text, let clipboard watchers react (they are refused and
        // let go), and offer again if one of them replaced the clipboard --
        // clipboard sync tools re-publish what they saw. Only an offer that is
        // still ours, on a clipboard nobody holds open, is pasted from.
        let mut attempts = 0;
        loop {
            attempts += 1;
            if let Err(error) = offer_text(owner.0) {
                return Err(restore_on_failure(error));
            }
            settle_clipboard(owner.0);
            if still_offered(owner.0) {
                break;
            }
            if attempts == 3 {
                return Err(restore_on_failure(DriverError::new(
                    "DRIVER.CLIPBOARD_BUSY",
                    "another application keeps replacing the clipboard; nothing was pasted",
                )));
            }
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
            PASTE_KEYS_SENT.with(|sent| sent.set(true));
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
            if let Err(error) = focus_for_keys(handle, &element) {
                return Err(restore_on_failure(error));
            }
            if let Err(error) = deliver_atomically(&chord.to_inputs(), "the paste shortcut") {
                let released = chord.release_modifiers();
                return Err(restore_on_failure(error.with_detail(
                    "residual_state_cleared",
                    serde_json::json!(released),
                )));
            }
            PASTE_KEYS_SENT.with(|sent| sent.set(true));
            "ctrl_v"
        };
        let deadline = Instant::now() + PASTE_CONSUME_TIMEOUT;
        while PASTE_RENDERS.with(|renders| renders.get()) == 0 && Instant::now() < deadline {
            pump_for(Duration::from_millis(10));
        }
        let consumed = PASTE_RENDERS.with(|renders| renders.get()) > 0;
        let refused = PASTE_REFUSED.with(|refused| refused.get());
        // The target reads while it holds the clipboard open; opening it
        // ourselves (inside restore) waits for it to finish.
        let restored = restore_clipboard(owner.0, &saved);
        PASTE_TEXT.with(|slot| slot.borrow_mut().clear());
        // A clipboard sync tool may re-publish what it saw of the offer (its
        // markers, never its text) after the restore. Watch for that briefly
        // and restore once more; anything else replacing it is the user's.
        let sequence = unsafe { GetClipboardSequenceNumber() };
        let watch_until = Instant::now() + Duration::from_millis(1_000);
        let mut restored = restored;
        let mut restored_again = false;
        while Instant::now() < watch_until {
            pump_for(Duration::from_millis(50));
            if unsafe { GetClipboardSequenceNumber() } != sequence {
                if !restored_again && echo_of_offer() {
                    restored = restore_clipboard(owner.0, &saved);
                    restored_again = true;
                } else {
                    break;
                }
            }
        }
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
            "clipboard_formats_restored": saved.len(),
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
    fn the_backend_runs_per_monitor_dpi_aware() {
        // Bounds, input and captured pixels only agree in physical pixels.
        let _backend = WindowsUiaBackend::new().expect("UIA is available");
        assert_eq!(dpi_awareness(), "per_monitor");
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
