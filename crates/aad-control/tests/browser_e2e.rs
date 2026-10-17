#![cfg(target_os = "linux")]
//! Real, sandboxed Chromium against a private loopback page and empty profile.
use aad_control::{browser::BrowserBackend, *};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
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
                        let frame =
                            String::from_utf8_lossy(&bytes[..count]).starts_with("GET /frame");
                        let body = if frame {
                            "<!doctype html><input aria-label='FrameInput'>"
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
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
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
            locator: Locator {
                name: Some(name.into()),
                role: Some(role.into()),
                ..Default::default()
            },
        },
    );
    assert!(response.error.is_none(), "find {role}/{name}: {response:?}");
    serde_json::from_value(response.data.unwrap()["target"].clone()).unwrap()
}
fn act(session: &Session, target: Target, action: Action) -> Response {
    call(session, Operation::Act { target, action })
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

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn browser_actions_are_observed_document_bound_and_have_real_page_effects() {
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
                locator: Locator {
                    name: Some("Duplicate".into()),
                    ..Default::default()
                }
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
    assert!(act(&session, target, Action::Click {}).error.is_none());
    let observed = observe(&session, main);
    assert_eq!(status(&observed), "clicked:false");
    let target = find(&session, &observed, "button", "Save");
    fixture
        .client()
        .eval("document.getElementById('save').setAttribute('aria-label','Replaced')")
        .unwrap();
    let stale = act(&session, target, Action::Click {});
    assert_eq!(stale.effect, "not_applied");
    assert_eq!(stale.error.unwrap()["code"], "CONTROL.STALE_TARGET");
    let observed = observe(&session, frame);
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
}
