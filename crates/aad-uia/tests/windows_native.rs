//! End-to-end coverage against real Win32 controls and Windows UI Automation.
//!
//! The fixture is deliberately implemented in Rust and runs in the same test
//! process. This keeps the native provider regression test in the shipped
//! implementation language while still exercising Windows' real UIA bridge.

#![cfg(windows)]

use aad_uia::{native_driver, UiaDriver};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, UpdateWindow, COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_UNAWARE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetDlgItem, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetWindowTextW, ShowWindow, TranslateMessage, BN_CLICKED,
    CW_USEDEFAULT, ES_AUTOHSCROLL, ES_PASSWORD, HMENU, MSG, SHOW_WINDOW_CMD, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_LBUTTONDOWN, WNDCLASSEXW, WS_BORDER,
    WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
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
    // Laid out in 96-DPI pixels: an unaware window is scaled as a whole, so
    // its controls stay inside it whatever the display scale, and whether or
    // not the driver already made this process DPI aware.
    let result = unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE);
        create_fixture(&title)
    };
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
        // The driver reports an empty value as absent, not as "".
        if edit["node"]["value"].as_str().unwrap_or_default() == value {
            return captured;
        }
        assert!(
            Instant::now() < deadline,
            "fresh snapshot never observed edit value {value:?}: {}",
            edit["node"]
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

// ---------------------------------------------------------------------------
// A window of known pixels, for screenshots
// ---------------------------------------------------------------------------

/// The swatch colour, chosen so no theme or font smoothing produces it.
const SWATCH: [u8; 3] = [18, 201, 77];
const SWATCH_NAME: &str = "Swatch green";
/// The swatch's size in the window's own (logical, for an unaware window) pixels.
const SWATCH_SIZE: (i32, i32) = (120, 80);

unsafe extern "system" fn plain_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

type WindowProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// Left-button presses the swatch received.
static SWATCH_CLICKS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn swatch_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_LBUTTONDOWN {
        SWATCH_CLICKS.fetch_add(1, Ordering::SeqCst);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

/// A top-level window holding a solid swatch and a password edit, on its own
/// thread. `unaware` creates it DPI-unaware, so Windows bitmap-scales it on a
/// scaled display while the driver works in physical pixels.
fn start_swatch(title: String, unaware: bool) -> Result<Fixture, String> {
    spawn_window(move || unsafe {
        if unaware {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE);
        }
        create_swatch(&title)
    })
}

/// Run a window created by `create` on its own thread with a message loop.
fn spawn_window(
    create: impl FnOnce() -> Result<HWND, String> + Send + 'static,
) -> Result<Fixture, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || {
        let created = create();
        let hwnd = match created {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = sender.send(Err(error.clone()));
                return Err(error);
            }
        };
        let _ = sender.send(Ok(FixtureReady { hwnd }));
        let mut message = MSG::default();
        loop {
            match unsafe { GetMessageW(&mut message, None, 0, 0) }.0 {
                -1 => return Err("GetMessageW failed".to_string()),
                0 => return Ok(()),
                _ => unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                },
            }
        }
    });
    let ready = receiver
        .recv_timeout(Duration::from_secs(10))
        .map_err(|error| format!("swatch fixture did not become ready: {error}"))??;
    Ok(Fixture {
        hwnd: ready.hwnd,
        thread: Some(thread),
    })
}

unsafe fn create_swatch(title: &str) -> Result<HWND, String> {
    let module = GetModuleHandleW(None).map_err(|error| error.to_string())?;
    let instance = HINSTANCE(module.0);
    let suffix = format!("{}_{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let frame_class = wide(&format!("AadCaptureFrame_{suffix}"));
    let swatch_class = wide(&format!("AadCaptureSwatch_{suffix}"));
    let colour =
        COLORREF(u32::from(SWATCH[0]) | (u32::from(SWATCH[1]) << 8) | (u32::from(SWATCH[2]) << 16));
    for (class_name, brush, procedure) in [
        (
            &frame_class,
            HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut std::ffi::c_void),
            plain_window_proc as WindowProc,
        ),
        (&swatch_class, CreateSolidBrush(colour), swatch_window_proc),
    ] {
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            hbrBackground: brush,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            return Err(windows::core::Error::from_win32().to_string());
        }
    }
    let title = wide(title);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(frame_class.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        420,
        260,
        None,
        None,
        instance,
        None,
    )
    .map_err(|error| error.to_string())?;
    let swatch_text = wide(SWATCH_NAME);
    CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(swatch_class.as_ptr()),
        PCWSTR(swatch_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        30,
        20,
        SWATCH_SIZE.0,
        SWATCH_SIZE.1,
        hwnd,
        HMENU(1 as *mut std::ffi::c_void),
        instance,
        None,
    )
    .map_err(|error| error.to_string())?;
    create_control(
        hwnd,
        instance,
        "EDIT",
        "",
        WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_PASSWORD as u32),
        30,
        130,
        200,
        28,
        2,
    )?;
    let _ = ShowWindow(hwnd, SHOW_WINDOW_CMD(5));
    let _ = UpdateWindow(hwnd);
    Ok(hwnd)
}

