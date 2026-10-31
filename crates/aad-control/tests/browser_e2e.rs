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
<a href="/download" aria-label="Get report">report</a>
<button aria-label="Popup" onclick="window.open('/second','_blank','popup')">Popup</button>
<button aria-label="Ask" onclick="document.getElementById('status').value='answer:'+prompt('Your name?','guest')">Ask</button>
<button aria-label="Sure" onclick="document.getElementById('status').value='confirm:'+confirm('Sure?')">Sure</button>
<input type="file" aria-label="Attachment" onchange="document.getElementById('status').value='files:'+Array.from(this.files).map(f=>f.name+':'+f.size).join(',')">
<input type="file" aria-label="Hidden upload" multiple style="display:none" onchange="document.getElementById('status').value='hidden:'+Array.from(this.files).map(f=>f.name).join(',')">
<input id="typed" aria-label="Typed" onkeydown="document.getElementById('keylog').value+='d:'+event.key+':'+event.isTrusted+' '" onkeypress="document.getElementById('keylog').value+='p:'+event.key+' '"><output id="keylog" aria-label="Key log"></output>
<ul id="sortable" aria-label="Sortable"><li aria-label="One" style="cursor:grab">One</li><li aria-label="Two" style="cursor:grab">Two</li><li aria-label="Three" style="cursor:grab">Three</li></ul>
<div style="display:flex;gap:20px"><div draggable="true" aria-label="Card" style="width:80px;height:30px" ondragstart="event.dataTransfer.setData('text/plain','card')">Card</div>
<div aria-label="Bin" style="width:120px;height:40px;border:1px solid" ondragover="event.preventDefault()" ondrop="event.preventDefault();document.getElementById('status').value='dropped:'+event.dataTransfer.getData('text/plain')+':'+event.isTrusted">Bin</div></div>
<script>
let held=null,moves=0;const list=document.getElementById('sortable');
list.addEventListener('mousedown',e=>{held=e.target.closest('li');moves=0;e.preventDefault();});
document.addEventListener('mousemove',()=>{if(held)moves++;});
document.addEventListener('mouseup',e=>{if(!held)return;const over=document.elementFromPoint(e.clientX,e.clientY);const li=over&&over.closest('#sortable li');
if(li&&li!==held)li.after(held);document.getElementById('status').value='sorted:'+Array.from(list.children).map(l=>l.textContent).join(',')+':'+(moves>4)+':'+e.isTrusted;held=null;});
</script>
<x-search></x-search><x-card><button slot="action" aria-label="Slotted" onclick="document.getElementById('status').value='slotted'">Slotted</button></x-card><x-closed></x-closed>
<script>
customElements.define('x-search',class extends HTMLElement{constructor(){super();const r=this.attachShadow({mode:'open'});
r.innerHTML='<label>Query <input id=q></label><button id=go>Go</button>';
r.getElementById('go').addEventListener('click',e=>{document.getElementById('status').value='shadow:'+r.getElementById('q').value+':'+e.isTrusted;});}});
customElements.define('x-card',class extends HTMLElement{constructor(){super();this.attachShadow({mode:'open'}).innerHTML='<div><b>Card title</b><slot name=action></slot></div>';}});
customElements.define('x-closed',class extends HTMLElement{constructor(){super();this.attachShadow({mode:'closed'}).innerHTML='<button>Hidden inside</button>';}});
</script>
<nav aria-label="Primary"><a href="?home" aria-label="Home link">Home</a> <a href="?docs">Docs</a></nav>
<main><section aria-label="Results"><table aria-label="Prices"><tr><th>Item</th><th>Cost</th></tr><tr><td>Tea</td><td>3</td></tr><tr><td>Cake</td><td>5</td></tr></table>
<button onclick="document.getElementById('status').value='buy:results'">Buy</button></section>
<section aria-label="Other"><button onclick="document.getElementById('status').value='buy:other'">Buy</button></section></main>
<button aria-labelledby="buyer">x</button><span id="buyer">Labelled elsewhere</span><input type="search" aria-label="Find">
<iframe src="/frame"></iframe><iframe src="XFRAME_URL"></iframe>
</body></html>"#;
/// A host name the fixture browser maps to loopback: a different site from 127.0.0.1.
const CROSS_SITE: &str = "cross-site.test";

