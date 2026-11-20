// UIA event handlers or WinEvent hooks -- which one actually sees a click?
//
// Measured, with the fixture's own counters confirming every interaction
// landed: a real mouse click on a WinForms button and on a WinForms checkbox
// produced ZERO UIA events. Only an edit's value change came through. A
// recorder built on UIA event handlers would therefore silently miss most of
// what a person does, which is the exact failure this project exists to avoid.
//
// The likely reason is that WinForms is surfaced through the MSAA-to-UIA
// bridge, and the bridge does not synthesise the full UIA event set. MSAA's
// own notifications are delivered by SetWinEventHook instead.
//
// This runs both mechanisms at once against the same interactions, so the
// comparison is like-for-like. Whichever sees the click decides the design.
//
// WinEvent hooks are delivered to a thread's message queue, so this one needs
// a real message loop -- unlike the UIA handlers, which arrive on COM worker
// threads.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{implement, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl, IUIAutomationPropertyChangedEventHandler,
    IUIAutomationPropertyChangedEventHandler_Impl, SetWinEventHook, TreeScope_Subtree,
    UIA_Invoke_InvokedEventId, UIA_ToggleToggleStatePropertyId, UIA_ValueValuePropertyId,
    HWINEVENTHOOK, UIA_EVENT_ID, UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, MSG, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

/// MSAA events worth watching. Names from winuser.h.
const EVENT_OBJECT_FOCUS: u32 = 0x8005;
const EVENT_OBJECT_INVOKED: u32 = 0x8012;
const EVENT_OBJECT_VALUECHANGE: u32 = 0x800E;
const EVENT_OBJECT_STATECHANGE: u32 = 0x800A;
const EVENT_OBJECT_SELECTION: u32 = 0x8006;

static UIA_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
static WIN_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
static TARGET: OnceLock<isize> = OnceLock::new();
static WIN_TOTAL: AtomicUsize = AtomicUsize::new(0);

fn name_of(sender: Option<&IUIAutomationElement>) -> String {
    sender
        .and_then(|element| unsafe { element.CurrentName() }.ok())
        .map(|value| value.to_string())
        .unwrap_or_default()
}

#[implement(IUIAutomationEventHandler)]
struct UiaAutomation;

impl IUIAutomationEventHandler_Impl for UiaAutomation_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        id: UIA_EVENT_ID,
    ) -> Result<()> {
        UIA_SEEN
            .lock()
            .unwrap()
            .push(format!("event {} on {:?}", id.0, name_of(sender)));
        Ok(())
    }
}

#[implement(IUIAutomationPropertyChangedEventHandler)]
struct UiaProperty;

impl IUIAutomationPropertyChangedEventHandler_Impl for UiaProperty_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        property: UIA_PROPERTY_ID,
        _value: &windows::core::VARIANT,
    ) -> Result<()> {
        UIA_SEEN
            .lock()
            .unwrap()
            .push(format!("property {} on {:?}", property.0, name_of(sender)));
        Ok(())
    }
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // Only the fixture's own window tree; the hook is desktop-wide.
    let Some(&target) = TARGET.get() else { return };
    if hwnd.0 as isize != target && !is_descendant(hwnd, target) {
        return;
    }
    // OBJID_CLIENT is the control itself; ignore caret/cursor noise.
    if id_object != 0 && id_object != -4 {
        return;
    }
    let label = match event {
        EVENT_OBJECT_FOCUS => "focus",
        EVENT_OBJECT_INVOKED => "invoked",
        EVENT_OBJECT_VALUECHANGE => "value_changed",
        EVENT_OBJECT_STATECHANGE => "state_changed",
        EVENT_OBJECT_SELECTION => "selection",
        other => {
            WIN_SEEN.lock().unwrap().push(format!("other({other:#x})"));
            return;
        }
    };
    WIN_TOTAL.fetch_add(1, Ordering::Relaxed);
    WIN_SEEN.lock().unwrap().push(label.to_string());
}

fn is_descendant(hwnd: HWND, root: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GA_ROOT};
    let ancestor = unsafe { GetAncestor(hwnd, GA_ROOT) };
    ancestor.0 as isize == root
}

fn find_fixture() -> HWND {
    for clicks in 0..4 {
        for text in ["", "typed", "driven"] {
            for checked in ["False", "True"] {
                let title = format!(
                    "AAD Capture Fixture | clicks={clicks} text={text} checked={checked}\0"
                );
                let wide: Vec<u16> = title.encode_utf16().collect();
                if let Ok(hwnd) = unsafe { FindWindowW(None, windows::core::PCWSTR(wide.as_ptr())) }
                {
                    if !hwnd.is_invalid() {
                        return hwnd;
                    }
                }
            }
        }
    }
    panic!("capture fixture not found");
}

fn main() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;

    let hwnd = find_fixture();
    let _ = TARGET.set(hwnd.0 as isize);
    let root = unsafe { automation.ElementFromHandle(hwnd) }?;

    let handler: IUIAutomationEventHandler = UiaAutomation.into();
    unsafe {
        automation.AddAutomationEventHandler(
            UIA_Invoke_InvokedEventId,
            &root,
            TreeScope_Subtree,
            None,
            &handler,
        )
    }?;
    let property: IUIAutomationPropertyChangedEventHandler = UiaProperty.into();
    unsafe {
        automation.AddPropertyChangedEventHandlerNativeArray(
            &root,
            TreeScope_Subtree,
            None,
            &property,
            &[UIA_ValueValuePropertyId, UIA_ToggleToggleStatePropertyId],
        )
    }?;

    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_INVOKED,
            None,
            Some(win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if hook.is_invalid() {
        println!("SetWinEventHook failed");
    }

    println!("LISTENING");

    // WinEvents need a message pump; run one until the window is gone or time
    // runs out, then report.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(22);
    while std::time::Instant::now() < deadline {
        let mut message = MSG::default();
        // Non-blocking drain, then a short sleep, so the deadline is honoured.
        while unsafe {
            windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut message,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
            )
        }
        .as_bool()
        {
            unsafe { DispatchMessageW(&message) };
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    println!("\n=== UIA 事件处理器看到了什么 ===");
    let uia = UIA_SEEN.lock().unwrap();
    if uia.is_empty() {
        println!("  什么都没有");
    } else {
        for line in uia.iter() {
            println!("  {line}");
        }
    }

    println!("\n=== WinEvent 钩子看到了什么 ===");
    let win = WIN_SEEN.lock().unwrap();
    if win.is_empty() {
        println!("  什么都没有");
    } else {
        for line in win.iter() {
            println!("  {line}");
        }
    }
    Ok(())
}
