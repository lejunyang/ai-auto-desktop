use aad_control::*;
use aad_mcp::{Request as RpcRequest, Server};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

struct Fake {
    name: &'static str,
    calls: AtomicUsize,
    /// When set, each act waits for a release before it returns.
    gate: Mutex<Option<mpsc::Receiver<()>>>,
}
impl Backend for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into()]
    }
    fn lease_key(&self, _: &Value) -> Option<String> {
        Some(format!("test-control-instance:{self:p}"))
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok(vec![BackendResource {
            resource: Resource {
                resource_id: "fixture:one".into(),
                provider: self.name.into(),
                kind: "desktop".into(),
                name: "test".into(),
                capabilities: self.capabilities(),
                metadata: json!({}),
            },
            selector: json!({}),
        }])
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        Ok(json!({"nodes":[{"node_id":"n","name":"Save","role":"button"}],"truncated":false}))
    }
    fn act(&self, _: &Value, _: &Value, _: &Action, context: &Context) -> Result<Value> {
        context.commit()?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.gate.lock().unwrap().as_ref() {
            let _ = gate.recv();
        }
        std::thread::sleep(Duration::from_millis(80));
        Ok(json!({"submitted":true}))
    }
}
fn rpc(id: u64, name: &str, args: Value) -> RpcRequest {
    RpcRequest {
        id: Some(json!(id)),
        method: "tools/call".into(),
        params: json!({"name":name,"arguments":args}),
    }
}
fn payload(reply: Value) -> Value {
    assert!(reply.get("error").is_none(), "{reply}");
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
fn session(name: &'static str, input: bool) -> (Session, Arc<Fake>) {
    let fake = Arc::new(Fake {
        name,
        calls: AtomicUsize::new(0),
        gate: Mutex::new(None),
    });
    let session = Session::new(
        vec![fake.clone()],
        Policy {
            allow_input: input,
            ..Default::default()
        },
    );
    (session, fake)
}
fn server(name: &'static str, input: bool) -> (Arc<Server>, Arc<Fake>) {
    let (session, fake) = session(name, input);
    (Arc::new(Server::with_control(Arc::new(session))), fake)
}
fn target(server: &Server) -> Value {
    payload(
        server
            .handle(&rpc(1, "control_targets", json!({})))
            .unwrap(),
    );
    let observed = payload(
        server
            .handle(&rpc(
                2,
                "control_observe",
                json!({"resource_id":"fixture:one"}),
            ))
            .unwrap(),
    );
    observed["data"]["nodes"][0]["target"].clone()
}

#[test]
fn all_platform_providers_route_through_the_same_tools_without_a_uia_driver() {
    for provider in [
        "desktop.windows_uia",
        "desktop.linux_atspi",
        "desktop.macos_ax",
        "browser.cdp",
    ] {
        let (server, fake) = server(provider, true);
        let target = target(&server);
        let reply = payload(
            server
                .handle(&rpc(
                    3,
                    "control_act",
                    json!({"target":target,"action":{"type":"click"}}),
                ))
                .unwrap(),
        );
        assert_eq!(reply["data"]["provider"], provider);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let denied = server
            .handle(&rpc(4, "invoke", json!({"target":"legacy:bypass:n"})))
            .unwrap();
        assert_eq!(denied["error"]["code"], -32602);
    }
}
#[test]
fn grants_unknown_fields_and_coordinates_cannot_be_bypassed() {
    let (server, fake) = server("desktop.linux_atspi", false);
    let target = target(&server);
    let denied = payload(
        server
            .handle(&rpc(
                3,
                "control_act",
                json!({"target":target,"action":{"type":"click"}}),
            ))
            .unwrap(),
    );
    assert_eq!(denied["error"]["code"], "POLICY.DENIED");
    let bad = server
        .handle(&rpc(
            4,
            "control_act",
            json!({"target":target,"action":{"type":"click","x":1,"y":2}}),
        ))
        .unwrap();
    assert_eq!(bad["error"]["code"], -32602);
    let bad = server
        .handle(&rpc(5, "control_capabilities", json!({"allow_input":true})))
        .unwrap();
    assert_eq!(bad["error"]["code"], -32602);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn cancellation_notification_reaches_an_in_flight_control_request() {
    let (server, fake) = server("browser.cdp", true);
    let target = target(&server);
    let running = server.clone();
    let worker = std::thread::spawn(move || {
        payload(
            running
                .handle(&rpc(
                    9,
                    "control_act",
                    json!({"target":target,"action":{"type":"click"}}),
                ))
                .unwrap(),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while fake.calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(server
        .handle(&RpcRequest {
            id: None,
            method: "notifications/cancelled".into(),
            params: json!({"requestId":9,"reason":"test"})
        })
        .is_none());
    let response = worker.join().unwrap();
    assert_eq!(response["effect"], "unknown");
    assert_eq!(response["error"]["code"], "CONTROL.CANCELLED");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}
/// Input handed over a chunk at a time. Each time the server asks for more,
/// everything sent before has been read and handled.
struct Feed {
    chunks: mpsc::Receiver<String>,
    asked: mpsc::Sender<()>,
    pending: Vec<u8>,
}
impl std::io::Read for Feed {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            let _ = self.asked.send(());
            match self.chunks.recv() {
                Ok(chunk) => self.pending = chunk.into_bytes(),
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}
/// The replies the server wrote, one JSON value per line.
struct Replies(mpsc::Sender<Value>, Vec<u8>);
impl std::io::Write for Replies {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.1.extend_from_slice(buf);
        while let Some(end) = self.1.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.1.drain(..=end).collect();
            let _ = self.0.send(serde_json::from_slice(&line).unwrap());
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn line(id: u64, name: &str, args: Value) -> String {
    let call = json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name":name,"arguments":args}});
    format!("{call}\n")
}
#[test]
fn a_call_cancelled_while_it_waits_is_answered_without_running() {
    let (session, fake) = session("browser.cdp", true);
    let (release, held) = mpsc::channel();
    *fake.gate.lock().unwrap() = Some(held);
    let (send, chunks) = mpsc::channel::<String>();
    let (asked_tx, asked) = mpsc::channel();
    let (replies_tx, replies) = mpsc::channel();
    let serving = std::thread::spawn(move || {
        aad_mcp::serve_with(
            std::io::BufReader::new(Feed {
                chunks,
                asked: asked_tx,
                pending: Vec::new(),
            }),
            Replies(replies_tx, Vec::new()),
            Server::with_control(Arc::new(session)),
        )
    });
    // Send a chunk and wait until the server has read and handled all of it.
    let feed = |chunk: String| {
        send.send(chunk).unwrap();
        asked.recv_timeout(Duration::from_secs(5)).unwrap();
    };
    let reply = || replies.recv_timeout(Duration::from_secs(5)).unwrap();
    asked.recv_timeout(Duration::from_secs(5)).unwrap();

    feed(line(1, "control_targets", json!({})));
    reply();
    feed(line(
        2,
        "control_observe",
        json!({"resource_id":"fixture:one"}),
    ));
    let target = payload(reply())["data"]["nodes"][0]["target"].clone();
    let click = json!({"target":target,"action":{"type":"click"}});

    // The first click holds the line; the second waits behind it and is
    // cancelled there.
    feed(line(3, "control_act", click.clone()));
    let deadline = Instant::now() + Duration::from_secs(2);
    while fake.calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let cancel = json!({"jsonrpc":"2.0","method":"notifications/cancelled",
        "params":{"requestId":4,"reason":"test"}});
    feed(line(4, "control_act", click) + &format!("{cancel}\n"));
    release.send(()).unwrap();

    assert_eq!(payload(reply())["effect"], "applied");
    let cancelled = reply();
    assert_eq!(cancelled["id"], 4);
    assert_eq!(cancelled["result"]["isError"], true);
    let cancelled = payload(cancelled);
    assert_eq!(cancelled["error"]["code"], "CONTROL.CANCELLED");
    assert_eq!(cancelled["effect"], "not_applied");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);

    drop(send);
    serving.join().unwrap().unwrap();
}
#[test]
fn a_remote_connection_advertises_no_local_file_or_legacy_execution_tools() {
    let (server, _) = server("browser.cdp", false);
    let listed = server
        .handle(&RpcRequest {
            id: Some(json!(1)),
            method: "tools/list".into(),
            params: json!({}),
        })
        .unwrap();
    assert!(listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| aad_mcp::control_tools::method(tool["name"].as_str().unwrap()).is_some()));
    assert_eq!(
        server.handle(&rpc(2, "save_workflow", json!({}))).unwrap()["error"]["code"],
        -32602
    );
}

#[test]
#[ignore = "takes the cooperating desktop input lease; run with desktop tests stopped"]
fn saved_control_workflows_own_only_the_resources_their_steps_use() {
    struct Recordings(std::path::PathBuf);
    impl Drop for Recordings {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AAD_RECORDINGS_DIR") };
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let recordings = Recordings(
        std::env::temp_dir().join(format!("aad-mcp-lease-test-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir(&recordings.0).unwrap();
    // The other tests in this integration binary do not access recordings.
    unsafe { std::env::set_var("AAD_RECORDINGS_DIR", &recordings.0) };
    let desktop = Session::new(Vec::new(), Policy::default());
    desktop.input_lease("desktop-input").unwrap();
    let (controlled, fake) = session("browser.cdp", true);
    let server = Server::local(controlled).with_run_store(recordings.0.join("runs.sqlite3"));
    for (name, step) in [
        (
            "click",
            json!({"app":{"provider":"browser.cdp"},"locator":{"name":"Save"},"action":{"type":"click"}}),
        ),
        (
            "check",
            json!({"app":{"provider":"browser.cdp"},"locator":{"name":"Save"},"check":{"present":true,"within_ms":1000}}),
        ),
    ] {
        let saved = payload(
            server
                .handle(&rpc(
                    1,
                    "save_workflow",
                    json!({"name":name,"steps":[step]}),
                ))
                .unwrap(),
        );
        assert_eq!(saved["executed_steps"], 1, "{saved}");
    }
    let ran = payload(
        server
            .handle(&rpc(2, "run_workflow", json!({"name":"click"})))
            .unwrap(),
    );
    assert_eq!(ran["status"], "succeeded", "{ran}");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let checked = payload(
        server
            .handle(&rpc(3, "run_workflow", json!({"name":"check"})))
            .unwrap(),
    );
    assert_eq!(checked["status"], "succeeded", "{checked}");

    let rival = Server::local(Session::new(
        vec![fake.clone()],
        Policy {
            allow_input: true,
            ..Default::default()
        },
    ));
    let refused = payload(
        rival
            .handle(&rpc(4, "run_workflow", json!({"name":"click"})))
            .unwrap(),
    );
    assert_ne!(refused["status"], "succeeded", "{refused}");
    assert!(
        refused.to_string().contains("CONTROL.RESOURCE_BUSY"),
        "{refused}"
    );
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        1,
        "the competing action did not reach the backend"
    );
}
