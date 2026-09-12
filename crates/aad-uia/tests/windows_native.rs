//! End-to-end coverage against real Win32 controls and Windows UI Automation.
//!
//! The fixture is deliberately implemented in Rust and runs in the same test
//! process. This keeps the native provider regression test in the shipped
//! implementation language while still exercising Windows' real UIA bridge.

#![cfg(windows)]

use aad_uia::{native_driver, UiaDriver};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{UpdateWindow, COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetDlgItem, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetWindowTextW, ShowWindow, TranslateMessage, BN_CLICKED,
    CW_USEDEFAULT, ES_AUTOHSCROLL, HMENU, MSG, SHOW_WINDOW_CMD, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLOSE, WM_COMMAND, WM_DESTROY, WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW,
    WS_TABSTOP, WS_VISIBLE,
};

const EDIT_ID: i32 = 1001;
const INVOKE_ID: i32 = 1002;
const STATUS_ID: i32 = 1003;
const DUPLICATE_ONE_ID: i32 = 1004;
const DUPLICATE_TWO_ID: i32 = 1005;
const POINTER_ID: i32 = 1006;

const INVOKE_BUTTON: &str = "Apply fixture value";
const DUPLICATE_BUTTON: &str = "Duplicate action";
const POINTER_BUTTON: &str = "Pointer click target";
const INITIAL_STATUS: &str = "Status: idle";
const INVOKED_STATUS: &str = "Status: invoked";
const DUPLICATE_STATUS: &str = "Status: duplicate invoked";
const POINTER_STATUS: &str = "Status: pointer clicked";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn fixture_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let control_id = (wparam.0 & 0xffff) as i32;
            let notification = ((wparam.0 >> 16) & 0xffff) as u32;
            if notification == BN_CLICKED {
                let text = match control_id {
                    INVOKE_ID => Some(INVOKED_STATUS),
                    DUPLICATE_ONE_ID | DUPLICATE_TWO_ID => Some(DUPLICATE_STATUS),
                    POINTER_ID => Some(POINTER_STATUS),
                    _ => None,
                };
                if let Some(text) = text {
                    if let Ok(status) = GetDlgItem(hwnd, STATUS_ID) {
                        let text = wide(text);
                        let _ = SetWindowTextW(status, PCWSTR(text.as_ptr()));
                    }
                    return LRESULT(0);
                }
            }
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

#[derive(Clone, Copy)]
struct FixtureReady {
    hwnd: HWND,
}

// HWNDs are process-local integer handles. The fixture thread owns the window;
// the test thread only posts WM_CLOSE and passes its numeric id to UIA.
unsafe impl Send for FixtureReady {}

