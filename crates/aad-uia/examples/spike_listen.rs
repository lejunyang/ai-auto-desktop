// Listen while something else does the clicking.
//
// The previous spike concluded "a button click raises no events". That was
// wrong: reading the fixture's title afterwards showed clicks=0, so the
// synthetic mouse input never landed. It measured a broken click, not UIA.
//
// The fix is to stop actuating from inside the listener. This subscribes and
// prints events as they arrive; the clicking is done from another process by
// the product's own driver, which is independently confirmed to work (the
// fixture's click counter moves). Then "no event" means no event.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use windows::core::{implement, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl, IUIAutomationPropertyChangedEventHandler,
    IUIAutomationPropertyChangedEventHandler_Impl, TreeScope_Subtree, UIA_EVENT_ID,
    UIA_Invoke_InvokedEventId, UIA_PROPERTY_ID, UIA_SelectionItem_ElementSelectedEventId,
    UIA_StructureChangedEventId, UIA_ToggleToggleStatePropertyId, UIA_ValueValuePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

fn describe(sender: Option<&IUIAutomationElement>) -> String {
    sender
        .map(|element| {
            let name = unsafe { element.CurrentName() }
                .unwrap_or_default()
                .to_string();
            let control = unsafe { element.CurrentControlType() }
                .map(|value| value.0)
                .unwrap_or_default();
            format!("name={name:?} control_type={control}")
        })
        .unwrap_or_else(|| "<no sender>".into())
}

#[implement(IUIAutomationEventHandler)]
struct Automation(Sender<String>);

impl IUIAutomationEventHandler_Impl for Automation_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        id: UIA_EVENT_ID,
    ) -> Result<()> {
        let _ = self.0.send(format!(
            "event {} {} [thread {}]",
            id.0,
            describe(sender),
            unsafe { GetCurrentThreadId() }
        ));
        Ok(())
    }
}

#[implement(IUIAutomationPropertyChangedEventHandler)]
struct Property(Sender<String>);

impl IUIAutomationPropertyChangedEventHandler_Impl for Property_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        property: UIA_PROPERTY_ID,
        _value: &windows::core::VARIANT,
    ) -> Result<()> {
        let _ = self.0.send(format!(
            "property {} {} [thread {}]",
            property.0,
            describe(sender),
            unsafe { GetCurrentThreadId() }
        ));
        Ok(())
    }
}

fn main() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;

    // Match on the stable prefix: the title changes as the form is used.
    let hwnd = find_fixture()?;
    let root = unsafe { automation.ElementFromHandle(hwnd) }?;

    let (tx, rx) = channel::<String>();
    let handler: IUIAutomationEventHandler = Automation(tx.clone()).into();
    for id in [
        UIA_Invoke_InvokedEventId,
        UIA_SelectionItem_ElementSelectedEventId,
        UIA_StructureChangedEventId,
    ] {
        unsafe {
            automation.AddAutomationEventHandler(id, &root, TreeScope_Subtree, None, &handler)
        }?;
    }

    let property: IUIAutomationPropertyChangedEventHandler = Property(tx).into();
    unsafe {
        automation.AddPropertyChangedEventHandlerNativeArray(
            &root,
            TreeScope_Subtree,
            None,
            &property,
            &[UIA_ValueValuePropertyId, UIA_ToggleToggleStatePropertyId],
        )
    }?;

    println!("LISTENING");

    // Keep the subscription alive and report whatever turns up.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    let collected = Arc::new(Mutex::new(Vec::<String>::new()));
    while std::time::Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(std::time::Duration::from_millis(250)) {
            println!("  {line}");
            collected.lock().unwrap().push(line);
        }
    }

    println!("TOTAL {}", collected.lock().unwrap().len());
    Ok(())
}

fn find_fixture() -> Result<HWND> {
    // The title carries live state, so try the ones the fixture can show.
    for title in [
        "AAD Capture Fixture | clicks=0 text= checked=False",
        "AAD Capture Fixture | clicks=1 text= checked=False",
        "AAD Capture Fixture | clicks=0 text=typed checked=False",
        "AAD Capture Fixture | clicks=1 text=typed checked=False",
    ] {
        let wide: Vec<u16> = format!("{title}\0").encode_utf16().collect();
        if let Ok(hwnd) = unsafe { FindWindowW(None, windows::core::PCWSTR(wide.as_ptr())) } {
            if !hwnd.is_invalid() {
                return Ok(hwnd);
            }
        }
    }
    panic!("capture fixture not found -- is it running?");
}
