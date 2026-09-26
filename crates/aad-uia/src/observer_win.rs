//! Native Windows implementation of the ambient observation session.
//!
//! Kept in its own file (rather than inside `windows.rs`) because it owns an
//! independent set of desktop-wide subscriptions and its own hook thread
//! local: the recorder's single-window `CaptureSession` and this system-wide
//! observer must be installable at the same time without sharing callback
//! state.
//!
//! Subscription design is settled by real-machine measurement — see
//! `docs/research/ambient-observation-monitor.md` §8:
//!
//! - a desktop-root UIA StructureChanged subscription is a constant ~36
//!   events/s background storm, so UIA callbacks are filtered to the
//!   foreground process the moment they arrive and never enter the buffer;
//! - the WinEvent hook is cheap, desktop-wide and already carries the hwnd, so
//!   it is left unfiltered apart from the object-id gate;
//! - `EVENT_SYSTEM_FOREGROUND` reports every application switch exactly once.

#![cfg(windows)]

use crate::observe::{
    EventElement, EventWindow, ObservationBuffer, ObservationKind, ObservedEvent,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::Duration;
use windows::core::{implement, Result};
use windows::Win32::Foundation::{CloseHandle, HWND};
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
    UnhookWinEvent, HWINEVENTHOOK, UIA_EVENT_ID, UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    PeekMessageW, MSG, PM_REMOVE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

// WinEvent families the observer consumes.
const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_OBJECT_FOCUS: u32 = 0x8005;
const EVENT_OBJECT_SELECTION: u32 = 0x8006;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
const EVENT_OBJECT_STATECHANGE: u32 = 0x800A;
const EVENT_OBJECT_VALUECHANGE: u32 = 0x800E;
const EVENT_OBJECT_INVOKED: u32 = 0x8012;

const OBJID_WINDOW: i32 = 0;
const OBJID_CLIENT: i32 = -4;

/// How often the pump wakes to notice a stop request.
const PUMP_TICK: Duration = Duration::from_millis(20);

fn window_title(hwnd: HWND) -> String {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len as usize])
}

fn process_name(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut buffer = [0u16; 512];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        ok.then(|| {
            let full = String::from_utf16_lossy(&buffer[..size as usize]);
            full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
        })
    }
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn event_window(hwnd: HWND) -> EventWindow {
    let pid = pid_of(hwnd);
    EventWindow {
        window_id: Some(format!("hwnd:{}", hwnd.0 as isize)),
        process_id: pid,
        process_name: process_name(pid),
        title: Some(window_title(hwnd)).filter(|text| !text.is_empty()),
    }
}

/// Cheap element summary for an event. Deliberately lighter than the
/// recorder's full `describe_element`: no patterns are queried. The event is
/// only a trigger; the follow-up snapshot diff is the authoritative content.
fn element_summary(sender: Option<&IUIAutomationElement>) -> Option<EventElement> {
    let element = sender?;
    let text = |result: windows::core::Result<windows::core::BSTR>| -> Option<String> {
        result
            .ok()
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
    };
    Some(EventElement {
        role: Some(format!(
            "control_type_{}",
            unsafe { element.CurrentControlType() }.map(|id| id.0).unwrap_or(0)
        )),
        name: text(unsafe { element.CurrentName() }),
        automation_id: text(unsafe { element.CurrentAutomationId() }),
        class_name: text(unsafe { element.CurrentClassName() }),
    })
}

/// State shared with the native callbacks.
struct Shared {
    buffer: Arc<ObservationBuffer>,
    /// Pid of the current foreground window. UIA callbacks from any other
    /// process are dropped immediately: measurement showed a root subtree
    /// subscription receives a constant ~36 events/s from background apps.
    foreground_pid: AtomicU32,
}

impl Shared {
    fn is_foreground_pid(&self, pid: u32) -> bool {
        pid != 0 && pid == self.foreground_pid.load(Ordering::SeqCst)
    }
}

thread_local! {
    static OBSERVE_HOOK: std::cell::RefCell<Option<Arc<Shared>>> =
        const { std::cell::RefCell::new(None) };
}

unsafe extern "system" fn on_observe_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let is_foreground_switch = event == EVENT_SYSTEM_FOREGROUND;
        if !is_foreground_switch {
            // Object events only carry meaning for the window/client itself;
            // every other id is noise from nested non-client objects.
            if id_object != OBJID_WINDOW && id_object != OBJID_CLIENT {
                return;
            }
            if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
                return;
            }
        }

        let Some(kind) = winevent_kind(event) else {
            return;
        };

        OBSERVE_HOOK.with(|slot| {
            let Some(shared) = slot.borrow().as_ref().map(Arc::clone) else {
                return;
            };

            if is_foreground_switch {
                let pid = pid_of(hwnd);
                shared.foreground_pid.store(pid, Ordering::SeqCst);
                shared
                    .buffer
                    .push(kind, "winevent".to_string(), event_window(hwnd), None);
                return;
            }

            // Non-foreground object activity is not what an ambient log wants.
            let pid = pid_of(hwnd);
            if !shared.is_foreground_pid(pid) {
                return;
            }
            shared
                .buffer
                .push(kind, "winevent".to_string(), event_window(hwnd), None);
        });
    }));
}