/// Answer one fixture request, or nothing. Browsers open sockets before they
/// have a request to send (preconnects). Answering such a socket with the
/// default page before its request arrives makes the browser read that page as
/// the reply to whatever it asks next -- a new tab at /second showing the
/// fixture page -- so only a complete request line is ever answered.
fn respond(mut stream: std::net::TcpStream, page: &str) {
    // Accepted sockets may inherit the listener's non-blocking mode.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && Instant::now() < deadline => {}
            Err(_) => return,
        }
        if bytes.len() > 65536 {
            return;
        }
    }
    let request = String::from_utf8_lossy(&bytes);
    let mut headers = "Content-Type: text/html; charset=utf-8";
    let body = if request.starts_with("GET /download") {
        headers = "Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"report.txt\"";
        "downloaded body"
    } else if request.starts_with("GET /frame") {
        "<!doctype html><input aria-label='FrameInput' onkeydown=\"document.getElementById('fs').value='key:'+event.key+':'+event.isTrusted\"><button aria-label='FrameButton' onmousedown=\"document.getElementById('fs').value='down:'+event.isTrusted\">Press</button><output id='fs' aria-label='FrameStatus'>ready</output><div style='height:900px'></div>"
    } else if request.starts_with("GET /xframe") {
        "<!doctype html><body style='margin:0'><input aria-label='CrossInput' onkeydown=\"document.getElementById('cs').value='key:'+event.key+':'+event.isTrusted\"><button aria-label='CrossButton' onmousedown=\"document.getElementById('cs').value='down:'+event.button+':'+event.isTrusted\">Press</button><output id='cs' aria-label='CrossStatus'>ready</output><div style='height:900px'></div></body>"
    } else if request.starts_with("GET /api") {
        headers = "Content-Type: application/json";
        r#"{"ok":true,"password":"hunter2"}"#
    } else if request.starts_with("GET /second") {
        "<!doctype html><title>Second</title><body><p aria-label='Second marker'>second</p></body>"
    } else {
        page
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\n{headers}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[test]
fn the_fixture_server_answers_only_requests_it_has_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let accept = || loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    // A preconnected socket whose request comes late gets that request's page.
    let mut client = std::net::TcpStream::connect(address).unwrap();
    let server = accept();
    let worker = std::thread::spawn(move || respond(server, PAGE));
    std::thread::sleep(Duration::from_millis(1500));
    client
        .write_all(b"GET /second HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let mut answer = String::new();
    client.read_to_string(&mut answer).unwrap();
    worker.join().unwrap();
    assert!(answer.contains("Second marker"), "{answer}");
    // One that never sends a request is closed without an answer.
    let mut idle = std::net::TcpStream::connect(address).unwrap();
    let server = accept();
    let worker = std::thread::spawn(move || respond(server, PAGE));
    idle.shutdown(std::net::Shutdown::Write).unwrap();
    let mut answer = String::new();
    idle.read_to_string(&mut answer).unwrap();
    worker.join().unwrap();
    assert_eq!(answer, "");
}

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
        // Another site on the same server, so its frame runs out of process.
        let page = PAGE.replace(
            "XFRAME_URL",
            &format!(
                "http://{CROSS_SITE}:{}/xframe",
                listener.local_addr().unwrap().port()
            ),
        );
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let server = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // One thread per connection: a socket the browser opens
                        // ahead of time must not hold up the next request.
                        let page = page.clone();
                        std::thread::spawn(move || respond(stream, &page));
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
                "--site-per-process",
            ])
            .arg(format!("--host-resolver-rules=MAP {CROSS_SITE} 127.0.0.1"))
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

fn value<'a>(observation: &'a Value, name: &str) -> &'a str {
    node(observation, name)["value"]
        .as_str()
        .unwrap_or_default()
}
/// Observe the main frame and act on the node with this role and name.
fn act_on(session: &Session, main: &str, role: &str, name: &str, action: Action) -> Response {
    let observed = observe(session, main);
    let target = find(session, &observed, role, name);
    act(session, target, action)
}
fn keys(keys: &[&str]) -> Action {
    Action::Key {
        keys: keys.iter().map(|k| k.to_string()).collect(),
    }
}
fn type_text(text: &str, mode: Option<TypeMode>) -> Action {
    Action::TypeText {
        text: text.into(),
        mode,
    }
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn typing_sends_trusted_keystrokes_and_shortcuts_stay_on_the_focused_field() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Typed", WaitCondition::Appears)
        .error
        .is_none());
    let observed = observe(&session, &main);
    assert!(actions(&observed, "Typed").contains(&"type_text".to_string()));
    assert!(!actions(&observed, "Save").contains(&"type_text".to_string()));
    // Keys mode: real key events with key, code and the text they type.
    let typed = act_on(
        &session,
        &main,
        "textbox",
        "Typed",
        type_text("Ab1!", Some(TypeMode::Keys)),
    );
    assert!(typed.error.is_none(), "{typed:?}");
    assert_eq!(typed.data.unwrap()["backend_result"]["keystrokes"], 4);
    let observed = observe(&session, &main);
    assert_eq!(value(&observed, "Typed"), "Ab1!");
    assert_eq!(
        value(&observed, "Key log"),
        "d:A:true p:A d:b:true p:b d:1:true p:1 d:!:true p:! "
    );
    // A character a US layout has no key for is refused before anything is sent.
    let refused = act_on(
        &session,
        &main,
        "textbox",
        "Typed",
        type_text("é", Some(TypeMode::Keys)),
    );
    assert_eq!(refused.commit_state, CommitState::NotCommitted);
    assert_eq!(refused.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
    // Ctrl+A selects the field's text, so typing replaces it; unicode mode
    // inserts any character.
    let selected = act_on(&session, &main, "textbox", "Typed", keys(&["ctrl", "a"]));
    assert!(selected.error.is_none(), "{selected:?}");
    let typed = act_on(&session, &main, "textbox", "Typed", type_text("Zé日", None));
    assert!(typed.error.is_none(), "{typed:?}");
    assert_eq!(value(&observe(&session, &main), "Typed"), "Zé日");
    // Shift+Arrow extends the selection; Backspace removes it.
    for chord in [
        &["shift", "ArrowLeft"][..],
        &["shift", "ArrowLeft"],
        &["Backspace"],
    ] {
        let pressed = act_on(&session, &main, "textbox", "Typed", keys(chord));
        assert!(pressed.error.is_none(), "{chord:?}: {pressed:?}");
    }
    assert_eq!(value(&observe(&session, &main), "Typed"), "Z");
    // Undo brings the deleted text back.
    let undone = act_on(&session, &main, "textbox", "Typed", keys(&["ctrl", "z"]));
    assert!(undone.error.is_none(), "{undone:?}");
    assert_eq!(value(&observe(&session, &main), "Typed"), "Zé日");
    // Shortcuts the browser itself would act on never reach the page.
    for chord in [
        &["ctrl", "t"][..],
        &["ctrl", "w"],
        &["F12"],
        &["alt", "F4"],
        &["ctrl", "shift", "n"],
    ] {
        let refused = act_on(&session, &main, "textbox", "Typed", keys(chord));
        assert_eq!(refused.commit_state, CommitState::NotCommitted, "{chord:?}");
        assert_eq!(
            refused.error.unwrap()["code"],
            "CONTROL.ACTION_UNSUPPORTED",
            "{chord:?}"
        );
    }
    assert_eq!(
        targets(&session)
            .iter()
            .filter(|r| r["metadata"]["main_frame"] == true)
            .count(),
        1,
        "no tab was opened"
    );
}

