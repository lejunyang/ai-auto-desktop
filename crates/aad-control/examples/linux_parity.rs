//! Only used inside tests/linux's private D-Bus/Xvfb/openbox Qt fixture
//! session: desktop capture, OCR, paste, clipboard, global keys, window
//! actions and launch, each checked by its observable effect.
use aad_control::{transport::Endpoint, *};
use base64::Engine as _;
use serde_json::{json, Value};
use std::process::Command;
use std::time::{Duration, Instant};

fn op(value: Value) -> Operation {
    serde_json::from_value(value).expect("operation")
}
fn call(endpoint: &dyn Endpoint, operation: Value) -> std::result::Result<Value, Value> {
    let response = endpoint.request(&Request::new(
        Some(endpoint.session_id().into()),
        op(operation),
    ));
    match response.error {
        None => Ok(response.data.unwrap()),
        Some(error) => Err(serde_json::to_value(error).unwrap()),
    }
}
fn ok(endpoint: &dyn Endpoint, operation: Value) -> Value {
    let label = operation["method"].clone();
    call(endpoint, operation).unwrap_or_else(|error| panic!("{label}: {error}"))
}

struct Fixture<'a> {
    endpoint: &'a dyn Endpoint,
    resource: String,
}
impl Fixture<'_> {
    fn observe(&self) -> Value {
        ok(
            self.endpoint,
            json!({"method": "observe", "resource_id": self.resource, "max_nodes": 256}),
        )
    }
    fn node(&self, observation: &Value, role: &str, name: Option<&str>) -> Value {
        observation["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["role"] == role && name.is_none_or(|name| n["name"] == name))
            .cloned()
            .unwrap_or_else(|| panic!("no {role} {name:?} in {observation}"))
    }
    /// The top-level window: the application's child with bounds.
    fn window(&self, observation: &Value) -> Value {
        let nodes = observation["nodes"].as_array().unwrap();
        let root = nodes.iter().find(|n| n["parent_id"].is_null()).unwrap();
        nodes
            .iter()
            .find(|n| n["parent_id"] == root["node_id"] && !n["bounds"].is_null())
            .cloned()
            .unwrap_or_else(|| panic!("no top-level window in {observation}"))
    }
    fn until_window(&self, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let window = self.window(&self.observe());
            if check(&window) {
                return window;
            }
            assert!(
                Instant::now() < deadline,
                "window state timed out: {window}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn act(&self, node: &Value, action: Value) -> std::result::Result<Value, Value> {
        call(
            self.endpoint,
            json!({"method": "act", "target": node["target"], "action": action}),
        )
    }
    /// Observe until `check` holds for the named node, within 5 seconds.
    fn until(&self, role: &str, name: Option<&str>, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let observation = self.observe();
            let node = self.node(&observation, role, name);
            if check(&node) {
                return node;
            }
            assert!(Instant::now() < deadline, "postcondition timed out: {node}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn xclip(args: &[&str], input: Option<&str>) -> String {
    use std::io::Write;
    // `xclip -i` stays behind to own the selection: never wait on its stdout.
    let mut child = Command::new("xclip")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(if input.is_some() {
            std::process::Stdio::null()
        } else {
            std::process::Stdio::piped()
        })
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("xclip");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait().unwrap();
        return String::new();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let pid: u64 = args.next().expect("fixture PID").parse().unwrap();
    let fixture_binary = args.next().expect("fixture binary path for launch");
    let session = Session::native(Policy {
        allow_input: true,
        allow_clipboard_read: true,
        allow_global_keys: true,
        launch: [fixture_binary.clone()].into(),
        ..Default::default()
    });
    let endpoint: &dyn Endpoint = &session;
    let capabilities = ok(endpoint, json!({"method": "capabilities"}));
    let linux = capabilities["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "desktop.linux_atspi")
        .unwrap_or_else(|| panic!("no AT-SPI provider: {capabilities}"));
    for read in ["capture", "read_text"] {
        assert!(
            linux["reads"].as_array().unwrap().iter().any(|r| r == read),
            "{read} not advertised: {linux}"
        );
    }
    assert_eq!(capabilities["clipboard_read"], true);
    assert_eq!(capabilities["global_keys"], true);

    let targets = ok(endpoint, json!({"method": "targets"}));
    let resource = targets["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["provider"] == "desktop.linux_atspi" && r["metadata"]["process_id"] == pid)
        .expect("fixture resource")["resource_id"]
        .as_str()
        .unwrap()
        .to_string();
    let fixture = Fixture { endpoint, resource };

    // Capture: the window's frame and one label, at their live bounds.
    let observation = fixture.observe();
    let frame = fixture.window(&observation);
    let shot = ok(
        endpoint,
        json!({"method": "capture", "target": frame["target"]}),
    );
    let png = base64::engine::general_purpose::STANDARD
        .decode(shot["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&png[..4], b"\x89PNG");
    assert_eq!(shot["width"], frame["bounds"]["width"], "{shot:?}");
    assert_eq!(shot["height"], frame["bounds"]["height"]);
    assert_eq!(shot["digest"].as_str().unwrap().len(), 64);
    println!(
        "capture: frame {}x{} png {} bytes",
        shot["width"],
        shot["height"],
        png.len()
    );

    // OCR: the status label reads as its accessible name says.
    let label = fixture.node(&observation, "label", Some("Qt fixture status idle"));
    let read = ok(
        endpoint,
        json!({"method": "read_text", "target": label["target"], "language": "en-US"}),
    );
    let text = read["text"].as_str().unwrap().to_lowercase();
    assert!(text.contains("idle") && text.contains("status"), "{read}");
    assert_eq!(read["engine"], "tesseract");
    println!("read_text: {:?}", read["text"]);
    let unknown = call(
        endpoint,
        json!({"method": "read_text", "target": label["target"], "language": "tlh"}),
    )
    .unwrap_err();
    assert_eq!(unknown["code"], "DRIVER.OCR_UNAVAILABLE", "{unknown}");

    // Paste: the field receives the text, the clipboard is put back.
    xclip(
        &["-selection", "clipboard", "-i"],
        Some("person's clipboard"),
    );
    std::thread::sleep(Duration::from_millis(200));
    let before = ok(endpoint, json!({"method": "clipboard_read"}));
    assert_eq!(before["text"], "person's clipboard", "{before}");
    let entry_name = Some("Qt fixture XTest text entry");
    let entry = fixture.node(&fixture.observe(), "text", entry_name);
    fixture
        .act(&entry, json!({"type": "fill", "value": ""}))
        .unwrap();
    let entry = fixture.until("text", entry_name, |n| n["value"] == "");
    assert!(
        entry["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "paste"),
        "{entry}"
    );
    let pasted = fixture
        .act(&entry, json!({"type": "paste", "text": "pasted 粘贴"}))
        .unwrap_or_else(|e| panic!("paste: {e}"));
    fixture.until("text", entry_name, |n| n["value"] == "pasted 粘贴");
    std::thread::sleep(Duration::from_millis(300));
    let after = ok(endpoint, json!({"method": "clipboard_read"}));
    assert_eq!(after["text"], "person's clipboard", "{after} {pasted}");
    assert_eq!(
        xclip(&["-selection", "clipboard", "-o"], None),
        "person's clipboard"
    );
    println!(
        "paste: field filled, clipboard restored ({})",
        pasted["backend_result"]["backend_result"]["clipboard_restored"]
    );

    // Global keys: Ctrl+A then Backspace on whatever has focus (the entry).
    let entry = fixture.node(&fixture.observe(), "text", entry_name);
    fixture.act(&entry, json!({"type": "focus"})).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    ok(
        endpoint,
        json!({"method": "global_key", "keys": ["ctrl", "a"]}),
    );
    ok(
        endpoint,
        json!({"method": "global_key", "keys": ["backspace"]}),
    );
    fixture.until("text", entry_name, |n| n["value"] == "");
    let reserved = call(
        endpoint,
        json!({"method": "global_key", "keys": ["ctrl", "alt", "backspace"]}),
    )
    .unwrap_err();
    assert_eq!(reserved["code"], "DRIVER.KEY_RESERVED", "{reserved}");
    println!("global_key: Ctrl+A, Backspace cleared the focused field");

    // Window actions, each read back from the window manager.
    let window = |check: &dyn Fn(&Value) -> bool| {
        fixture.until_window(|node| check(&node["provenance"]["window"]))
    };
    let frame = fixture.window(&fixture.observe());
    let normal_width = frame["bounds"]["width"].as_i64().unwrap();
    let offered = |frame: &Value| -> Vec<String> {
        frame["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect()
    };
    assert!(offered(&frame).contains(&"maximize".into()), "{frame}");
    let maximized = fixture.act(&frame, json!({"type": "maximize"})).unwrap();
    assert_eq!(
        maximized["backend_result"]["backend_result"]["observed_state"]["maximized"], true,
        "{maximized}"
    );
    // The application itself sees the larger window.
    let frame = fixture.until_window(|node| {
        node["provenance"]["window"]["maximized"] == true
            && node["bounds"]["width"].as_i64().unwrap_or_default() > normal_width
    });
    assert!(offered(&frame).contains(&"restore".into()), "{frame}");
    fixture.act(&frame, json!({"type": "restore"})).unwrap();
    let frame = window(&|w| w["maximized"] == false);
    let minimized = fixture.act(&frame, json!({"type": "minimize"})).unwrap();
    assert_eq!(
        minimized["backend_result"]["backend_result"]["observed_state"]["minimized"],
        true
    );
    let frame = window(&|w| w["minimized"] == true);
    assert!(!offered(&frame).contains(&"minimize".into()), "{frame}");
    fixture.act(&frame, json!({"type": "restore"})).unwrap();
    let frame = window(&|w| w["minimized"] == false);
    println!("window: maximize, restore, minimize, restore verified via EWMH");

    // Launch: an allowlisted program's window comes back as a resource.
    let launched = ok(
        endpoint,
        json!({"method": "launch", "program": fixture_binary, "wait_ms": 8000}),
    );
    assert_eq!(launched["window_appeared"], true, "{launched}");
    let new_pid = launched["process_id"].as_u64().unwrap();
    let new_resource = launched["resources"][0]["resource_id"]
        .as_str()
        .unwrap()
        .to_string();
    let refused = call(
        endpoint,
        json!({"method": "launch", "program": "/bin/sh", "args": ["-c", "true"]}),
    )
    .unwrap_err();
    assert_eq!(refused["code"], "POLICY.DENIED", "{refused}");
    println!("launch: pid {new_pid} appeared as {new_resource}");

    // Close the launched copy's window and see its process go.
    let launched_fixture = Fixture {
        endpoint,
        resource: new_resource,
    };
    let other = launched_fixture.window(&launched_fixture.observe());
    if offered(&other).contains(&"activate".into()) {
        launched_fixture
            .act(&other, json!({"type": "activate"}))
            .unwrap();
    }
    let other = launched_fixture.window(&launched_fixture.observe());
    launched_fixture
        .act(&other, json!({"type": "close_window"}))
        .unwrap_or_else(|e| panic!("close_window: {e}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::path::Path::new(&format!("/proc/{new_pid}")).exists()
        && std::fs::read_to_string(format!("/proc/{new_pid}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z "))
    {
        assert!(Instant::now() < deadline, "launched fixture did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The original window became active again or at least still answers.
    let _ = frame;
    fixture.observe();
    println!("close_window: launched process {new_pid} exited");
    println!("Linux desktop parity fixture passed");
}
