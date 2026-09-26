// Ambient observation spike: what does a DESKTOP-ROOT subscription see?
//
// The product's capture session anchors one hwnd and listens for invoke/value
// on that window. An ambient monitor needs different questions answered:
//
//   1. Does EVENT_SYSTEM_FOREGROUND fire reliably when the user switches apps?
//   2. How noisy is a root-level UIA StructureChanged subscription (storm vs.
//      usable)? Same question for root focus and property handlers.
//   3. Do in-app changes such as browser tab switches show up, and as what?
//
// This subscribes at the desktop root, keeps only counters plus a bounded
// sample of raw events, and prints a histogram at the end. It never sends
// input. Run it, then switch apps / tabs / type for `seconds`, and read the
// report. Usage: spike_root_observe [seconds]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::{implement, Result};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl, IUIAutomationFocusChangedEventHandler,
    IUIAutomationFocusChangedEventHandler_Impl, IUIAutomationPropertyChangedEventHandler,
    IUIAutomationPropertyChangedEventHandler_Impl, SetWinEventHook, TreeScope_Subtree,
    UIA_Invoke_InvokedEventId, UIA_SelectionItem_ElementSelectedEventId,
    UIA_StructureChangedEventId, UIA_ToggleToggleStatePropertyId, UIA_ValueValuePropertyId,
    HWINEVENTHOOK, UIA_EVENT_ID, UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    MSG, PM_REMOVE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

// WinEvent range of interest for an ambient monitor.
const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_OBJECT_FOCUS: u32 = 0x8005;
const EVENT_OBJECT_SELECTION: u32 = 0x8006;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
const EVENT_OBJECT_VALUECHANGE: u32 = 0x800E;
const EVENT_OBJECT_INVOKED: u32 = 0x8012;
const EVENT_OBJECT_STATECHANGE: u32 = 0x800A;

/// At most this many raw lines are kept; beyond that only counters move, so a
/// storm cannot exhaust memory.
const MAX_SAMPLES: usize = 4000;

struct Sample {
    t_ms: u64,
    kind: &'static str,
    detail: String,
}

static START: Mutex<Option<std::time::Instant>> = Mutex::new(None);
static SAMPLES: Mutex<Vec<Sample>> = Mutex::new(Vec::new());
static OVERFLOW: AtomicU64 = AtomicU64::new(0);

fn elapsed_ms() -> u64 {
    START
        .lock()
        .unwrap()
        .map(|start| start.elapsed().as_millis() as u64)
        .unwrap_or(0)
}

fn record(kind: &'static str, detail: String) {
    let mut samples = SAMPLES.lock().unwrap();
    if samples.len() < MAX_SAMPLES {
        samples.push(Sample {
            t_ms: elapsed_ms(),
            kind,
            detail,
        });
    } else {
        OVERFLOW.fetch_add(1, Ordering::Relaxed);
    }
}

fn window_title(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len as usize])
}

fn process_name(pid: u32) -> String {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        let Ok(handle) = handle else {
            return format!("pid:{pid}");
        };
        let mut buffer = [0u16; 512];
        let mut size = buffer.len() as u32;
        if QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
        .is_ok()
        {
            let full = String::from_utf16_lossy(&buffer[..size as usize]);
            let _ = windows::Win32::Foundation::CloseHandle(handle);
            full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
        } else {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
            format!("pid:{pid}")
        }
    }
}

fn hwnd_detail(hwnd: HWND, id_object: i32) -> String {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let title = window_title(hwnd);
    let clipped: String = title.chars().take(48).collect();
    format!("obj={id_object:#x} proc={} title={clipped:?}", process_name(pid))
}

// --- WinEvent: foreground switches and MSAA-level activity -----------------

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let kind = match event {
        EVENT_SYSTEM_FOREGROUND => "fg",
        EVENT_OBJECT_FOCUS => "obj-focus",
        EVENT_OBJECT_SELECTION => "obj-selection",
        EVENT_OBJECT_NAMECHANGE => "obj-name",
        EVENT_OBJECT_VALUECHANGE => "obj-value",
        EVENT_OBJECT_STATECHANGE => "obj-state",
        EVENT_OBJECT_INVOKED => "obj-invoked",
        _ => return,
    };
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() && kind != "fg" {
        return;
    }
    record(kind, hwnd_detail(hwnd, id_object));
}

// --- UIA: root-level focus / structure / invoke / property -----------------

fn element_detail(sender: Option<&IUIAutomationElement>) -> String {
    let Some(element) = sender else {
        return "<no sender>".into();
    };
    let name = unsafe { element.CurrentName() }
        .map(|value| value.to_string())
        .unwrap_or_default();
    let control = unsafe { element.CurrentControlType() }.map(|id| id.0).unwrap_or(0);
    let pid = unsafe { element.CurrentProcessId() }.unwrap_or(0) as u32;
    let clipped: String = name.chars().take(48).collect();
    format!("type={control} proc={} name={clipped:?}", process_name(pid))
}

#[implement(IUIAutomationFocusChangedEventHandler)]
struct FocusHandler;

impl IUIAutomationFocusChangedEventHandler_Impl for FocusHandler_Impl {
    fn HandleFocusChangedEvent(&self, sender: Option<&IUIAutomationElement>) -> Result<()> {
        record("uia-focus", element_detail(sender));
        Ok(())
    }
}

#[implement(IUIAutomationEventHandler)]
struct AutomationHandler;