fn drag_to(session: &Session, main: &str, from: &str, (role, to): (&str, &str)) -> Response {
    let observed = observe(session, main);
    let source = find(session, &observed, "listitem", from);
    let to = find(session, &observed, role, to);
    act(
        session,
        source,
        Action::Drag {
            to,
            button: None,
            fraction: None,
            to_fraction: Some(Fraction { fx: 0.5, fy: 0.8 }),
        },
    )
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn drags_move_pointer_sortables_and_html5_draggables() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Bin", WaitCondition::Appears)
        .error
        .is_none());
    let observed = observe(&session, &main);
    for name in ["One", "Card"] {
        assert!(
            actions(&observed, name).contains(&"drag".to_string()),
            "{name}"
        );
    }
    // A pointer-driven sortable: press, glide, release, all trusted.
    let dragged = drag_to(&session, &main, "One", ("listitem", "Three"));
    assert!(dragged.error.is_none(), "{dragged:?}");
    assert_eq!(dragged.data.unwrap()["backend_result"]["html5_drag"], false);
    assert_eq!(
        status(&observe(&session, &main)),
        "sorted:Two,Three,One:true:true"
    );
    // An HTML5 draggable reaches its drop target with its data.
    let observed = observe(&session, &main);
    let card: Target = serde_json::from_value(node(&observed, "Card")["target"].clone()).unwrap();
    let bin: Target = serde_json::from_value(node(&observed, "Bin")["target"].clone()).unwrap();
    let dropped = act(
        &session,
        card,
        Action::Drag {
            to: bin,
            button: None,
            fraction: None,
            to_fraction: None,
        },
    );
    assert!(dropped.error.is_none(), "{dropped:?}");
    assert_eq!(dropped.data.unwrap()["backend_result"]["html5_drag"], true);
    assert_eq!(status(&observe(&session, &main)), "dropped:card:true");
    // A destination something else covers is refused before anything is pressed.
    let covered = drag_to(&session, &main, "Two", ("button", "Covered"));
    assert_eq!(
        covered.commit_state,
        CommitState::NotCommitted,
        "{covered:?}"
    );
    assert_eq!(covered.error.unwrap()["code"], "CONTROL.OCCLUDED");
    assert_eq!(status(&observe(&session, &main)), "dropped:card:true");
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn controls_inside_open_shadow_roots_are_observed_and_operated() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    let loaded = wait_until(&session, &main, "Go", WaitCondition::Appears);
    assert!(loaded.error.is_none(), "{loaded:?}");
    let observed = observe(&session, &main);
    let query = |observed: &Value| {
        observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["role"] == "textbox" && n["name"] == "Query")
            .cloned()
            .expect("the shadow field is observed")
    };
    assert!(query(&observed)["actions"]
        .as_array()
        .unwrap()
        .contains(&json!("fill")));
    // Slotted content shows once, where it is slotted; a closed root stays shut.
    let slotted = observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["name"] == "Slotted" && n["role"] == "button")
        .count();
    assert_eq!(slotted, 1);
    assert!(observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|n| n["name"] != "Hidden inside"));
    let filled = act_on(
        &session,
        &main,
        "textbox",
        "Query",
        Action::Fill { value: "ab".into() },
    );
    assert!(filled.error.is_none(), "{filled:?}");
    // Focus inside the shadow root is the field's own: typing reaches it.
    let typed = act_on(
        &session,
        &main,
        "textbox",
        "Query",
        type_text("c", Some(TypeMode::Keys)),
    );
    assert!(typed.error.is_none(), "{typed:?}");
    let observed = observe(&session, &main);
    assert_eq!(query(&observed)["value"], "abc");
    assert_eq!(query(&observed)["states"]["focused"], true);
    // A trusted click is hit-tested through the shadow host to the button.
    let clicked = act_on(
        &session,
        &main,
        "button",
        "Go",
        Action::Click {
            button: None,
            count: None,
            fraction: Some(Fraction { fx: 0.5, fy: 0.5 }),
        },
    );
    assert!(clicked.error.is_none(), "{clicked:?}");
    assert_eq!(status(&observe(&session, &main)), "shadow:abc:true");
    let clicked = act_on(&session, &main, "button", "Slotted", Action::click());
    assert!(clicked.error.is_none(), "{clicked:?}");
    assert_eq!(status(&observe(&session, &main)), "slotted");
    // A shadow control that changes after it was observed is stale.
    let observed = observe(&session, &main);
    let go = find(&session, &observed, "button", "Go");
    fixture
        .client()
        .eval(
            "document.querySelector('x-search').shadowRoot.getElementById('go').textContent='Went'",
        )
        .unwrap();
    let stale = act(&session, go, Action::click());
    assert_eq!(stale.error.unwrap()["code"], "CONTROL.STALE_TARGET");
}

