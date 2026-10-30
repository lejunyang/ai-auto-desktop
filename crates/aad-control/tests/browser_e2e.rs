#![cfg(any(target_os = "linux", windows))]
//! Real, sandboxed Chromium against a private loopback page and empty profile.
//!
//! Linux CI points AAD_TEST_CHROME at a sandboxed Chromium; on Windows any
//! installed Chrome or Edge works, since the profile is a fresh temporary one.
use aad_control::{browser::BrowserBackend, *};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const PAGE: &str = r#"<!doctype html><html><body>
<form onsubmit="event.preventDefault()"><input id="name" aria-label="Name" oninput="document.getElementById('status').value='fill:'+this.value" onkeydown="if(event.key==='Enter')document.getElementById('status').value='key:'+event.isTrusted">
<button id="save" aria-label="Save" type="button" onclick="document.getElementById('status').value='clicked:'+event.isTrusted">Save</button></form>
<output id="status" aria-label="Status">ready</output>
<input id="protected" aria-label="Protected" type="password" value="redacted-fixture">
<button aria-label="Duplicate">a</button><button aria-label="Duplicate">b</button>
<select aria-label="Size" onchange="document.getElementById('status').value='size:'+this.value"><option value="s">Small</option><option value="l">Large</option></select>
<input type="checkbox" aria-label="Agree" onchange="document.getElementById('status').value='agree:'+this.checked">
<input type="radio" name="tier" aria-label="Basic" checked><input type="radio" name="tier" aria-label="Pro">
<button aria-label="Pointer" onmousedown="document.getElementById('status').value='down:'+event.button+':'+event.isTrusted" ondblclick="document.getElementById('status').value='dbl:'+event.isTrusted" onmouseenter="document.getElementById('status').value='hover:'+event.isTrusted">Pointer</button>
<div aria-label="Log" style="height:40px;overflow:auto" onscroll="document.getElementById('status').value='scrolled:'+(this.scrollTop>0)"><div style="height:400px">rows</div></div>
<div style="position:relative"><button aria-label="Covered">Covered</button><div style="position:absolute;inset:0;background:white"></div></div>
<a href="/second" aria-label="Second page">second</a>
<button aria-label="Popup" onclick="window.open('/second','_blank','popup')">Popup</button>
<iframe src="/frame"></iframe>
</body></html>"#;

