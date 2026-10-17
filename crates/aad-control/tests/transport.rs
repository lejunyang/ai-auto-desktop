//! Isolated loopback fixtures exercise the same byte transport as SSH stdio.
use aad_control::{
    transport::{self, Endpoint, RemoteClient},
    *,
};
use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

struct Fake {
    writes: AtomicUsize,
    delay: Duration,
}
impl Backend for Fake {
    fn name(&self) -> &str {
        "fixture"
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["click".into()]
    }
    fn resources(&self, _: Duration) -> Result<Vec<BackendResource>> {
        Ok(vec![BackendResource {
            resource: Resource {
                resource_id: "fixture:one".into(),
                provider: "fixture".into(),
                kind: "desktop".into(),
                name: "test".into(),
                capabilities: self.capabilities(),
                metadata: json!({}),
            },
            selector: json!({}),
        }])
    }
    fn observe(&self, _: &Value, _: usize, _: Duration) -> Result<Value> {
        Ok(json!({"nodes":[{"node_id":"n","role":"button","name":"Save"}],"truncated":false}))
    }
    fn act(&self, _: &Value, _: &Value, _: &Action, context: &Context) -> Result<Value> {
        context.commit()?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        Ok(json!({"submitted":true}))
    }
}
struct Harness {
    client: Arc<RemoteClient>,
    socket: TcpStream,
    server: Option<std::thread::JoinHandle<()>>,
    backend: Arc<Fake>,
}
impl Harness {
    fn new(input: bool, delay: Duration, slow_writer: bool) -> Self {
        Self::with_budget(input, delay, slow_writer, 10_000)
    }
    fn with_budget(input: bool, delay: Duration, slow_writer: bool, max_requests: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let backend = Arc::new(Fake {
            writes: AtomicUsize::new(0),
            delay,
        });
        let endpoint = Arc::new(
            Session::new(
                vec![backend.clone()],
                Policy {
                    allow_input: input,
                    ..Default::default()
                },
            )
            .with_limits(
                Duration::from_secs(3600),
                Duration::from_secs(300),
                max_requests,
            )
            .unwrap(),
        );
        let input = server.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let _ = transport::serve(input, server, endpoint);
        });
        let reader = socket.try_clone().unwrap();
        let writer = socket.try_clone().unwrap();
        let client = if slow_writer {
            RemoteClient::from_streams(
                reader,
                SlowWriter {
                    socket: writer,
                    writes: 0,
                },
            )
        } else {
            RemoteClient::from_streams(reader, writer)
        }
        .unwrap();
        Self {
            client: Arc::new(client),
            socket,
            server: Some(server),
            backend,
        }
    }
    fn call(&self, op: Operation) -> Response {
        self.client.request(&Request::new(None, op))
    }
    fn target(&self) -> Target {
        assert!(self.call(Operation::Targets {}).error.is_none());
        let observation = self
            .call(Operation::Observe {
                resource_id: "fixture:one".into(),
                max_nodes: 10,
            })
            .data
            .unwrap();
        serde_json::from_value(observation["nodes"][0]["target"].clone()).unwrap()
    }
}

