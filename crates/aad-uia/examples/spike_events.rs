// Spike: can this Rust stack receive UIA events at all?
//
// Everything else in event-driven recording is plumbing I know how to write.
// This is the part that could turn out to be impossible or need a different
// shape, so it gets answered before anything is built on top of it.
//
// Three separate questions, and a "yes" to one is not a yes to the others:
//   1. Can the `windows` crate implement the callback COM interfaces at all?
//   2. Do callbacks actually fire for a real interaction?
//   3. Which thread do they arrive on -- that decides what the buffer must be.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// The #[implement] macro expands to `windows_core::` paths, so that name has to
// resolve here. The windows crate re-exports it as `windows::core`.
// windows-core is a direct dependency; the macro resolves it at the crate root.

use windows::core::{implement, Interface, Result, BSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl, IUIAutomationFocusChangedEventHandler,
    IUIAutomationFocusChangedEventHandler_Impl, IUIAutomationPropertyChangedEventHandler,
    IUIAutomationPropertyChangedEventHandler_Impl, TreeScope_Subtree, UIA_EVENT_ID,
    UIA_InvokePatternId, UIA_Invoke_InvokedEventId, UIA_PROPERTY_ID, UIA_TogglePatternId,
    UIA_ToggleToggleStatePropertyId, UIA_ValuePatternId, UIA_ValueValuePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

#[derive(Debug)]
struct Seen {
    kind: String,
    name: String,
    thread: u32,
}

#[implement(IUIAutomationEventHandler)]
struct Invoked(Arc<Mutex<Vec<Seen>>>);

impl IUIAutomationEventHandler_Impl for Invoked_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        _id: UIA_EVENT_ID,
    ) -> Result<()> {
        let name = sender
            .and_then(|element| unsafe { element.CurrentName() }.ok())
            .map(|value| value.to_string())
            .unwrap_or_default();
        self.0.lock().unwrap().push(Seen {
            kind: "invoked".into(),
            name,
            thread: unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
        });
        Ok(())
    }
}

#[implement(IUIAutomationPropertyChangedEventHandler)]
struct ValueChanged(Arc<Mutex<Vec<Seen>>>);

impl IUIAutomationPropertyChangedEventHandler_Impl for ValueChanged_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        _property: UIA_PROPERTY_ID,
        _value: &windows::core::VARIANT,
    ) -> Result<()> {
        let name = sender
            .and_then(|element| unsafe { element.CurrentName() }.ok())
            .map(|value| value.to_string())
            .unwrap_or_default();
        self.0.lock().unwrap().push(Seen {
            kind: "value_changed".into(),
            name,
            thread: unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
        });
        Ok(())
    }
}

#[implement(IUIAutomationFocusChangedEventHandler)]
struct FocusChanged(Arc<AtomicUsize>);

impl IUIAutomationFocusChangedEventHandler_Impl for FocusChanged_Impl {
    fn HandleFocusChangedEvent(&self, _sender: Option<&IUIAutomationElement>) -> Result<()> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn main() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;

    let title: Vec<u16> = "AAD Capture Fixture | clicks=0 text= checked=False\0"
        .encode_utf16()
        .collect();
    let hwnd: HWND = unsafe { FindWindowW(None, windows::core::PCWSTR(title.as_ptr())) }?;
    println!("window: {hwnd:?}");

    let root = unsafe { automation.ElementFromHandle(hwnd) }?;
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let focus_count = Arc::new(AtomicUsize::new(0));

    let invoked: IUIAutomationEventHandler = Invoked(seen.clone()).into();
    unsafe {
        automation.AddAutomationEventHandler(
            UIA_Invoke_InvokedEventId,
            &root,
            TreeScope_Subtree,
            None,
            &invoked,
        )
    }?;

    let value_changed: IUIAutomationPropertyChangedEventHandler =
        ValueChanged(seen.clone()).into();
    unsafe {
        automation.AddPropertyChangedEventHandlerNativeArray(
            &root,
            TreeScope_Subtree,
            None,
            &value_changed,
            &[UIA_ValueValuePropertyId, UIA_ToggleToggleStatePropertyId],
        )
    }?;

    let focus: IUIAutomationFocusChangedEventHandler = FocusChanged(focus_count.clone()).into();
    unsafe { automation.AddFocusChangedEventHandler(None, &focus) }?;

    println!("subscribed on thread {}", unsafe {
        windows::Win32::System::Threading::GetCurrentThreadId()
    });
    println!("driving one of each interaction kind");

    // Drive one interaction from here so the spike does not depend on a human.
    let condition = unsafe { automation.CreateTrueCondition() }?;
    let all = unsafe { root.FindAll(TreeScope_Subtree, &condition) }?;
    let count = unsafe { all.Length() }?;
    for index in 0..count {
        let element = unsafe { all.GetElement(index) }?;
        let name = unsafe { element.CurrentName() }
            .unwrap_or_default()
            .to_string();

        // A button: should raise Invoked.
        if name.contains("Submit") {
            if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_InvokePatternId) } {
                if let Ok(invoke) = pattern.cast::<
                    windows::Win32::UI::Accessibility::IUIAutomationInvokePattern,
                >() {
                    println!("invoking {name}");
                    let _ = unsafe { invoke.Invoke() };
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
        }

        // An edit: should raise a Value property change.
        if name.contains("NameBox") {
            if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_ValuePatternId) } {
                if let Ok(value) = pattern.cast::<
                    windows::Win32::UI::Accessibility::IUIAutomationValuePattern,
                >() {
                    println!("typing into {name}");
                    let _ = unsafe { value.SetValue(&BSTR::from("hello")) };
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
        }

        // A checkbox: toggling is a different pattern again.
        if name.contains("Subscribe") {
            if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_TogglePatternId) } {
                if let Ok(toggle) = pattern.cast::<
                    windows::Win32::UI::Accessibility::IUIAutomationTogglePattern,
                >() {
                    println!("toggling {name}");
                    let _ = unsafe { toggle.Toggle() };
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
        }
    }

    std::thread::sleep(std::time::Duration::from_secs(3));

    let events = seen.lock().unwrap();
    println!("\n{} events, {} focus changes", events.len(), focus_count.load(Ordering::Relaxed));
    for event in events.iter() {
        println!("  {} name={:?} thread={}", event.kind, event.name, event.thread);
    }
    Ok(())
}
