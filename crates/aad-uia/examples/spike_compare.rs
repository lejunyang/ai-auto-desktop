// Does the WinEvent finding hold outside WinForms?
//
// Measured on a WinForms fixture, with both mechanisms subscribed at once and
// the fixture's own counters confirming the clicks landed: UIA event handlers
// saw nothing, WinEvent hooks saw 12 events. That is decisive for WinForms.
//
// It is not yet decisive for anything else. WinForms is surfaced through the
// MSAA-to-UIA bridge; a Chromium WebView (which is what this project's own GUI
// is) implements UIA natively and may well behave the opposite way. Choosing a
// mechanism on one toolkit and discovering the gap later would mean the
// recorder silently misses everything in a whole class of applications.
//
// So: same comparison, any window by title substring, so it can be pointed at
// the WebView GUI, at Explorer, at anything.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{implement, Result};
use windows::Win32::Foundation::{HWND, LPARAM, BOOL, TRUE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl, IUIAutomationPropertyChangedEventHandler,
    IUIAutomationPropertyChangedEventHandler_Impl, SetWinEventHook, HWINEVENTHOOK,
    TreeScope_Subtree, UIA_EVENT_ID, UIA_Invoke_InvokedEventId, UIA_PROPERTY_ID,
    UIA_SelectionItem_ElementSelectedEventId, UIA_ToggleToggleStatePropertyId,
    UIA_ValueValuePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetAncestor, GetWindowTextW, IsWindowVisible, PeekMessageW,
    GA_ROOT, MSG, PM_REMOVE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

const EVENT_OBJECT_FOCUS: u32 = 0x8005;
const EVENT_OBJECT_INVOKED: u32 = 0x8012;
const EVENT_OBJECT_VALUECHANGE: u32 = 0x800E;
const EVENT_OBJECT_STATECHANGE: u32 = 0x800A;
const EVENT_OBJECT_SELECTION: u32 = 0x8006;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;

static UIA_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
static WIN_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
static TARGET: OnceLock<isize> = OnceLock::new();
static UIA_TOTAL: AtomicUsize = AtomicUsize::new(0);

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
        UIA_TOTAL.fetch_add(1, Ordering::Relaxed);
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
        UIA_TOTAL.fetch_add(1, Ordering::Relaxed);
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
    let Some(&target) = TARGET.get() else { return };
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if hwnd.0 as isize != target && root.0 as isize != target {
        return;
    }
    if id_object != 0 && id_object != -4 {
        return;
    }
    let label = match event {
        EVENT_OBJECT_FOCUS => "focus",
        EVENT_OBJECT_INVOKED => "invoked",
        EVENT_OBJECT_VALUECHANGE => "value_changed",
        EVENT_OBJECT_STATECHANGE => "state_changed",
        EVENT_OBJECT_SELECTION => "selection",
        EVENT_OBJECT_NAMECHANGE => "name_changed",
        other => {
            WIN_SEEN.lock().unwrap().push(format!("other({other:#x})"));
            return;
        }
    };
    WIN_SEEN.lock().unwrap().push(label.to_string());
}

static NEEDLE: OnceLock<String> = OnceLock::new();
static FOUND: Mutex<Option<isize>> = Mutex::new(None);

unsafe extern "system" fn enum_window(hwnd: HWND, _param: LPARAM) -> BOOL {
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return TRUE;
    }
    let mut buffer = [0u16; 512];
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if length == 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&buffer[..length as usize]);
    if let Some(needle) = NEEDLE.get() {
        if title.contains(needle.as_str()) {
            *FOUND.lock().unwrap() = Some(hwnd.0 as isize);
            return BOOL(0);
        }
    }
    TRUE
}

fn main() -> Result<()> {
    let needle = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: spike_compare <window title substring>");
        std::process::exit(2);
    });
    let _ = NEEDLE.set(needle.clone());

    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;

    let _ = unsafe { EnumWindows(Some(enum_window), LPARAM(0)) };
    let Some(raw) = *FOUND.lock().unwrap() else {
        eprintln!("no visible window whose title contains {needle:?}");
        std::process::exit(1);
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let _ = TARGET.set(raw);

    let root = unsafe { automation.ElementFromHandle(hwnd) }?;
    let handler: IUIAutomationEventHandler = UiaAutomation.into();
    for id in [
        UIA_Invoke_InvokedEventId,
        UIA_SelectionItem_ElementSelectedEventId,
    ] {
        unsafe {
            automation.AddAutomationEventHandler(id, &root, TreeScope_Subtree, None, &handler)
        }?;
    }
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
    println!("LISTENING on {needle:?} (hook_valid={})", !hook.is_invalid());

    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(22);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe { DispatchMessageW(&message) };
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let uia = UIA_SEEN.lock().unwrap();
    let win = WIN_SEEN.lock().unwrap();
    println!("\nUIA handlers : {} events", uia.len());
    for line in uia.iter().take(12) {
        println!("    {line}");
    }
    println!("WinEvent hook: {} events", win.len());
    for line in win.iter().take(12) {
        println!("    {line}");
    }
    Ok(())
}
