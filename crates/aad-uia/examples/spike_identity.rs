// Can a WinEvent be turned back into an element we can record?
//
// WinEvent hooks see the clicks that UIA handlers miss, but they deliver an
// HWND plus an object id -- not an element. A recorder needs an element: a
// role, a name, and enough to synthesise a locator that will still find the
// thing on replay. If that resolution is unreliable, the hook is useless for
// recording no matter how many events it sees, and the design has to change.
//
// The hopeful case is WinForms specifically: its controls are real windows, so
// the hook's hwnd should be the control itself and ElementFromHandle should
// resolve it directly. That is a guess about this toolkit, so it gets measured
// rather than assumed -- and measured on the WebView too, where elements have
// no HWND of their own and the answer will likely differ.

use std::sync::{Mutex, OnceLock};

use windows::core::Result;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, SetWinEventHook, HWINEVENTHOOK,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetAncestor, GetWindowTextW, IsWindowVisible, PeekMessageW,
    GA_ROOT, MSG, PM_REMOVE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

const EVENT_OBJECT_FOCUS: u32 = 0x8005;
const EVENT_OBJECT_INVOKED: u32 = 0x8012;
const EVENT_OBJECT_VALUECHANGE: u32 = 0x800E;
const EVENT_OBJECT_STATECHANGE: u32 = 0x800A;

static TARGET: OnceLock<isize> = OnceLock::new();
static NEEDLE: OnceLock<String> = OnceLock::new();
static FOUND: Mutex<Option<isize>> = Mutex::new(None);
static REPORT: Mutex<Vec<String>> = Mutex::new(Vec::new());
static AUTOMATION: OnceLock<AgileAutomation> = OnceLock::new();

/// The UI Automation client object is documented as agile, so it is safe to
/// use from the hook callback thread as well as the one that created it.
struct AgileAutomation(IUIAutomation);
unsafe impl Send for AgileAutomation {}
unsafe impl Sync for AgileAutomation {}

unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let Some(&target) = TARGET.get() else { return };
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if hwnd.0 as isize != target && root.0 as isize != target {
        return;
    }
    // Do not filter by object id yet: which ids these events carry is exactly
    // what is being measured. Filtering on a guess is how the first run of this
    // spike reported "no events" for clicks that the comparison spike saw.
    
    let label = match event {
        EVENT_OBJECT_FOCUS => "focus",
        EVENT_OBJECT_INVOKED => "invoked",
        EVENT_OBJECT_VALUECHANGE => "value_changed",
        EVENT_OBJECT_STATECHANGE => "state_changed",
        _ => return,
    };

    let Some(automation) = AUTOMATION.get() else { return };

    // The question: does this hwnd resolve to the *control*, or only to the
    // top-level window? Only the former is usable for recording.
    let resolved = match unsafe { automation.0.ElementFromHandle(hwnd) } {
        Ok(element) => {
            let name = unsafe { element.CurrentName() }
                .unwrap_or_default()
                .to_string();
            let control_type = unsafe { element.CurrentControlType() }
                .map(|value| value.0)
                .unwrap_or(0);
            let automation_id = unsafe { element.CurrentAutomationId() }
                .unwrap_or_default()
                .to_string();
            let is_window = hwnd.0 as isize == target;
            format!(
                "name={name:?} control_type={control_type} automation_id={automation_id:?} \
                 hwnd_is_toplevel={is_window}"
            )
        }
        Err(error) => format!("<could not resolve: {error}>"),
    };

    REPORT
        .lock()
        .unwrap()
        .push(format!(
            "{label} id_object={id_object} id_child={id_child} -> {resolved}"
        ));
}

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
        eprintln!("usage: spike_identity <window title substring> [seconds]");
        std::process::exit(2);
    });
    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let _ = NEEDLE.set(needle.clone());

    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;
    let _ = AUTOMATION.set(AgileAutomation(automation));

    let _ = unsafe { EnumWindows(Some(enum_window), LPARAM(0)) };
    let Some(raw) = *FOUND.lock().unwrap() else {
        eprintln!("no visible window whose title contains {needle:?}");
        std::process::exit(1);
    };
    let _ = TARGET.set(raw);

    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_INVOKED,
            None,
            Some(on_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    println!("LISTENING on {needle:?} (hook_valid={})", !hook.is_invalid());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe { DispatchMessageW(&message) };
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    println!("\n=== 每个事件能不能还原出元素 ===");
    let report = REPORT.lock().unwrap();
    if report.is_empty() {
        println!("  没有事件");
    }
    for line in report.iter().take(20) {
        println!("  {line}");
    }
    Ok(())
}
