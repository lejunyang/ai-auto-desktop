//! The control session against the real Windows desktop, end to end.
//!
//! Every window here is created by this test binary -- the launch test starts
//! a second copy of it as the program to launch -- and closed again. Run with
//! `--ignored --test-threads=1`: the tests take the foreground.

#![cfg(windows)]

use aad_control::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassExW, ShowWindow, TranslateMessage, CW_USEDEFAULT, MSG, SHOW_WINDOW_CMD,
    WINDOW_EX_STYLE, WM_DESTROY, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// Set by the launching test; the launched copy opens a window with it.
const CHILD_TITLE: &str = "AAD_LAUNCH_CHILD_TITLE";
const CHILD_TEST: &str = "launched_window_child";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn child_window_proc(
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

/// The program the launch test starts: this binary, running only this test.
/// Anywhere else (no title in the environment, or not asked for by name) it
/// does nothing.
#[test]
#[ignore = "the window desktop_launch starts; does nothing unless launched"]
fn launched_window_child() {
    let Ok(title) = std::env::var(CHILD_TITLE) else {
        return;
    };
    if !std::env::args().any(|arg| arg == CHILD_TEST) {
        return;
    }
    unsafe {
        let module = GetModuleHandleW(None).unwrap();
        let class_name = wide(&format!("AadLaunchedChild_{}", std::process::id()));
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(child_window_proc),
            hInstance: HINSTANCE(module.0),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut std::ffi::c_void),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        assert_ne!(RegisterClassExW(&class), 0);
        let title = wide(&title);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            320,
            None,
            None,
            HINSTANCE(module.0),
            None,
        )
        .unwrap();
        let _ = ShowWindow(hwnd, SHOW_WINDOW_CMD(5));
        // Close ourselves eventually even if the parent never does.
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut message = MSG::default();
        while Instant::now() < deadline && GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn run(session: &Session, operation: Operation, timeout_ms: u64) -> Response {
    let mut request = Request::new(Some(session.id().into()), operation);
    request.timeout_ms = timeout_ms;
    session.execute(&request)
}

fn data(response: Response) -> Value {
    assert!(response.error.is_none(), "{:?}", response.error);
    response.data.unwrap()
}

fn code(response: Response) -> String {
    response.error.expect("refused")["code"]
        .as_str()
        .unwrap()
        .to_string()
}

fn launch(program: &str) -> Operation {
    Operation::Launch {
        program: program.into(),
        args: vec![
            CHILD_TEST.into(),
            "--exact".into(),
            "--ignored".into(),
            "--test-threads=1".into(),
            "--quiet".into(),
        ],
        wait_ms: Some(20_000),
    }
}

#[test]
#[ignore = "requires an interactive Windows desktop; starts and closes its own window"]
fn a_launched_program_answers_its_window_which_can_be_observed_captured_and_closed() {
    let program = std::env::current_exe().unwrap().display().to_string();
    let title = format!("AI Auto Desktop launched {}", uuid::Uuid::new_v4());
    std::env::set_var(CHILD_TITLE, &title);

    // Neither input alone nor an allowlist alone is enough.
    let read_only = Session::native(Policy {
        launch: [program.clone()].into(),
        ..Default::default()
    });
    assert_eq!(code(run(&read_only, launch(&program), 5_000)), "POLICY.DENIED");
    assert!(data(run(&read_only, Operation::Capabilities {}, 5_000))
        .get("launch")
        .is_none());
    let unlisted = Session::native(Policy {
        allow_input: true,
        ..Default::default()
    });
    assert_eq!(code(run(&unlisted, launch(&program), 5_000)), "POLICY.DENIED");
    // A shell is refused even when the operator allowlisted it.
    let shell = Session::native(Policy {
        allow_input: true,
        launch: ["cmd.exe".to_string()].into(),
        ..Default::default()
    });
    assert_eq!(code(run(&shell, launch("cmd.exe"), 5_000)), "POLICY.DENIED");

    let session = Session::native(Policy {
        allow_input: true,
        launch: [program.clone()].into(),
        ..Default::default()
    });
    let capabilities = data(run(&session, Operation::Capabilities {}, 5_000));
    assert_eq!(capabilities["launch"], json!([program]));
    let launched = data(run(&session, launch(&program), 25_000));
    std::env::remove_var(CHILD_TITLE);
    assert_eq!(launched["window_appeared"], true, "{launched}");
    let resource = launched["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == title.as_str())
        .unwrap_or_else(|| panic!("no window titled {title}: {launched}"))
        .clone();
    assert_eq!(resource["metadata"]["process_id"], launched["process_id"]);

    // The answered resource is observable without listing targets again.
    let resource_id = resource["resource_id"].as_str().unwrap().to_string();
    let observed = data(run(
        &session,
        Operation::Observe {
            resource_id: resource_id.clone(),
            max_nodes: 200,
            view: View::default(),
        },
        10_000,
    ));
    let root = observed["nodes"][0].clone();
    let capabilities: Vec<&str> = root["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for expected in ["activate", "close_window", "minimize", "maximize"] {
        assert!(capabilities.contains(&expected), "{capabilities:?}");
    }
    let target: Target = serde_json::from_value(root["target"].clone()).unwrap();

    // Read-only operations go through the same session.
    let shot = data(run(
        &session,
        Operation::Capture {
            target: target.clone(),
        },
        10_000,
    ));
    assert_eq!(shot["format"], "png");
    assert!(shot["data"].as_str().unwrap().starts_with("iVBORw0KGgo"));
    assert_eq!(shot["digest"].as_str().unwrap().len(), 64);
    assert_eq!(shot["provider"], "desktop.windows_uia");

    let closed = data(run(
        &session,
        Operation::Act {
            target,
            action: Action::CloseWindow {},
        },
        10_000,
    ));
    assert_eq!(closed["backend_result"]["window"]["closed"], true, "{closed}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let targets = data(run(&session, Operation::Targets {}, 10_000));
        let still = targets["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["resource_id"] == resource_id.as_str());
        if !still {
            break;
        }
        assert!(Instant::now() < deadline, "the launched window stayed open");
        std::thread::sleep(Duration::from_millis(200));
    }
}