#[test]
fn terminal_session_budget_closes_the_transport_instead_of_heartbeating_forever() {
    let h = Harness::with_budget(false, Duration::ZERO, false, 2);
    assert!(h.call(Operation::Capabilities {}).error.is_none());
    let response = h.call(Operation::Capabilities {});
    assert_eq!(response.commit_state, CommitState::NotCommitted);
    assert_eq!(response.error.unwrap()["code"], "CONTROL.SESSION_LIMIT");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let response = h.call(Operation::Capabilities {});
        assert_eq!(response.commit_state, CommitState::NotCommitted);
        if response.error.unwrap()["code"] == "CONTROL.DISCONNECTED" {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        self.client.disconnect();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}
struct SlowWriter {
    socket: TcpStream,
    writes: usize,
}
impl Write for SlowWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        if self.writes > 1 {
            std::thread::sleep(Duration::from_millis(700));
        }
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}
#[test]
fn persistent_session_negotiates_targets_observes_finds_and_acts() {
    let h = Harness::new(true, Duration::ZERO, false);
    let target = h.target();
    let found = h.call(Operation::Find {
        observation_id: target.observation_id.clone(),
        locator: Locator {
            name: Some("Save".into()),
            ..Default::default()
        },
    });
    assert_eq!(
        found.data.unwrap()["target"],
        serde_json::to_value(&target).unwrap()
    );
    assert!(h
        .call(Operation::Act {
            target,
            action: Action::Click {}
        })
        .error
        .is_none());
    assert_eq!(h.backend.writes.load(Ordering::SeqCst), 1);
    assert!(h.call(Operation::Close {}).data.unwrap()["closed"]
        .as_bool()
        .unwrap());
}
#[test]
fn client_cannot_elevate_server_grants_or_cross_session_targets() {
    let read_only = Harness::new(false, Duration::ZERO, false);
    let target = read_only.target();
    assert_eq!(
        read_only
            .call(Operation::Act {
                target: target.clone(),
                action: Action::Click {}
            })
            .error
            .unwrap()["code"],
        "POLICY.DENIED"
    );
    assert_eq!(read_only.backend.writes.load(Ordering::SeqCst), 0);
    let other = Harness::new(true, Duration::ZERO, false);
    assert_eq!(
        other
            .call(Operation::Act {
                target,
                action: Action::Click {}
            })
            .error
            .unwrap()["code"],
        "CONTROL.SESSION_MISMATCH"
    );
}
#[test]
fn cancellation_is_processed_during_an_in_flight_write() {
    let h = Harness::new(true, Duration::from_millis(200), false);
    let target = h.target();
    let request = Request::new(
        None,
        Operation::Act {
            target,
            action: Action::Click {},
        },
    );
    let id = request.request_id.clone();
    let client = h.client.clone();
    let worker = std::thread::spawn(move || client.request(&request));
    let deadline = Instant::now() + Duration::from_secs(2);
    while h.backend.writes.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        h.call(Operation::Cancel { request_id: id }).data.unwrap()["cancellation_requested"],
        true
    );
    let result = worker.join().unwrap();
    assert_eq!(result.effect, "unknown");
    assert_eq!(result.error.unwrap()["code"], "CONTROL.CANCELLED");
    assert_eq!(h.backend.writes.load(Ordering::SeqCst), 1);
}
#[test]
fn disconnect_during_input_is_unknown_and_never_retried() {
    let h = Harness::new(true, Duration::from_millis(200), false);
    let target = h.target();
    let client = h.client.clone();
    let worker = std::thread::spawn(move || {
        client.request(&Request::new(
            None,
            Operation::Act {
                target,
                action: Action::Click {},
            },
        ))
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while h.backend.writes.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    // Use the public close operation to wake pending callers deterministically.
    // TCP shutdown on Windows may still deliver the server's cancellation reply
    // through a cloned handle; both paths must retain an unknown input outcome.
    h.client.disconnect();
    h.socket.shutdown(Shutdown::Both).unwrap();
    let response = worker.join().unwrap();
    assert_eq!(response.effect, "unknown");
    assert_eq!(response.commit_state, CommitState::Unknown);
    assert_eq!(response.error.unwrap()["code"], "CONTROL.DISCONNECTED");
    assert_eq!(h.backend.writes.load(Ordering::SeqCst), 1);
}
#[test]
fn blocked_transport_writer_cannot_defeat_the_client_deadline() {
    let h = Harness::new(false, Duration::ZERO, true);
    let mut request = Request::new(None, Operation::Targets {});
    request.timeout_ms = 20;
    let started = Instant::now();
    let response = h.client.request(&request);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(response.error.unwrap()["code"], "CONTROL.TIMEOUT");
}
#[test]
fn framing_rejects_truncation_oversize_and_protocol_injection() {
    assert!(transport::read_frame(&mut BufReader::new(&b"123456\n"[..]), 4).is_err());
    assert!(transport::read_frame(&mut BufReader::new(&b"{}"[..]), 64).is_err());
    assert!(serde_json::from_value::<Request>(json!({"request_id":"a","timeout_ms":20,"operation":{"method":"targets","principal":"admin"}})).is_err());
    assert!(serde_json::from_value::<Request>(json!({"request_id":"a","timeout_ms":20,"principal":"admin","operation":{"method":"targets"}})).is_err());
}

#[test]
fn reconnect_never_replays_pending_input_or_accepts_old_targets() {
    let h = Harness::new(true, Duration::from_millis(150), false);
    let target = h.target();
    let old_target = target.clone();
    let client = h.client.clone();
    let worker = std::thread::spawn(move || {
        client.request(&Request::new(
            None,
            Operation::Act {
                target,
                action: Action::Click {},
            },
        ))
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while h.backend.writes.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    h.socket.shutdown(Shutdown::Both).unwrap();
    assert_eq!(worker.join().unwrap().commit_state, CommitState::Unknown);
    drop(h);
    let new = Harness::new(true, Duration::ZERO, false);
    assert_eq!(new.backend.writes.load(Ordering::SeqCst), 0);
    let rejected = new.call(Operation::Act {
        target: old_target,
        action: Action::Click {},
    });
    assert_eq!(rejected.commit_state, CommitState::NotCommitted);
    assert_eq!(rejected.error.unwrap()["code"], "CONTROL.SESSION_MISMATCH");
    assert_eq!(new.backend.writes.load(Ordering::SeqCst), 0);
}