fn winevent_kind(event: u32) -> Option<ObservationKind> {
    Some(match event {
        EVENT_SYSTEM_FOREGROUND => ObservationKind::ForegroundChanged,
        EVENT_OBJECT_FOCUS => ObservationKind::FocusChanged,
        EVENT_OBJECT_SELECTION => ObservationKind::SelectionChanged,
        EVENT_OBJECT_NAMECHANGE | EVENT_OBJECT_STATECHANGE => ObservationKind::Other,
        EVENT_OBJECT_VALUECHANGE => ObservationKind::ValueChanged,
        EVENT_OBJECT_INVOKED => ObservationKind::Invoked,
        _ => return None,
    })
}

#[implement(IUIAutomationFocusChangedEventHandler)]
struct ObserveFocusHandler {
    shared: Arc<Shared>,
}

impl IUIAutomationFocusChangedEventHandler_Impl for ObserveFocusHandler_Impl {
    fn HandleFocusChangedEvent(&self, sender: Option<&IUIAutomationElement>) -> Result<()> {
        let pid = sender
            .and_then(|element| unsafe { element.CurrentProcessId() }.ok())
            .unwrap_or(0) as u32;
        if !self.shared.is_foreground_pid(pid) {
            return Ok(());
        }
        self.shared
            .buffer
            .push(
                ObservationKind::FocusChanged,
                "uia".to_string(),
                window_from_pid(pid, sender),
                element_summary(sender),
            );
        Ok(())
    }
}

#[implement(IUIAutomationEventHandler)]
struct ObserveAutomationHandler {
    shared: Arc<Shared>,
}

impl IUIAutomationEventHandler_Impl for ObserveAutomationHandler_Impl {
    fn HandleAutomationEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        id: UIA_EVENT_ID,
    ) -> Result<()> {
        let pid = sender
            .and_then(|element| unsafe { element.CurrentProcessId() }.ok())
            .unwrap_or(0) as u32;
        if !self.shared.is_foreground_pid(pid) {
            return Ok(());
        }
        let kind = if id == UIA_StructureChangedEventId {
            ObservationKind::StructureChanged
        } else if id == UIA_SelectionItem_ElementSelectedEventId {
            ObservationKind::SelectionChanged
        } else if id == UIA_Invoke_InvokedEventId {
            ObservationKind::Invoked
        } else {
            ObservationKind::Other
        };
        self.shared
            .buffer
            .push(
            kind,
            "uia".to_string(),
            window_from_pid(pid, sender),
            element_summary(sender),
        );
        Ok(())
    }
}

#[implement(IUIAutomationPropertyChangedEventHandler)]
struct ObservePropertyHandler {
    shared: Arc<Shared>,
}

impl IUIAutomationPropertyChangedEventHandler_Impl for ObservePropertyHandler_Impl {
    fn HandlePropertyChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
        property: UIA_PROPERTY_ID,
        _value: &windows::core::VARIANT,
    ) -> Result<()> {
        let pid = sender
            .and_then(|element| unsafe { element.CurrentProcessId() }.ok())
            .unwrap_or(0) as u32;
        if !self.shared.is_foreground_pid(pid) {
            return Ok(());
        }
        let kind = if property == UIA_ValueValuePropertyId {
            ObservationKind::ValueChanged
        } else if property == UIA_ToggleToggleStatePropertyId {
            ObservationKind::Other
        } else {
            return Ok(());
        };
        self.shared
            .buffer
            .push(
            kind,
            "uia".to_string(),
            window_from_pid(pid, sender),
            element_summary(sender),
        );
        Ok(())
    }
}

/// Window attribution for a UIA sender.
///
/// Only the process is reported. Two measured failures argue against doing
/// more inside the callback:
///
/// - `CurrentNativeWindowHandle` frequently returns an internal renderer or
///   popup ("Chrome Legacy Window"), not the top-level window the user thinks
///   of; snapshotting it yields a useless 1-node tree;
/// - reading the foreground window's title here is racy, because by the time
///   the event is drained the foreground may already have changed.
///
/// The consumer attributes a pid-only event to the foreground window of its
/// own batch, which is the window the user was actually looking at.
fn window_from_pid(pid: u32, _sender: Option<&IUIAutomationElement>) -> EventWindow {
    EventWindow {
        window_id: None,
        process_id: pid,
        process_name: process_name(pid),
        title: None,
    }
}

/// A live, desktop-wide observation session.
///
/// Owns the pump thread for the lifetime of the session, exactly like
/// `CaptureSession`: WinEvent hooks are delivered to the installing thread's
/// queue, so something must pump while the subscription exists. Dropping the
/// session removes every hook and handler.
pub struct ObserverSession {
    buffer: Arc<ObservationBuffer>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    pub sources: Vec<&'static str>,
}