/// The resource of the frame whose metadata satisfies `wanted`, once listed.
fn frame_of(session: &Session, wanted: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = targets(session)
            .into_iter()
            .find(|r| r["metadata"]["main_frame"] == false && wanted(&r["metadata"]))
        {
            return found;
        }
        assert!(Instant::now() < deadline, "the frame was not listed");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn trusted_click() -> Action {
    Action::Click {
        button: None,
        count: None,
        fraction: Some(Fraction { fx: 0.5, fy: 0.5 }),
    }
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn frames_in_and_out_of_process_take_trusted_pointer_and_keyboard_input() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    let main_frame = targets(&session)
        .into_iter()
        .find(|r| r["resource_id"] == main.as_str())
        .unwrap()["metadata"]["frame_id"]
        .clone();
    // The cross-site frame runs in its own renderer and is listed with its parent.
    let cross = frame_of(&session, |m| m["out_of_process"] == true);
    assert_eq!(cross["metadata"]["parent_frame_id"], main_frame);
    assert!(cross["metadata"]["url"]
        .as_str()
        .unwrap()
        .contains(CROSS_SITE));
    let cross = cross["resource_id"].as_str().unwrap().to_string();
    let local = frame_of(&session, |m| {
        m["url"].as_str().is_some_and(|u| u.ends_with("/frame"))
    });
    let local = local["resource_id"].as_str().unwrap().to_string();
    let loaded = wait_until(&session, &cross, "CrossButton", WaitCondition::Appears);
    assert!(loaded.error.is_none(), "{loaded:?}");

    // Out of process: pointer, keyboard and typing reach the frame's elements.
    let clicked = act_on(&session, &cross, "button", "CrossButton", trusted_click());
    assert!(clicked.error.is_none(), "{clicked:?}");
    assert_eq!(
        value(&observe(&session, &cross), "CrossStatus"),
        "down:0:true"
    );
    let right = act_on(
        &session,
        &cross,
        "button",
        "CrossButton",
        Action::Click {
            button: Some(Button::Right),
            count: None,
            fraction: None,
        },
    );
    assert!(right.error.is_none(), "{right:?}");
    assert_eq!(
        value(&observe(&session, &cross), "CrossStatus"),
        "down:2:true"
    );
    let typed = act_on(
        &session,
        &cross,
        "textbox",
        "CrossInput",
        type_text("hi", Some(TypeMode::Keys)),
    );
    assert!(typed.error.is_none(), "{typed:?}");
    let observed = observe(&session, &cross);
    assert_eq!(value(&observed, "CrossInput"), "hi");
    assert_eq!(value(&observed, "CrossStatus"), "key:i:true");
    let pressed = act_on(&session, &cross, "textbox", "CrossInput", keys(&["Enter"]));
    assert!(pressed.error.is_none(), "{pressed:?}");
    assert_eq!(
        value(&observe(&session, &cross), "CrossStatus"),
        "key:Enter:true"
    );

    // In process: the frame's offset in the page is applied.
    let clicked = act_on(&session, &local, "button", "FrameButton", trusted_click());
    assert!(clicked.error.is_none(), "{clicked:?}");
    assert_eq!(
        value(&observe(&session, &local), "FrameStatus"),
        "down:true"
    );
    let typed = act_on(
        &session,
        &local,
        "textbox",
        "FrameInput",
        type_text("ok", None),
    );
    assert!(typed.error.is_none(), "{typed:?}");
    assert_eq!(value(&observe(&session, &local), "FrameInput"), "ok");

    // Something the page lays over a frame is hit, not the frame's element.
    fixture
        .client()
        .eval(
            "const veil=document.createElement('div'); veil.style='position:fixed;inset:0;z-index:9'; \
             document.body.appendChild(veil); true",
        )
        .unwrap();
    for frame in [&cross, &local] {
        let name = if frame == &cross {
            "CrossButton"
        } else {
            "FrameButton"
        };
        let covered = act_on(&session, frame, "button", name, trusted_click());
        assert_eq!(
            covered.commit_state,
            CommitState::NotCommitted,
            "{covered:?}"
        );
        assert_eq!(covered.error.unwrap()["code"], "CONTROL.OCCLUDED");
    }
}

fn observe_view(session: &Session, resource: &str, view: View) -> Response {
    call(
        session,
        Operation::Observe {
            resource_id: resource.into(),
            max_nodes: 2000,
            view,
        },
    )
}
fn locate(session: &Session, observation: &Value, locator: Value) -> Response {
    call(
        session,
        Operation::Find {
            observation_id: observation["observation_id"].as_str().unwrap().into(),
            locator: Box::new(serde_json::from_value(locator).unwrap()),
            expect: Default::default(),
        },
    )
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn pages_read_as_regions_text_and_accessible_names_and_locators_narrow() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    let loaded = wait_until(&session, &main, "Find", WaitCondition::Appears);
    assert!(loaded.error.is_none(), "{loaded:?}");
    // Roles and names from the browser's accessibility tree.
    let observed = observe(&session, &main);
    let accessibility = &observed["metadata"]["accessibility"];
    assert_eq!(accessibility["applied"], true, "{accessibility}");
    eprintln!(
        "accessibility: {} nodes in {} ms",
        accessibility["nodes"], accessibility["ms"]
    );
    assert_eq!(node(&observed, "Find")["role"], "searchbox");
    assert_eq!(node(&observed, "Labelled elsewhere")["role"], "button");
    assert_eq!(node(&observed, "Status")["role"], "status");
    assert!(observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["role"] == "document"));

    // An overview names the landmarks; an outline drills into one.
    let overview = observe_view(
        &session,
        &main,
        View {
            mode: ViewMode::Overview,
            ..Default::default()
        },
    );
    assert!(overview.error.is_none(), "{overview:?}");
    let regions: Vec<String> = overview.data.unwrap()["regions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["region"].as_str().unwrap().to_string())
        .collect();
    for region in ["Primary", "Results", "Other", "(loose in the page)"] {
        assert!(
            regions.contains(&region.to_string()),
            "{region}: {regions:?}"
        );
    }
    let outline = observe_view(
        &session,
        &main,
        View {
            mode: ViewMode::Outline,
            region: Some("Results".into()),
            ..Default::default()
        },
    )
    .data
    .unwrap();
    assert_eq!(outline["region"], "Results");
    let names: Vec<&str> = outline["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"Buy") && names.contains(&"Tea"),
        "{names:?}"
    );
    assert!(!names.contains(&"Home link"), "{names:?}");
    let missing = observe_view(
        &session,
        &main,
        View {
            mode: ViewMode::Outline,
            region: Some("Nowhere".into()),
            ..Default::default()
        },
    );
    let failure = missing.error.unwrap();
    assert_eq!(failure["code"], "CONTROL.NOT_FOUND");
    assert!(failure["details"]["regions"].as_array().unwrap().len() >= 3);

    // Text: the region's words and its table as rows; the page bounded.
    let text = observe_view(
        &session,
        &main,
        View {
            mode: ViewMode::Text,
            region: Some("Results".into()),
            ..Default::default()
        },
    );
    assert!(text.error.is_none(), "{text:?}");
    let text = text.data.unwrap();
    assert!(text["text"].as_str().unwrap().contains("Buy"), "{text}");
    assert!(!text["text"].as_str().unwrap().contains("Home"), "{text}");
    assert_eq!(text["tables"][0]["name"], "Prices");
    assert_eq!(
        text["tables"][0]["rows"],
        json!([["Item", "Cost"], ["Tea", "3"], ["Cake", "5"]])
    );
    fixture
        .client()
        .eval("const notes=document.createElement('p'); notes.textContent='word '.repeat(300); document.body.appendChild(notes); true")
        .unwrap();
    let page = observe_view(
        &session,
        &main,
        View {
            mode: ViewMode::Text,
            max_characters: Some(500),
            ..Default::default()
        },
    )
    .data
    .unwrap();
    assert_eq!(page["text_truncated"], true, "{page}");
    assert_eq!(page["text"].as_str().unwrap().chars().count(), 500);
    assert!(page["characters"].as_u64().unwrap() > 500);

    // Locators: within a region, the nth on screen, a contained name.
    let observed = observe(&session, &main);
    let ambiguous = locate(
        &session,
        &observed,
        json!({"role": "button", "name": "Buy"}),
    );
    assert_eq!(ambiguous.error.unwrap()["code"], "CONTROL.AMBIGUOUS");
    let other = locate(
        &session,
        &observed,
        json!({"role": "button", "name": "Buy", "within": {"name": "Other"}}),
    );
    assert!(other.error.is_none(), "{other:?}");
    let first = locate(
        &session,
        &observed,
        json!({"role": "button", "name": "Buy", "nth": 1}),
    );
    let first: Target = serde_json::from_value(first.data.unwrap()["target"].clone()).unwrap();
    let contained = locate(
        &session,
        &observed,
        json!({"name": "home", "match": "contains"}),
    );
    assert_eq!(contained.data.unwrap()["node"]["name"], "Home link");
    let refused = locate(&session, &observed, json!({"automation_id": "x"}));
    assert_eq!(
        refused.error.unwrap()["code"],
        "CONTROL.LOCATOR_UNSUPPORTED"
    );
    assert!(act(&session, first, Action::click()).error.is_none());
    assert_eq!(status(&observe(&session, &main)), "buy:results");
    let observed = observe(&session, &main);
    let last = locate(
        &session,
        &observed,
        json!({"role": "button", "name": "Buy", "nth": "last"}),
    );
    let last: Target = serde_json::from_value(last.data.unwrap()["target"].clone()).unwrap();
    assert!(act(&session, last, Action::click()).error.is_none());
    assert_eq!(status(&observe(&session, &main)), "buy:other");

    // Cost of the accessibility pass on a crowded page.
    fixture
        .client()
        .eval(
            "const many=document.createElement('div'); for (let i=0;i<500;i++) { \
             const b=document.createElement('button'); b.textContent='Item '+i; many.appendChild(b); } \
             document.body.appendChild(many); true",
        )
        .unwrap();
    let started = Instant::now();
    let crowded = observe_view(&session, &main, View::default()).data.unwrap();
    eprintln!(
        "crowded page: {} nodes observed in {} ms, accessibility {}",
        crowded["node_count"],
        started.elapsed().as_millis(),
        crowded["metadata"]["accessibility"]
    );
}

fn page_value(fixture: &Fixture, expression: &str) -> Value {
    fixture.client().eval(expression).unwrap()["result"]["value"].clone()
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn pages_emulate_devices_list_storage_and_requests_and_clear_site_data() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    let loaded = wait_until(&session, &main, "Name", WaitCondition::Appears);
    assert!(loaded.error.is_none(), "{loaded:?}");
    let observed = observe(&session, &main);
    for action in ["emulate", "clear_site_data"] {
        assert!(observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["role"] == "document"
                && n["capabilities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a == action)));
    }
    // A phone in dark mode, then back.
    let body = document(&session, &observed);
    let refused = act(
        &session,
        body,
        Action::Emulate {
            preset: None,
            color_scheme: None,
        },
    );
    assert_eq!(refused.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
    let body = document(&session, &observe(&session, &main));
    let phone = act(
        &session,
        body,
        Action::Emulate {
            preset: Some(DevicePreset::Mobile),
            color_scheme: Some(ColorScheme::Dark),
        },
    );
    assert!(phone.error.is_none(), "{phone:?}");
    // A page without a viewport tag lays out at 980 CSS pixels, as on a phone.
    assert_eq!(page_value(&fixture, "screen.width"), 390);
    assert_eq!(
        page_value(
            &fixture,
            "matchMedia('(prefers-color-scheme: dark)').matches"
        ),
        true
    );
    assert_eq!(page_value(&fixture, "navigator.maxTouchPoints"), 5);
    let body = document(&session, &observe(&session, &main));
    let reset = act(
        &session,
        body,
        Action::Emulate {
            preset: Some(DevicePreset::Reset),
            color_scheme: None,
        },
    );
    assert!(reset.error.is_none(), "{reset:?}");
    assert_ne!(page_value(&fixture, "screen.width"), 390);

    // Storage: cookie names and flags, values only under the operator's grant.
    page_value(
        &fixture,
        "document.cookie='theme=dark; path=/'; localStorage.setItem('cart','3 items'); true",
    );
    let stored = session
        .browser_storage(&main, Duration::from_secs(5))
        .unwrap();
    let theme = stored["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "theme")
        .cloned()
        .unwrap_or_else(|| panic!("{stored}"));
    assert!(theme.get("value").is_none(), "{theme}");
    assert_eq!(theme["path"], "/");
    let cart = &stored["local_storage"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["key"] == "cart")
        .cloned()
        .unwrap();
    assert_eq!(cart["value_length"], 7);
    assert!(cart.get("value").is_none());
    let trusted = Session::new(
        vec![Arc::new(BrowserBackend::new(fixture.port))],
        Policy {
            allow_cookie_values: true,
            ..Default::default()
        },
    );
    let trusted_main = main_of(&trusted, &fixture.page_id);
    let shown = trusted
        .browser_storage(&trusted_main, Duration::from_secs(5))
        .unwrap();
    assert!(shown["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["name"] == "theme" && c["value"] == "dark"));
    drop(trusted);

    // Requests the page made since this session attached, bodies on request.
    page_value(
        &fixture,
        "fetch('/api?token=secret&page=2').then(r => r.text()).then(() => true)",
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let api = loop {
        let listed = session
            .browser_network(&main, 50, true, Duration::from_secs(5))
            .unwrap();
        if let Some(found) = listed["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["url"].as_str().unwrap().contains("/api") && r["state"] == "finished")
        {
            break found.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no finished api request: {listed}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(api["method"], "GET");
    assert_eq!(api["status"], 200);
    assert_eq!(api["mime_type"], "application/json");
    let url = api["url"].as_str().unwrap();
    assert!(!url.contains("secret") && url.contains("page=2"), "{url}");
    let body = api["body"]["text"].as_str().unwrap();
    assert!(
        body.contains("\"ok\":true") && !body.contains("hunter2"),
        "{body}"
    );
    assert_eq!(
        session
            .browser_network(&main, 0, false, Duration::from_secs(5))
            .unwrap_err()
            .code,
        "CONTROL.INVALID_REQUEST"
    );

    // Clearing site data takes the cookies and storage with it.
    let body = document(&session, &observe(&session, &main));
    let cleared = act(&session, body, Action::ClearSiteData {});
    assert!(cleared.error.is_none(), "{cleared:?}");
    let after = session
        .browser_storage(&main, Duration::from_secs(5))
        .unwrap();
    assert_eq!(after["cookies"], json!([]), "{after}");
    assert_eq!(after["local_storage"]["count"], 0, "{after}");
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
fn uploads_reach_file_inputs_only_from_allowed_directories() {
    let _serial = serial();
    let fixture = Fixture::new();
    let allowed = std::env::temp_dir().join(format!("aad-upload-{}", uuid::Uuid::new_v4()));
    let outside = std::env::temp_dir().join(format!("aad-upload-out-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(allowed.join("report.txt"), "hello").unwrap();
    std::fs::write(allowed.join("notes.txt"), "n").unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    let path = |dir: &PathBuf, name: &str| dir.join(name).display().to_string();
    let session = Session::new(
        vec![Arc::new(BrowserBackend::new(fixture.port))],
        Policy {
            allow_input: true,
            upload_roots: vec![allowed.clone()],
            ..Default::default()
        },
    );
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Name", WaitCondition::Appears)
        .error
        .is_none());
    let observed = observe(&session, &main);
    // A file input takes files and nothing else.
    assert_eq!(actions(&observed, "Attachment"), vec!["upload".to_string()]);
    assert_eq!(node(&observed, "Hidden upload")["states"]["hidden"], true);
    let input = find(&session, &observed, "button", "Attachment");
    let fill = act(
        &session,
        input,
        Action::Fill {
            value: "C:\\fakepath".into(),
        },
    );
    assert_eq!(fill.error.unwrap()["code"], "CONTROL.ACTION_UNSUPPORTED");
    for (files, code) in [
        (vec![path(&outside, "secret.txt")], "POLICY.DENIED"),
        (vec!["report.txt".to_string()], "CONTROL.INVALID_REQUEST"),
        (
            vec![path(&allowed, "missing.txt")],
            "CONTROL.FILE_NOT_FOUND",
        ),
        (
            vec![path(&allowed, "report.txt"), path(&allowed, "notes.txt")],
            "CONTROL.INVALID_REQUEST",
        ),
    ] {
        let observed = observe(&session, &main);
        let input = find(&session, &observed, "button", "Attachment");
        let refused = act(&session, input, Action::Upload { files });
        assert_eq!(refused.commit_state, CommitState::NotCommitted);
        assert_eq!(refused.error.unwrap()["code"], code);
    }
    assert_eq!(status(&observe(&session, &main)), "ready");
    let observed = observe(&session, &main);
    let input = find(&session, &observed, "button", "Attachment");
    let uploaded = act(
        &session,
        input,
        Action::Upload {
            files: vec![path(&allowed, "report.txt")],
        },
    );
    assert!(uploaded.error.is_none(), "{uploaded:?}");
    let observed = observe(&session, &main);
    assert_eq!(status(&observed), "files:report.txt:5");
    assert_eq!(
        node(&observed, "Attachment")["files"],
        json!(["report.txt"])
    );
    // A hidden input behind a styled button still takes several files.
    let input = find(&session, &observed, "button", "Hidden upload");
    let uploaded = act(
        &session,
        input,
        Action::Upload {
            files: vec![path(&allowed, "report.txt"), path(&allowed, "notes.txt")],
        },
    );
    assert!(uploaded.error.is_none(), "{uploaded:?}");
    assert_eq!(
        status(&observe(&session, &main)),
        "hidden:report.txt,notes.txt"
    );
    drop(session);
    // Without a grant every upload is refused.
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    let observed = observe(&session, &main);
    let input = find(&session, &observed, "button", "Attachment");
    let refused = act(
        &session,
        input,
        Action::Upload {
            files: vec![path(&allowed, "report.txt")],
        },
    );
    assert_eq!(refused.error.unwrap()["code"], "POLICY.DENIED");
    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn downloads_land_in_the_session_directory_and_are_listed() {
    let _serial = serial();
    let fixture = Fixture::new();
    // A browser attached by port is somebody's own: its downloads stay put.
    {
        let (session, backend) = session_for(&fixture);
        targets(&session);
        let attached = backend.downloads(Duration::from_secs(5)).unwrap();
        assert_eq!(attached["tracking"], false, "{attached}");
        assert_eq!(
            attached["reason"],
            "attached browser: downloads are not redirected"
        );
    }
    // The fixture launched this one, so the session routes its downloads.
    // Chromium resets the download setting when any browser-level DevTools
    // client goes away, here the one above; the session sets it again.
    let backend = Arc::new(BrowserBackend::launched(fixture.port));
    let session = Session::new(
        vec![backend.clone()],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    );
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Name", WaitCondition::Appears)
        .error
        .is_none());
    let before = backend.downloads(Duration::from_secs(5)).unwrap();
    assert_eq!(before["tracking"], true, "{before}");
    assert_eq!(before["downloads"], json!([]));
    let observed = observe(&session, &main);
    let link = find(&session, &observed, "link", "Get report");
    assert!(act(&session, link, Action::click()).error.is_none());
    let deadline = Instant::now() + Duration::from_secs(10);
    let item = loop {
        let listed = backend.downloads(Duration::from_secs(5)).unwrap();
        if let Some(done) = listed["downloads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["state"] == "completed")
        {
            break done.clone();
        }
        assert!(Instant::now() < deadline, "no completed download: {listed}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(item["suggested_filename"], "report.txt");
    assert!(item["url"].as_str().unwrap().ends_with("/download"));
    assert_eq!(item["received_bytes"], 15);
    let saved = PathBuf::from(item["path"].as_str().unwrap());
    assert!(saved.starts_with(before["directory"].as_str().unwrap()));
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "downloaded body");
    // The page itself stayed where it was.
    assert_eq!(status(&observe(&session, &main)), "ready");
    // Releasing hands the browser its own setting back; nothing is routed after.
    backend.release_downloads();
    let released = backend.downloads(Duration::from_secs(5)).unwrap();
    assert_eq!(released["tracking"], false, "{released}");
    assert_eq!(released["downloads"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(before["directory"].as_str().unwrap());
}

fn only_dialog(observed: &Value) -> &Value {
    let nodes = observed["nodes"].as_array().unwrap();
    assert_eq!(
        nodes.len(),
        1,
        "a blocked page shows only its dialog: {observed}"
    );
    assert_eq!(nodes[0]["role"], "alertdialog");
    &nodes[0]
}

#[test]
#[ignore = "requires isolated Chromium with its sandbox enabled; CI runs it explicitly"]
fn dialogs_block_the_page_until_accepted_or_dismissed() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (session, _) = session_for(&fixture);
    let main = main_of(&session, &fixture.page_id);
    assert!(wait_until(&session, &main, "Name", WaitCondition::Appears)
        .error
        .is_none());

    // A click that opens a prompt is delivered; the page then shows the prompt.
    let observed = observe(&session, &main);
    let ask = find(&session, &observed, "button", "Ask");
    let started = Instant::now();
    let clicked = act(&session, ask, Action::click());
    assert!(clicked.error.is_none(), "{clicked:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        clicked.data.unwrap()["backend_result"]["dialog_opened"],
        true
    );
    let listed = targets(&session);
    let page = listed
        .iter()
        .find(|r| r["metadata"]["page_id"] == fixture.page_id.as_str())
        .unwrap();
    assert_eq!(page["metadata"]["dialog"]["type"], "prompt", "{page}");
    assert_eq!(page["resource_id"], main.as_str());
    let blocked = observe(&session, &main);
    let dialog = only_dialog(&blocked);
    assert_eq!(dialog["name"], "Your name?");
    assert_eq!(dialog["value"], "guest");
    assert_eq!(dialog["dialog"]["type"], "prompt");
    assert_eq!(dialog["capabilities"], json!(["accept", "dismiss"]));
    let target = find(&session, &blocked, "alertdialog", "Your name?");
    let capture = call(
        &session,
        Operation::Capture {
            target: target.clone(),
        },
    );
    assert_eq!(capture.error.unwrap()["code"], "CONTROL.DIALOG_OPEN");
    let accepted = act(
        &session,
        target,
        Action::Accept {
            text: Some("Ada".into()),
        },
    );
    assert!(accepted.error.is_none(), "{accepted:?}");
    assert_eq!(status(&observe(&session, &main)), "answer:Ada");

    // A confirm: text is refused, dismiss answers false.
    let observed = observe(&session, &main);
    let sure = find(&session, &observed, "button", "Sure");
    assert!(act(&session, sure, Action::click()).error.is_none());
    let blocked = observe(&session, &main);
    assert_eq!(only_dialog(&blocked)["dialog"]["type"], "confirm");
    let target = find(&session, &blocked, "alertdialog", "Sure?");
    let refused = act(
        &session,
        target,
        Action::Accept {
            text: Some("no".into()),
        },
    );
    assert_eq!(refused.error.unwrap()["code"], "CONTROL.INVALID_REQUEST");
    let blocked = observe(&session, &main);
    let target = find(&session, &blocked, "alertdialog", "Sure?");
    assert!(act(&session, target, Action::Dismiss {}).error.is_none());
    assert_eq!(status(&observe(&session, &main)), "confirm:false");

    // A dialog a timer opens: acting on an earlier target is refused at
    // once instead of timing out, and nothing reaches the page.
    fixture
        .client()
        .eval("setTimeout(() => alert('Timer fired'), 300); true")
        .unwrap();
    let observed = observe(&session, &main);
    let name = find(&session, &observed, "textbox", "Name");
    std::thread::sleep(Duration::from_millis(800));
    let started = Instant::now();
    let refused = act(
        &session,
        name,
        Action::Fill {
            value: "must not land".into(),
        },
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(refused.commit_state, CommitState::NotCommitted);
    let failure = refused.error.unwrap();
    assert_eq!(failure["code"], "CONTROL.DIALOG_OPEN");
    assert_eq!(failure["details"]["dialog"]["message"], "Timer fired");
    let blocked = observe(&session, &main);
    assert_eq!(only_dialog(&blocked)["dialog"]["type"], "alert");
    let target = find(&session, &blocked, "alertdialog", "Timer fired");
    assert!(act(&session, target, Action::Accept { text: None })
        .error
        .is_none());
    let observed = observe(&session, &main);
    assert_ne!(node(&observed, "Name")["value"], "must not land");

    // A dialog that opened before a session ever looked at the page cannot be
    // answered through CDP: the browser lets only a connection that saw it
    // open handle it, and the blocked renderer never answers a new one. It is
    // listed as unresponsive, quickly, rather than hanging the listing.
    drop(session);
    fixture
        .client()
        .eval("setTimeout(() => alert('Early'), 200); true")
        .unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let (late, _) = session_for(&fixture);
    let started = Instant::now();
    let listed = targets(&late);
    assert!(started.elapsed() < Duration::from_secs(6));
    let page = listed
        .iter()
        .find(|r| r["metadata"]["page_id"] == fixture.page_id.as_str())
        .unwrap();
    assert_eq!(page["metadata"]["responsive"], false, "{page}");
    assert_eq!(page["capabilities"], json!([]));
    let seen = call_within(
        &late,
        Operation::Observe {
            resource_id: page["resource_id"].as_str().unwrap().into(),
            max_nodes: 500,
            view: Default::default(),
        },
        10_000,
    );
    assert_eq!(seen.error.unwrap()["code"], "BROWSER.PAGE_UNRESPONSIVE");
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
        if data["resources"].as_array().unwrap().iter().any(|r| {
            r["metadata"]["main_frame"] == false
                && r["metadata"]["url"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("/frame"))
        }) {
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
        .find(|r| {
            r["metadata"]["main_frame"] == false
                && r["metadata"]["url"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("/frame"))
        })
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
    // The frame is listed as soon as it exists; its document may still be loading.
    let loaded = wait_until(&session, frame, "FrameInput", WaitCondition::Appears);
    assert!(loaded.error.is_none(), "{loaded:?}");
    let observed = observe(&session, frame);
    for name in [
        "navigate",
        "back",
        "forward",
        "reload",
        "open_tab",
        "close_page",
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
    for name in ["key", "type_text", "fill"] {
        assert!(actions(&observed, "FrameInput").contains(&name.to_string()));
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