fn capture(driver: &UiaDriver, target: &Value) -> Result<(Value, image::RgbImage), String> {
    let shot = driver
        .call("capture", &json!({"target": target}))
        .map_err(|error| error.code)?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(shot["data"].as_str().expect("png data"))
        .expect("valid base64");
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .expect("a decodable PNG")
        .to_rgb8();
    assert_eq!(shot["width"], image.width());
    assert_eq!(shot["height"], image.height());
    Ok((shot, image))
}

fn swatch_pixels(image: &image::RgbImage) -> usize {
    image.pixels().filter(|pixel| pixel.0 == SWATCH).count()
}

fn system_scale() -> f64 {
    f64::from(unsafe { GetDpiForSystem() }) / 96.0
}

#[test]
#[ignore = "requires an interactive Windows desktop; CI opts in explicitly"]
fn a_capture_returns_the_pixels_the_element_shows() {
    // The driver first: it makes the process per-monitor DPI aware, so this
    // window is too and its sizes are physical pixels.
    let driver = native_driver().expect("create native UIA driver");
    let title = format!("AI Auto Desktop capture fixture {}", uuid::Uuid::new_v4());
    let fixture = start_swatch(title.clone(), false).expect("start swatch fixture");
    let window_id = wait_for_window(&driver, &title);
    assert_eq!(window_id, format!("hwnd:{}", fixture.hwnd.0 as isize));
    let captured = wait_for_node(&driver, &window_id, "pane", SWATCH_NAME);

    let swatch = find(
        &driver,
        &captured,
        json!({"role": "pane", "name": SWATCH_NAME}),
    );
    let (shot, image) = capture(&driver, &swatch["target"]).expect("capture the swatch");
    assert_eq!(shot["format"], "png");
    assert_eq!(shot["clipped"], false);
    assert_eq!(
        (image.width(), image.height()),
        (SWATCH_SIZE.0 as u32, SWATCH_SIZE.1 as u32)
    );
    assert_eq!(swatch_pixels(&image), image.pixels().count(), "every pixel");

    // The window's root node is the whole window: the swatch sits inside it,
    // unscaled, and the window's background around it.
    let root = captured["nodes"][0].clone();
    assert!(root["parent_id"].is_null());
    let target = json!({"snapshot_id": captured["snapshot_id"], "revision": captured["revision"], "node_id": root["node_id"]});
    let (_, window) = capture(&driver, &target).expect("capture the window");
    assert!(window.width() > SWATCH_SIZE.0 as u32 && window.height() > SWATCH_SIZE.1 as u32);
    assert_eq!(
        swatch_pixels(&window),
        (SWATCH_SIZE.0 * SWATCH_SIZE.1) as usize
    );

    // The same pixels give the same PNG, so its digest tells change apart.
    let (again, _) = capture(&driver, &swatch["target"]).expect("capture again");
    assert_eq!(again["data"], shot["data"]);

    let secret = find(
        &driver,
        &captured,
        json!({"role": "edit", "states": {"protected": true}}),
    );
    assert_eq!(
        capture(&driver, &secret["target"]).unwrap_err(),
        "DRIVER.PROTECTED_ELEMENT"
    );

    unsafe {
        // SW_SHOWMINNOACTIVE: leave activation alone for the tests after this.
        let _ = ShowWindow(fixture.hwnd, SHOW_WINDOW_CMD(7));
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        capture(&driver, &swatch["target"]).unwrap_err(),
        "DRIVER.WINDOW_MINIMIZED"
    );
}

#[test]
#[ignore = "requires an interactive Windows desktop; CI opts in explicitly"]
fn a_dpi_unaware_window_is_captured_in_physical_pixels() {
    let driver = native_driver().expect("create native UIA driver");
    let title = format!("AI Auto Desktop unaware fixture {}", uuid::Uuid::new_v4());
    let _fixture = start_swatch(title.clone(), true).expect("start unaware fixture");
    let window_id = wait_for_window(&driver, &title);
    let captured = wait_for_node(&driver, &window_id, "pane", SWATCH_NAME);
    let swatch = find(
        &driver,
        &captured,
        json!({"role": "pane", "name": SWATCH_NAME}),
    );
    // UIA reports the scaled, physical box, and the capture matches it.
    let bounds = &swatch["node"]["bounds"];
    let scale = system_scale();
    let expected = (
        (f64::from(SWATCH_SIZE.0) * scale).round() as i64,
        (f64::from(SWATCH_SIZE.1) * scale).round() as i64,
    );
    assert!(
        (bounds["width"].as_i64().unwrap() - expected.0).abs() <= 1
            && (bounds["height"].as_i64().unwrap() - expected.1).abs() <= 1,
        "bounds {bounds} at scale {scale}"
    );
    let (_, image) = capture(&driver, &swatch["target"]).expect("capture the swatch");
    assert_eq!(i64::from(image.width()), bounds["width"].as_i64().unwrap());
    assert_eq!(
        i64::from(image.height()),
        bounds["height"].as_i64().unwrap()
    );
    // Scaling blends the outermost pixels; the inside is the exact colour.
    let (w, h) = (image.width(), image.height());
    for (x, y) in [(w / 2, h / 2), (w / 4, h / 4), (w * 3 / 4, h * 3 / 4)] {
        assert_eq!(image.get_pixel(x, y).0, SWATCH, "pixel {x},{y} of {w}x{h}");
    }
    assert!(swatch_pixels(&image) * 10 >= (w * h) as usize * 8);
}
