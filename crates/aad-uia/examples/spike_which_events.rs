// Which events can a recorder actually rely on?
//
// First spike answered the mechanical question -- Rust can implement the
// callback interfaces, events arrive, and they arrive on a *different* thread
// than the one that subscribed, so the buffer must be shared safely.
//
// It also turned up something that decides the design: driving a WinForms
// button through UIA's Invoke pattern raised no Invoked event, and toggling a
// checkbox raised no property change. Only the edit's Value change came
// through. If that holds for real clicks too, then a recorder built on Invoked
// would silently miss every button press -- the precise failure this project
// exists to prevent.
//
// So: subscribe broadly, drive a real mouse click rather than a programmatic
// invoke, and report exactly which events fire for which interaction.

use std::sync::{Arc, Mutex};

use windows::core::{implement, Interface, Result, BSTR};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl, IUIAutomationPropertyChangedEventHandler,
    IUIAutomationPropertyChangedEventHandler_Impl, TreeScope_Subtree, UIA_EVENT_ID,
    UIA_Invoke_InvokedEventId, UIA_PROPERTY_ID, UIA_SelectionItem_ElementSelectedEventId,
    UIA_ToggleToggleStatePropertyId, UIA_ValueValuePropertyId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetCursorPos};

#[derive(Debug, Clone)]
struct Seen {
    kind: String,
    name: String,
    thread: u32,
}

#[implement(IUIAutomationEventHandler)]
struct Automation(Arc<Mutex<Vec<Seen>>>);

impl IUIAutomationEventHandler_Impl for Automation_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        id: UIA_EVENT_ID,
    ) -> Result<()> {
        let name = sender
            .and_then(|element| unsafe { element.CurrentName() }.ok())
            .map(|value| value.to_string())
            .unwrap_or_default();
        self.0.lock().unwrap().push(Seen {
            kind: format!("event:{}", id.0),
            name,
            thread: unsafe { GetCurrentThreadId() },
        });
        Ok(())
    }
}

#[implement(IUIAutomationPropertyChangedEventHandler)]
struct Property(Arc<Mutex<Vec<Seen>>>);

impl IUIAutomationPropertyChangedEventHandler_Impl for Property_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        property: UIA_PROPERTY_ID,
        _value: &windows::core::VARIANT,
    ) -> Result<()> {
        let name = sender
            .and_then(|element| unsafe { element.CurrentName() }.ok())
            .map(|value| value.to_string())
            .unwrap_or_default();
        self.0.lock().unwrap().push(Seen {
            kind: format!("property:{}", property.0),
            name,
            thread: unsafe { GetCurrentThreadId() },
        });
        Ok(())
    }
}

/// Click where the element actually is, the way a person would.
fn click_element(element: &IUIAutomationElement) -> Result<()> {
    let rect = unsafe { element.CurrentBoundingRectangle() }?;
    let x = (rect.left + rect.right) / 2;
    let y = (rect.top + rect.bottom) / 2;
    unsafe { SetCursorPos(x, y) }?;
    std::thread::sleep(std::time::Duration::from_millis(120));

    // One event per call: this machine's SendInput refuses batches of two or
    // more (AutoHotkey and LogiBolt are installed and filter them).
    for flags in [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP] {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if sent != 1 {
            println!("  (SendInput refused: sent {sent})");
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    Ok(())
}

fn find_by_name<'a>(
    elements: &'a windows::Win32::UI::Accessibility::IUIAutomationElementArray,
    needle: &str,
) -> Option<IUIAutomationElement> {
    let count = unsafe { elements.Length() }.ok()?;
    for index in 0..count {
        let element = unsafe { elements.GetElement(index) }.ok()?;
        let name = unsafe { element.CurrentName() }
            .unwrap_or_default()
            .to_string();
        if name.contains(needle) {
            return Some(element);
        }
    }
    None
}

fn main() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;

    let title: Vec<u16> = "AAD Capture Fixture | clicks=0 text= checked=False\0"
        .encode_utf16()
        .collect();
    let hwnd: HWND = unsafe { FindWindowW(None, windows::core::PCWSTR(title.as_ptr())) }?;
    let root = unsafe { automation.ElementFromHandle(hwnd) }?;

    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));

    // Every automation event that could plausibly mark a click.
    let handler: IUIAutomationEventHandler = Automation(seen.clone()).into();
    for id in [
        UIA_Invoke_InvokedEventId,
        UIA_SelectionItem_ElementSelectedEventId,
    ] {
        unsafe {
            automation.AddAutomationEventHandler(id, &root, TreeScope_Subtree, None, &handler)
        }?;
    }

    let property: IUIAutomationPropertyChangedEventHandler = Property(seen.clone()).into();
    unsafe {
        automation.AddPropertyChangedEventHandlerNativeArray(
            &root,
            TreeScope_Subtree,
            None,
            &property,
            &[UIA_ValueValuePropertyId, UIA_ToggleToggleStatePropertyId],
        )
    }?;

    println!("subscribed on thread {}", unsafe { GetCurrentThreadId() });

    let condition = unsafe { automation.CreateTrueCondition() }?;
    let all = unsafe { root.FindAll(TreeScope_Subtree, &condition) }?;

    let mut report: Vec<(String, Vec<Seen>)> = Vec::new();

    // Each interaction is driven, then the events it produced are collected,
    // so an event cannot be attributed to the wrong interaction.
    let mut take = |label: &str, seen: &Arc<Mutex<Vec<Seen>>>, report: &mut Vec<(String, Vec<Seen>)>| {
        std::thread::sleep(std::time::Duration::from_millis(900));
        let mut guard = seen.lock().unwrap();
        report.push((label.to_string(), guard.clone()));
        guard.clear();
    };

    if let Some(button) = find_by_name(&all, "Submit") {
        println!("real mouse click on Submit");
        click_element(&button)?;
        take("mouse click on a button", &seen, &mut report);
    }

    if let Some(check) = find_by_name(&all, "Subscribe") {
        println!("real mouse click on the checkbox");
        click_element(&check)?;
        take("mouse click on a checkbox", &seen, &mut report);
    }

    if let Some(edit) = find_by_name(&all, "NameBox") {
        println!("SetValue on the edit");
        if let Ok(pattern) =
            unsafe { edit.GetCurrentPattern(windows::Win32::UI::Accessibility::UIA_ValuePatternId) }
        {
            if let Ok(value) =
                pattern.cast::<windows::Win32::UI::Accessibility::IUIAutomationValuePattern>()
            {
                let _ = unsafe { value.SetValue(&BSTR::from("typed")) };
            }
        }
        take("set_value on an edit", &seen, &mut report);
    }

    println!("\n=== 每种交互产生了什么事件 ===");
    for (label, events) in &report {
        if events.is_empty() {
            println!("  {label}: 什么都没有  <-- 录不到");
        } else {
            let kinds: Vec<String> = events
                .iter()
                .map(|event| format!("{} on {:?}", event.kind, event.name))
                .collect();
            println!("  {label}: {}", kinds.join(", "));
        }
    }

    let cursor = POINT::default();
    let _ = cursor;
    Ok(())
}