struct Fixture {
    hwnd: HWND,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl Fixture {
    fn start(title: String) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || run_fixture(title, sender));
        let ready = receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| format!("fixture did not become ready: {error}"))??;
        Ok(Self {
            hwnd: ready.hwnd,
            thread: Some(thread),
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(self.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_fixture(
    title: String,
    ready: mpsc::SyncSender<Result<FixtureReady, String>>,
) -> Result<(), String> {
    let result = unsafe { create_fixture(&title) };
    let hwnd = match result {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    ready
        .send(Ok(FixtureReady { hwnd }))
        .map_err(|error| format!("could not announce fixture readiness: {error}"))?;

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        match result.0 {
            -1 => return Err("GetMessageW failed".to_string()),
            0 => return Ok(()),
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
}

unsafe fn create_fixture(title: &str) -> Result<HWND, String> {
    let module = GetModuleHandleW(None).map_err(|error| error.to_string())?;
    let instance = HINSTANCE(module.0);
    let class_name = wide(&format!("AiAutoDesktopRustFixture_{}", std::process::id()));
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(fixture_window_proc),
        hInstance: instance,
        hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut std::ffi::c_void),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    if RegisterClassExW(&class) == 0 {
        return Err(windows::core::Error::from_win32().to_string());
    }

    let title = wide(title);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        560,
        300,
        None,
        None,
        instance,
        None,
    )
    .map_err(|error| error.to_string())?;

    create_control(
        hwnd,
        instance,
        "EDIT",
        "Draft",
        WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        24,
        24,
        300,
        28,
        EDIT_ID,
    )?;
    create_control(
        hwnd,
        instance,
        "BUTTON",
        INVOKE_BUTTON,
        WS_TABSTOP,
        24,
        68,
        180,
        32,
        INVOKE_ID,
    )?;
    create_control(
        hwnd,
        instance,
        "STATIC",
        INITIAL_STATUS,
        WINDOW_STYLE::default(),
        24,
        116,
        300,
        28,
        STATUS_ID,
    )?;
    create_control(
        hwnd,
        instance,
        "BUTTON",
        DUPLICATE_BUTTON,
        WS_TABSTOP,
        24,
        164,
        180,
        32,
        DUPLICATE_ONE_ID,
    )?;
    create_control(
        hwnd,
        instance,
        "BUTTON",
        DUPLICATE_BUTTON,
        WS_TABSTOP,
        220,
        164,
        180,
        32,
        DUPLICATE_TWO_ID,
    )?;
    create_control(
        hwnd,
        instance,
        "BUTTON",
        POINTER_BUTTON,
        WS_TABSTOP,
        24,
        212,
        180,
        32,
        POINTER_ID,
    )?;

    let _ = ShowWindow(hwnd, SHOW_WINDOW_CMD(5));
    if !UpdateWindow(hwnd).as_bool() {
        return Err(windows::core::Error::from_win32().to_string());
    }
    Ok(hwnd)
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_control(
    parent: HWND,
    instance: HINSTANCE,
    class_name: &str,
    text: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: i32,
) -> Result<HWND, String> {
    let class_name = wide(class_name);
    let text = wide(text);
    CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(text.as_ptr()),
        style | WS_CHILD | WS_VISIBLE,
        x,
        y,
        width,
        height,
        parent,
        HMENU(id as usize as *mut std::ffi::c_void),
        instance,
        None,
    )
    .map_err(|error| error.to_string())
}

fn snapshot(driver: &UiaDriver, window_id: &str) -> Value {
    driver
        .call(
            "snapshot",
            &json!({"window_id": window_id, "max_depth": 12, "max_nodes": 100}),
        )
        .expect("capture the native fixture")
}

fn find(driver: &UiaDriver, snapshot: &Value, locator: Value) -> Value {
    driver
        .call(
            "find",
            &json!({
                "snapshot_id": snapshot["snapshot_id"],
                "locator": locator,
            }),
        )
        .expect("find one fixture control")
}

fn has_node(snapshot: &Value, role: &str, name: &str) -> bool {
    snapshot["nodes"].as_array().is_some_and(|nodes| {
        nodes
            .iter()
            .any(|node| node["role"] == role && node["name"] == name)
    })
}

fn wait_for_window(driver: &UiaDriver, title: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let windows = driver
            .call("list_windows", &json!({}))
            .expect("enumerate windows");
        let matches: Vec<&Value> = windows["windows"]
            .as_array()
            .expect("window list")
            .iter()
            .filter(|window| window["title"] == title)
            .collect();
        if let [window] = matches.as_slice() {
            return window["window_id"].as_str().expect("window id").to_string();
        }
        assert!(Instant::now() < deadline, "fixture window was not found");
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_node(driver: &UiaDriver, window_id: &str, role: &str, name: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let captured = snapshot(driver, window_id);
        if has_node(&captured, role, name) {
            return captured;
        }
        assert!(
            Instant::now() < deadline,
            "fresh snapshot never observed role={role:?} name={name:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_value(driver: &UiaDriver, window_id: &str, value: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let captured = snapshot(driver, window_id);
        let edit = find(driver, &captured, json!({"role": "edit"}));
        if edit["node"]["value"] == value {
            return captured;
        }
        assert!(
            Instant::now() < deadline,
            "fresh snapshot never observed edit value {value:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "requires an interactive Windows desktop; CI opts in explicitly"]
fn native_driver_observes_and_operates_real_win32_controls() {
    let title = format!("AI Auto Desktop Rust UIA Fixture {}", uuid::Uuid::new_v4());
    let fixture = Fixture::start(title.clone()).expect("start Win32 fixture");
    let driver = native_driver().expect("create native UIA driver");
    let window_id = wait_for_window(&driver, &title);
    assert_eq!(window_id, format!("hwnd:{}", fixture.hwnd.0 as isize));

    let mut captured = snapshot(&driver, &window_id);
    assert_eq!(captured["truncated"], false);

    let duplicate = json!({"role": "button", "name": DUPLICATE_BUTTON});
    let error = driver
        .call(
            "find",
            &json!({"snapshot_id": captured["snapshot_id"], "locator": duplicate}),
        )
        .expect_err("duplicate controls must remain ambiguous");
    assert_eq!(error.code, "DRIVER.AMBIGUOUS_MATCH");
    assert_eq!(error.details["match_count"], 2);
    assert!(has_node(&captured, "text", INITIAL_STATUS));

    let edit_locator = json!({"role": "edit"});
    let edit = find(&driver, &captured, edit_locator.clone());
    assert!(edit["node"]["actions"]
        .as_array()
        .is_some_and(|actions| actions.iter().any(|action| action == "set_value")));
    driver
        .call(
            "set_value",
            &json!({"target": edit["target"], "value": "Written by Rust"}),
        )
        .expect("set a real Win32 edit through ValuePattern");

    captured = wait_for_value(&driver, &window_id, "Written by Rust");
    let edit = find(&driver, &captured, edit_locator.clone());
    assert_eq!(edit["node"]["value"], "Written by Rust");
    driver
        .call("focus", &json!({"target": edit["target"]}))
        .expect("focus the native edit");

    captured = snapshot(&driver, &window_id);
    let invoke = find(
        &driver,
        &captured,
        json!({"role": "button", "name": INVOKE_BUTTON}),
    );
    driver
        .call("invoke", &json!({"target": invoke["target"]}))
        .expect("invoke a real Win32 button through InvokePattern");
    captured = wait_for_node(&driver, &window_id, "text", INVOKED_STATUS);
    assert!(has_node(&captured, "text", INVOKED_STATUS));

    let edit = find(&driver, &captured, edit_locator.clone());
    driver
        .call("set_value", &json!({"target": edit["target"], "value": ""}))
        .expect("clear edit before typing");
    captured = wait_for_value(&driver, &window_id, "");
    let edit = find(&driver, &captured, edit_locator.clone());
    driver
        .call(
            "type_text",
            &json!({"target": edit["target"], "text": "Rust Unicode: 你好"}),
        )
        .expect("type Unicode through SendInput");
    captured = wait_for_value(&driver, &window_id, "Rust Unicode: 你好");
    let edit = find(&driver, &captured, edit_locator);
    assert_eq!(edit["node"]["value"], "Rust Unicode: 你好");

    let pointer = find(
        &driver,
        &captured,
        json!({"role": "button", "name": POINTER_BUTTON}),
    );
    driver
        .call(
            "pointer_click",
            &json!({"target": pointer["target"], "button": "left"}),
        )
        .expect("click a real Win32 button through SendInput");
    captured = wait_for_node(&driver, &window_id, "text", POINTER_STATUS);
    assert!(has_node(&captured, "text", POINTER_STATUS));
}