impl IUIAutomationEventHandler_Impl for AutomationHandler_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        id: UIA_EVENT_ID,
    ) -> Result<()> {
        let kind = if id == UIA_StructureChangedEventId {
            "uia-structure"
        } else if id == UIA_SelectionItem_ElementSelectedEventId {
            "uia-selected"
        } else if id == UIA_Invoke_InvokedEventId {
            "uia-invoked"
        } else {
            "uia-event"
        };
        record(kind, element_detail(sender));
        Ok(())
    }
}

#[implement(IUIAutomationPropertyChangedEventHandler)]
struct PropertyHandler;

impl IUIAutomationPropertyChangedEventHandler_Impl for PropertyHandler_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        property: UIA_PROPERTY_ID,
        _value: &windows::core::VARIANT,
    ) -> Result<()> {
        let kind = if property == UIA_ValueValuePropertyId {
            "uia-value"
        } else if property == UIA_ToggleToggleStatePropertyId {
            "uia-toggle"
        } else {
            "uia-property"
        };
        record(kind, element_detail(sender));
        Ok(())
    }
}

fn main() -> Result<()> {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);

    *START.lock().unwrap() = Some(std::time::Instant::now());

    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;

    // Root-level UIA subscriptions.
    let root = unsafe { automation.GetRootElement() }?;
    let focus: IUIAutomationFocusChangedEventHandler = FocusHandler.into();
    unsafe { automation.AddFocusChangedEventHandler(None, &focus) }?;
    let automation_handler: IUIAutomationEventHandler = AutomationHandler.into();
    for id in [
        UIA_StructureChangedEventId,
        UIA_SelectionItem_ElementSelectedEventId,
        UIA_Invoke_InvokedEventId,
    ] {
        unsafe {
            automation.AddAutomationEventHandler(id, &root, TreeScope_Subtree, None, &automation_handler)
        }?;
    }
    let property_handler: IUIAutomationPropertyChangedEventHandler = PropertyHandler.into();
    unsafe {
        automation.AddPropertyChangedEventHandlerNativeArray(
            &root,
            TreeScope_Subtree,
            None,
            &property_handler,
            &[UIA_ValueValuePropertyId, UIA_ToggleToggleStatePropertyId],
        )
    }?;

    // Root WinEvent hook spanning foreground switches and object activity.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_OBJECT_INVOKED,
            None,
            Some(win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    let hook_ok = !hook.is_invalid();

    let fg = unsafe { GetForegroundWindow() };
    println!(
        "LISTENING root for {seconds}s (winevent hook valid: {hook_ok}); current fg: {:?}",
        window_title(fg)
    );

    // WinEvents need a message pump on this thread.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        let mut message = MSG::default();
        while unsafe {
            windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut message,
                None,
                0,
                0,
                PM_REMOVE,
            )
        }
        .as_bool()
        {
            unsafe { DispatchMessageW(&mut message) };
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    if hook_ok {
        let _ = unsafe { windows::Win32::UI::Accessibility::UnhookWinEvent(hook) };
    }
    let _ = unsafe { automation.RemoveFocusChangedEventHandler(&focus) };
    for id in [
        UIA_StructureChangedEventId,
        UIA_SelectionItem_ElementSelectedEventId,
        UIA_Invoke_InvokedEventId,
    ] {
        let _ = unsafe {
            automation.RemoveAutomationEventHandler(id, &root, &automation_handler)
        };
    }
    let _ = unsafe {
        automation.RemovePropertyChangedEventHandler(&root, &property_handler)
    };

    // --- Report -------------------------------------------------------------
    let samples = std::mem::take(&mut *SAMPLES.lock().unwrap());
    let overflow = OVERFLOW.load(Ordering::Relaxed);

    let mut by_kind: BTreeMap<&str, u64> = BTreeMap::new();
    let mut by_second: BTreeMap<u64, u64> = BTreeMap::new();
    let mut fg_switches: Vec<&Sample> = Vec::new();
    for sample in &samples {
        *by_kind.entry(sample.kind).or_default() += 1;
        *by_second.entry(sample.t_ms / 1000).or_default() += 1;
        if sample.kind == "fg" {
            fg_switches.push(sample);
        }
    }

    println!("\n== totals ({} kept, {overflow} overflowed) ==", samples.len());
    for (kind, count) in &by_kind {
        println!("  {kind:<14} {count}");
    }

    println!("\n== events per second ==");
    for second in 0..seconds {
        let count = by_second.get(&second).copied().unwrap_or(0);
        let bar = "#".repeat((count.min(200)) as usize);
        println!("  {second:>3}s {count:>5} {bar}");
    }

    println!("\n== foreground switches ({}) ==", fg_switches.len());
    for sample in &fg_switches {
        println!("  {:>6}ms  {}", sample.t_ms, sample.detail);
    }

    // Show a few samples of the noisiest kinds so tab switches etc. are visible.
    for wanted in ["uia-selected", "uia-structure", "obj-focus", "uia-value"] {
        let lines: Vec<&Sample> = samples.iter().filter(|s| s.kind == wanted).take(8).collect();
        if !lines.is_empty() {
            println!("\n== sample {wanted} (up to 8) ==");
            for sample in lines {
                println!("  {:>6}ms  {}", sample.t_ms, sample.detail);
            }
        }
    }

    // Touch LPARAM so the import is not flagged on toolchain churn.
    let _ = LPARAM(0);
    Ok(())
}