struct Fixture {
    child: Child,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
    port: u16,
    url: String,
    page_id: String,
}
impl Fixture {
    fn new() -> Self {
        let binary = std::env::var_os("AAD_TEST_CHROME")
            .expect("set AAD_TEST_CHROME to a sandbox-capable Chromium binary");
        let root = std::env::temp_dir().join(format!("aad-browser-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let server = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut bytes = [0u8; 8192];
                        let count = stream.read(&mut bytes).unwrap_or(0);
                        let request = String::from_utf8_lossy(&bytes[..count]);
                        let body = if request.starts_with("GET /frame") {
                            "<!doctype html><input aria-label='FrameInput'><div style='height:900px'></div>"
                        } else if request.starts_with("GET /second") {
                            "<!doctype html><title>Second</title><body><p aria-label='Second marker'>second</p></body>"
                        } else {
                            PAGE
                        };
                        let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        let mut command = Command::new(binary);
        command
            .args([
                "--headless=new",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
            ])
            .arg(format!("--user-data-dir={}", root.display()))
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // Reclaim only this fixture's own process group, never all Chrome processes.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().unwrap();
        let mut fixture = Self {
            child,
            root,
            stop,
            server: Some(server),
            port: 0,
            url,
            page_id: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = std::fs::read_to_string(fixture.root.join("DevToolsActivePort")) {
                if let Some(port) = text.lines().next().and_then(|s| s.parse().ok()) {
                    fixture.port = port;
                    break;
                }
            }
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "sandboxed Chromium exited before publishing its private debug port"
            );
            assert!(Instant::now() < deadline, "Chromium startup timed out");
            std::thread::sleep(Duration::from_millis(25));
        }
        // Bind the raw verifier to one explicit page identity. URL discovery is
        // only for initial startup; navigation can temporarily publish an empty
        // URL and must never redirect this verifier to another page.
        loop {
            if let Ok(targets) = aad_cdp::Client::list_targets(fixture.port) {
                if let Some(target) = targets.iter().find(|t| t.url.starts_with(&fixture.url)) {
                    fixture.page_id = target.id.clone();
                    break;
                }
            }
            assert!(Instant::now() < deadline, "fixture page startup timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
        fixture
    }
    fn client(&self) -> aad_cdp::Client {
        let target = aad_cdp::Client::list_targets(self.port)
            .unwrap()
            .into_iter()
            .find(|t| t.id == self.page_id)
            .expect("the fixture's explicit page target disappeared");
        aad_cdp::Client::attach(&target).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.server.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn call(session: &Session, op: Operation) -> Response {
    session.execute(&Request::new(Some(session.id().into()), op))
}
fn observe(session: &Session, resource: &str) -> Value {
    let response = call(
        session,
        Operation::Observe {
            resource_id: resource.into(),
            max_nodes: 500,
            view: Default::default(),
        },
    );
    assert!(response.error.is_none(), "{response:?}");
    response.data.unwrap()
}
fn find(session: &Session, observation: &Value, role: &str, name: &str) -> Target {
    let response = call(
        session,
        Operation::Find {
            observation_id: observation["observation_id"].as_str().unwrap().into(),
            locator: Box::new(Locator {
                name: Some(name.into()),
                role: Some(role.into()),
                ..Default::default()
            }),
            expect: Default::default(),
        },
    );
    assert!(response.error.is_none(), "find {role}/{name}: {response:?}");
    serde_json::from_value(response.data.unwrap()["target"].clone()).unwrap()
}
fn document(session: &Session, observation: &Value) -> Target {
    let response = call(
        session,
        Operation::Find {
            observation_id: observation["observation_id"].as_str().unwrap().into(),
            locator: Box::new(Locator {
                role: Some("document".into()),
                ..Default::default()
            }),
            expect: Default::default(),
        },
    );
    assert!(response.error.is_none(), "find document: {response:?}");
    serde_json::from_value(response.data.unwrap()["target"].clone()).unwrap()
}
fn act(session: &Session, target: Target, action: Action) -> Response {
    call(session, Operation::Act { target, action })
}
fn node<'a>(observation: &'a Value, name: &str) -> &'a Value {
    observation["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == name)
        .unwrap_or_else(|| panic!("no node named {name}"))
}
fn actions(observation: &Value, name: &str) -> Vec<String> {
    serde_json::from_value(node(observation, name)["actions"].clone()).unwrap()
}
fn status(observation: &Value) -> &str {
    observation["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "Status")
        .unwrap()["value"]
        .as_str()
        .unwrap()
}

/// Sessions hold the per-user input lease until they close, so the browser
/// tests take turns instead of refusing each other with RESOURCE_BUSY.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}
fn session_for(fixture: &Fixture) -> (Session, Arc<BrowserBackend>) {
    let backend = Arc::new(BrowserBackend::new(fixture.port));
    let session = Session::new(
        vec![backend.clone()],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    (session, backend)
}
/// The resources listed now, after control_targets.
fn targets(session: &Session) -> Vec<Value> {
    let response = call(session, Operation::Targets {});
    assert!(response.error.is_none(), "{response:?}");
    response.data.unwrap()["resources"]
        .as_array()
        .unwrap()
        .clone()
}
/// The main frame of the page with this id.
fn main_of(session: &Session, page_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = targets(session)
            .iter()
            .find(|r| r["metadata"]["page_id"] == page_id && r["metadata"]["main_frame"] == true)
        {
            return found["resource_id"].as_str().unwrap().to_string();
        }
        assert!(Instant::now() < deadline, "page {page_id} was not listed");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn call_within(session: &Session, op: Operation, timeout_ms: u64) -> Response {
    let mut request = Request::new(Some(session.id().into()), op);
    request.timeout_ms = timeout_ms;
    session.execute(&request)
}
fn wait_until(session: &Session, resource: &str, name: &str, condition: WaitCondition) -> Response {
    call_within(
        session,
        Operation::Wait {
            resource_id: resource.into(),
            locator: Box::new(Locator {
                name: Some(name.into()),
                ..Default::default()
            }),
            condition,
            states: None,
            max_nodes: 500,
            interval_ms: Some(100),
        },
        10_000,
    )
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn wait_returns_an_element_that_appears_after_a_timer_ready_to_act_on() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    // The fixture may still be committing its first document; a timer set in
    // the outgoing one would never fire.
    let loaded = wait_until(&session, &main, "Name", WaitCondition::Appears);
    assert!(loaded.error.is_none(), "{loaded:?}");
    fixture
        .client()
        .eval(
            "setTimeout(() => { const b = document.createElement('button'); \
             b.setAttribute('aria-label', 'Late'); b.textContent = 'Late'; \
             b.onclick = () => document.getElementById('status').value = 'late:clicked'; \
             document.body.appendChild(b); }, 700); true",
        )
        .unwrap();
    let started = Instant::now();
    let response = wait_until(&session, &main, "Late", WaitCondition::Appears);
    assert!(response.error.is_none(), "{response:?}");
    let data = response.data.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert!(data["polls"].as_u64().unwrap() >= 2, "{data}");
    assert_eq!(data["node"]["role"], "button");
    let target: Target = serde_json::from_value(data["target"].clone()).unwrap();
    let clicked = act(&session, target, Action::click());
    assert!(clicked.error.is_none(), "{clicked:?}");
    assert_eq!(status(&observe(&session, &main)), "late:clicked");
    fixture
        .client()
        .eval("setTimeout(() => document.querySelector('[aria-label=Late]').remove(), 500); true")
        .unwrap();
    let gone = wait_until(&session, &main, "Late", WaitCondition::Disappears);
    assert_eq!(gone.data.unwrap()["satisfied"], true);
    let never = call_within(
        &session,
        Operation::Wait {
            resource_id: main.clone(),
            locator: Box::new(Locator {
                name: Some("Never there".into()),
                ..Default::default()
            }),
            condition: WaitCondition::Appears,
            states: None,
            max_nodes: 500,
            interval_ms: Some(100),
        },
        1_000,
    );
    assert_eq!(never.error.unwrap()["code"], "CONTROL.WAIT_TIMEOUT");
}

fn visibility(fixture: &Fixture, page_id: &str) -> String {
    let target = aad_cdp::Client::list_targets(fixture.port)
        .unwrap()
        .into_iter()
        .find(|t| t.id == page_id)
        .expect("page is listed");
    aad_cdp::Client::attach(&target)
        .unwrap()
        .eval("document.visibilityState")
        .unwrap()["result"]["value"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn tabs_open_front_and_close_and_popups_name_their_opener() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Name", WaitCondition::Appears)
        .error
        .is_none());
    let observed = observe(&session, &main);
    let offered = &observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["role"] == "document")
        .unwrap()["capabilities"];
    for action in ["open_tab", "activate_page", "close_page"] {
        assert!(
            offered.as_array().unwrap().iter().any(|a| a == action),
            "{action} on the document: {offered}"
        );
    }
    let body = document(&session, &observed);
    let refused = act(
        &session,
        body,
        Action::OpenTab {
            url: "javascript:alert(1)".into(),
        },
    );
    assert_eq!(refused.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
    let body = document(&session, &observe(&session, &main));
    let opened = act(
        &session,
        body,
        Action::OpenTab {
            url: format!("{}second", fixture.url),
        },
    );
    assert!(opened.error.is_none(), "{opened:?}");
    let tab = opened.data.unwrap()["backend_result"]["page_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(tab, fixture.page_id);
    let second = main_of(&session, &tab);
    let arrived = wait_until(&session, &second, "Second marker", WaitCondition::Appears);
    assert!(arrived.error.is_none(), "{arrived:?}");
    assert_eq!(visibility(&fixture, &tab), "visible");
    assert_eq!(visibility(&fixture, &fixture.page_id), "hidden");

    // Front the first page again, then close the tab from its own document.
    let body = document(&session, &observe(&session, &main));
    let fronted = act(&session, body, Action::ActivatePage {});
    assert!(fronted.error.is_none(), "{fronted:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while visibility(&fixture, &fixture.page_id) != "visible" {
        assert!(
            Instant::now() < deadline,
            "activate_page did not front the page"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let body = document(&session, &observe(&session, &second));
    let closed = act(&session, body, Action::ClosePage {});
    assert!(closed.error.is_none(), "{closed:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while targets(&session)
        .iter()
        .any(|r| r["metadata"]["page_id"] == tab.as_str())
    {
        assert!(Instant::now() < deadline, "close_page left the tab open");
        std::thread::sleep(Duration::from_millis(50));
    }

    // A popup the page opens is listed with the page that opened it. A
    // popup needs a user gesture, so the click is trusted pointer input.
    let observed = observe(&session, &main);
    let popup = find(&session, &observed, "button", "Popup");
    let clicked = act(
        &session,
        popup,
        Action::Click {
            button: None,
            count: None,
            fraction: Some(Fraction { fx: 0.5, fy: 0.5 }),
        },
    );
    assert!(clicked.error.is_none(), "{clicked:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let popup = loop {
        if let Some(found) = targets(&session).into_iter().find(|r| {
            r["metadata"]["opener_page_id"] == fixture.page_id.as_str()
                && r["metadata"]["main_frame"] == true
        }) {
            break found;
        }
        assert!(Instant::now() < deadline, "the popup was not listed");
        std::thread::sleep(Duration::from_millis(50));
    };
    let resource = popup["resource_id"].as_str().unwrap();
    let arrived = wait_until(&session, resource, "Second marker", WaitCondition::Appears);
    assert!(arrived.error.is_none(), "{arrived:?}");
    // The opener itself was opened by nobody.
    assert!(targets(&session)
        .iter()
        .filter(|r| r["metadata"]["page_id"] == fixture.page_id.as_str())
        .all(|r| r["metadata"]["opener_page_id"].is_null()));
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn browser_actions_are_observed_document_bound_and_have_real_page_effects() {
    let _serial = serial();
    let fixture = Fixture::new();
    let session = Session::new(
        vec![Arc::new(BrowserBackend::new(fixture.port))],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let resources = loop {
        let response = call(&session, Operation::Targets {});
        let data = response.data.unwrap();
        if data["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["metadata"]["main_frame"] == false)
        {
            break data;
        }
        assert!(Instant::now() < deadline, "iframe did not load: {data}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let main = resources["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["metadata"]["main_frame"] == true)
        .unwrap()["resource_id"]
        .as_str()
        .unwrap();
    let frame = resources["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["metadata"]["main_frame"] == false)
        .unwrap()["resource_id"]
        .as_str()
        .unwrap();
    let first = observe(&session, main);
    let target = find(&session, &first, "textbox", "Name");
    // A read-only peer must have its own isolated world, not overwrite this
    // connection's node/observation map merely by observing the same document.
    let reader = Session::new(
        vec![Arc::new(BrowserBackend::new(fixture.port))],
        Policy::default(),
    );
    call(&reader, Operation::Targets {});
    observe(&reader, main);
    let protected = first["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Protected")
        .unwrap();
    assert!(protected["value"].is_null());
    assert_eq!(protected["actions"], json!([]));
    assert_eq!(
        call(
            &session,
            Operation::Find {
                observation_id: first["observation_id"].as_str().unwrap().into(),
                locator: Box::new(Locator {
                    name: Some("Duplicate".into()),
                    ..Default::default()
                }),
                expect: Default::default(),
            }
        )
        .error
        .unwrap()["code"],
        "CONTROL.AMBIGUOUS"
    );
    assert!(act(
        &session,
        target,
        Action::Fill {
            value: "hello from fixture".into()
        }
    )
    .error
    .is_none());
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "fill:hello from fixture");
    let target = find(&session, &observed, "textbox", "Name");
    let result = act(
        &session,
        target,
        Action::Key {
            keys: vec!["Enter".into()],
        },
    );
    assert!(result.error.is_none(), "{result:?}");
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "key:true");
    let target = find(&session, &observed, "button", "Save");
    assert!(act(&session, target, Action::click()).error.is_none());
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "clicked:false");
    // Choices: a list box by label or value, a checkbox both ways, a radio only on.
    let target = find(&session, &observed, "combobox", "Size");
    assert_eq!(
        node(&observed, "Size")["options"],
        json!(["Small", "Large"])
    );
    let result = act(
        &session,
        target,
        Action::Select {
            option: "Large".into(),
        },
    );
    assert!(result.error.is_none(), "{result:?}");
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "size:l");
    let target = find(&session, &observed, "combobox", "Size");
    let missing = act(
        &session,
        target,
        Action::Select {
            option: "Huge".into(),
        },
    );
    assert_eq!(missing.error.unwrap()["code"], "CONTROL.OPTION_NOT_FOUND");
    let observed = observe(&session, main);
    let target = find(&session, &observed, "checkbox", "Agree");
    assert_eq!(node(&observed, "Agree")["states"]["checked"], false);
    assert!(act(&session, target, Action::Check { checked: true })
        .error
        .is_none());
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "agree:true");
    let target = find(&session, &observed, "checkbox", "Agree");
    let unchanged = act(&session, target, Action::Check { checked: true });
    assert_eq!(unchanged.data.unwrap()["backend_result"]["unchanged"], true);
    let observed = observe(&session, main);
    let target = find(&session, &observed, "radio", "Basic");
    assert_eq!(
        act(&session, target, Action::Check { checked: false })
            .error
            .unwrap()["code"],
        "CONTROL.ACTION_UNSUPPORTED",
        "a radio is cleared by checking another"
    );

    // Trusted pointer input: right button, double click, hover and the wheel.
    let observed = observe(&session, main);
    let target = find(&session, &observed, "button", "Pointer");
    let right = act(
        &session,
        target,
        Action::Click {
            button: Some(Button::Right),
            count: None,
            fraction: None,
        },
    );
    assert!(right.error.is_none(), "{right:?}");
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "down:2:true");
    let target = find(&session, &observed, "button", "Pointer");
    assert!(act(
        &session,
        target,
        Action::Click {
            button: None,
            count: Some(2),
            fraction: None
        }
    )
    .error
    .is_none());
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "dbl:true");
    fixture
        .client()
        .eval("document.getElementById('status').value='ready'")
        .unwrap();
    let observed = observe(&session, main);
    let target = find(&session, &observed, "textbox", "Name");
    // Move the pointer off the button so entering it again is observable.
    assert!(act(
        &session,
        target,
        Action::Hover {
            ms: Some(1),
            fraction: None
        }
    )
    .error
    .is_none());
    let observed = observe(&session, main);
    let target = find(&session, &observed, "button", "Pointer");
    assert!(act(
        &session,
        target,
        Action::Hover {
            ms: Some(50),
            fraction: None
        }
    )
    .error
    .is_none());
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "hover:true");
    assert!(actions(&observed, "Log").contains(&"scroll".to_string()));
    let target = find(&session, &observed, "generic", "Log");
    let wheel = act(
        &session,
        target,
        Action::Scroll {
            direction: Direction::Down,
            notches: Some(2),
            fraction: None,
        },
    );
    assert!(wheel.error.is_none(), "{wheel:?}");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let observed = observe(&session, main);
        if status(&observed) == "scrolled:true" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the wheel did not scroll the log"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    // A point that lands on something laid over the element is refused, not clicked.
    let observed = observe(&session, main);
    let target = find(&session, &observed, "button", "Covered");
    let covered = act(
        &session,
        target,
        Action::Click {
            button: None,
            count: None,
            fraction: Some(Fraction { fx: 0.5, fy: 0.5 }),
        },
    );
    assert_eq!(covered.effect, "not_applied");
    assert_eq!(covered.error.unwrap()["code"], "CONTROL.OCCLUDED");

    let observed = observe(&session, main);
    let target = find(&session, &observed, "button", "Save");
    fixture
        .client()
        .eval("document.getElementById('save').setAttribute('aria-label','Replaced')")
        .unwrap();
    let stale = act(&session, target, Action::click());
    assert_eq!(stale.effect, "not_applied");
    assert_eq!(stale.error.unwrap()["code"], "CONTROL.STALE_TARGET");
    let observed = observe(&session, frame);
    for name in [
        "key", "scroll", "hover", "navigate", "back", "forward", "reload",
    ] {
        assert!(
            observed["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|n| !n["actions"].as_array().unwrap().iter().any(|a| a == name)),
            "{name} must not be offered inside a frame"
        );
    }
    let frame_target = find(&session, &observed, "textbox", "FrameInput");
    let mut forged = frame_target.clone();
    forged.resource_id = main.into();
    assert_eq!(
        act(
            &session,
            forged,
            Action::Fill {
                value: "bad".into()
            }
        )
        .error
        .unwrap()["code"],
        "POLICY.DENIED"
    );
    assert!(act(
        &session,
        frame_target,
        Action::Fill {
            value: "inside frame".into()
        }
    )
    .error
    .is_none());
    let observed = observe(&session, frame);
    assert_eq!(
        observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "FrameInput")
            .unwrap()["value"],
        "inside frame"
    );
    let old_frame_target = find(&session, &observed, "textbox", "FrameInput");
    let old_frame = resources["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["resource_id"] == frame)
        .unwrap()["metadata"]["frame_id"]
        .as_str()
        .unwrap();
    fixture.client().eval("document.querySelector('iframe').remove(); const replacement=document.createElement('iframe'); replacement.src='/frame'; document.body.appendChild(replacement)").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture
        .client()
        .command("Page.getFrameTree", json!({}))
        .unwrap()
        .to_string()
        .contains(old_frame)
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let stale = act(
        &session,
        old_frame_target,
        Action::Fill {
            value: "must not reach replacement frame".into(),
        },
    );
    assert_eq!(stale.commit_state, CommitState::NotCommitted);
    assert_eq!(stale.error.unwrap()["code"], "CONTROL.STALE_TARGET");
    let observed = observe(&session, main);
    let target = find(&session, &observed, "textbox", "Name");
    fixture
        .client()
        .command(
            "Page.navigate",
            json!({"url":format!("{}other",fixture.url)}),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if fixture.client().state().unwrap().url.ends_with("/other") {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let stale = act(
        &session,
        target,
        Action::Fill {
            value: "must not reach new document".into(),
        },
    );
    assert_eq!(stale.effect, "not_applied");
    assert_eq!(stale.error.unwrap()["code"], "CONTROL.STALE_TARGET");

    // Page actions live on the document node and address the main frame.
    let wait_for = |session: &Session, suffix: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if fixture.client().state().unwrap().url.ends_with(suffix) {
                // Let the new document finish building before it is observed.
                std::thread::sleep(Duration::from_millis(100));
                return observe(session, main);
            }
            assert!(Instant::now() < deadline, "did not reach {suffix}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let observed = wait_for(&session, "/other");
    let body = document(&session, &observed);
    let refused = act(
        &session,
        body,
        Action::Navigate {
            url: "file:///etc/passwd".into(),
        },
    );
    assert_eq!(refused.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
    let observed = observe(&session, main);
    let body = document(&session, &observed);
    let url = format!("{}second", fixture.url);
    assert!(act(&session, body, Action::Navigate { url })
        .error
        .is_none());
    let observed = wait_for(&session, "/second");
    assert!(observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["name"] == "Second marker"));
    let body = document(&session, &observed);
    assert!(act(&session, body, Action::Back {}).error.is_none());
    let observed = wait_for(&session, "/other");
    let body = document(&session, &observed);
    assert!(act(&session, body, Action::Forward {}).error.is_none());
    let observed = wait_for(&session, "/second");
    let body = document(&session, &observed);
    let reload = act(&session, body, Action::Reload {});
    assert!(reload.error.is_none(), "{reload:?}");
    let observed = wait_for(&session, "/second");
    let body = document(&session, &observed);
    assert!(
        act(&session, body, Action::Forward {}).error.unwrap()["code"]
            == "CONTROL.ACTION_UNSUPPORTED"
    );
}