impl ObserverSession {
    pub fn start(limit: usize) -> crate::backend::Result<Self> {
        let buffer = ObservationBuffer::new(limit);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, started) = channel::<std::result::Result<Vec<&'static str>, String>>();
        let worker_buffer = Arc::clone(&buffer);
        let worker_stop = Arc::clone(&stop);

        let worker = std::thread::Builder::new()
            .name("aad-observe".to_string())
            .spawn(move || observe_pump(worker_buffer, worker_stop, ready))
            .map_err(|error| {
                crate::backend::DriverError::unavailable(format!(
                    "could not start an observation thread: {error}"
                ))
            })?;

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
                Err(crate::backend::DriverError::unavailable(message))
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                Err(crate::backend::DriverError::unavailable(
                    "the observation thread did not start within five seconds",
                ))
            }
        }
    }

    pub fn drain(&self, max: usize) -> (Vec<ObservedEvent>, u64) {
        self.buffer.drain(max)
    }

    pub fn pending(&self) -> usize {
        self.buffer.pending()
    }
}

impl Drop for ObserverSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn observe_pump(
    buffer: Arc<ObservationBuffer>,
    stop: Arc<AtomicBool>,
    ready: Sender<std::result::Result<Vec<&'static str>, String>>,
) {
    // "Already initialised" is fine: another owner may hold the apartment.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let automation: Result<IUIAutomation> =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) };
    let automation = match automation {
        Ok(automation) => automation,
        Err(error) => {
            let _ = ready.send(Err(format!("UI Automation is unavailable: {error}")));
            return;
        }
    };

    let foreground = unsafe { GetForegroundWindow() };
    let shared = Arc::new(Shared {
        buffer: Arc::clone(&buffer),
        foreground_pid: AtomicU32::new(pid_of(foreground)),
    });

    let mut sources: Vec<&'static str> = Vec::new();

    // --- UIA handlers at the desktop root, filtered to the foreground pid ----
    let mut installed_uia = true;
    let root = match unsafe { automation.GetRootElement() } {
        Ok(root) => Some(root),
        Err(_) => {
            installed_uia = false;
            None
        }
    };

    let focus_handler: IUIAutomationFocusChangedEventHandler = ObserveFocusHandler {
        shared: Arc::clone(&shared),
    }
    .into();
    if unsafe { automation.AddFocusChangedEventHandler(None, &focus_handler) }.is_err() {
        installed_uia = false;
    }

    let automation_handler: IUIAutomationEventHandler = ObserveAutomationHandler {
        shared: Arc::clone(&shared),
    }
    .into();
    let property_handler: IUIAutomationPropertyChangedEventHandler = ObservePropertyHandler {
        shared: Arc::clone(&shared),
    }
    .into();

    if let Some(root) = root.as_ref() {
        for id in [
            UIA_StructureChangedEventId,
            UIA_SelectionItem_ElementSelectedEventId,
            UIA_Invoke_InvokedEventId,
        ] {
            if unsafe {
                automation.AddAutomationEventHandler(id, root, TreeScope_Subtree, None, &automation_handler)
            }
            .is_err()
            {
                installed_uia = false;
            }
        }
        if unsafe {
            automation.AddPropertyChangedEventHandlerNativeArray(
                root,
                TreeScope_Subtree,
                None,
                &property_handler,
                &[UIA_ValueValuePropertyId, UIA_ToggleToggleStatePropertyId],
            )
        }
        .is_err()
        {
            installed_uia = false;
        }
    }
    if installed_uia {
        sources.push("uia");
    }

    // --- Desktop-wide WinEvent hook (foreground switches + object activity) -
    OBSERVE_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Arc::clone(&shared));
    });
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_OBJECT_INVOKED,
            None,
            Some(on_observe_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    let hook_ok = !hook.is_invalid();
    if hook_ok {
        sources.push("winevent");
    }

    if sources.is_empty() {
        let _ = ready.send(Err(
            "neither UI Automation nor WinEvent hooks could be installed".into(),
        ));
        return;
    }
    let _ = ready.send(Ok(sources));

    while !stop.load(Ordering::SeqCst) {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe { DispatchMessageW(&mut message) };
        }
        std::thread::sleep(PUMP_TICK);
    }

    if hook_ok {
        let _ = unsafe { UnhookWinEvent(hook) };
    }
    OBSERVE_HOOK.with(|slot| {
        *slot.borrow_mut() = None;
    });
    let _ = unsafe { automation.RemoveFocusChangedEventHandler(&focus_handler) };
    if let Some(root) = root.as_ref() {
        for id in [
            UIA_StructureChangedEventId,
            UIA_SelectionItem_ElementSelectedEventId,
            UIA_Invoke_InvokedEventId,
        ] {
            let _ = unsafe {
                automation.RemoveAutomationEventHandler(id, root, &automation_handler)
            };
        }
        let _ = unsafe { automation.RemovePropertyChangedEventHandler(root, &property_handler) };
    }
}
