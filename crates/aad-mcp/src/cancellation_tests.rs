use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn cancel_after_activation_preserves_session() {
    struct Gated {
        session: aad_control::Session,
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        cancelled: mpsc::Sender<()>,
        retried: mpsc::Sender<()>,
    }
    impl Endpoint for Gated {
        fn session_id(&self) -> &str {
            self.session.id()
        }
        fn request(&self, request: &aad_control::Request) -> aad_control::Response {
            if request.request_id == "closing" {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
            }
            let response = self.session.execute(request);
            if request.request_id != "closing"
                && matches!(request.operation, aad_control::Operation::Close {})
            {
                self.retried.send(()).unwrap();
            }
            if matches!(request.operation, aad_control::Operation::Cancel { .. }) {
                self.cancelled.send(()).unwrap();
            }
            response
        }
        fn disconnect(&self) {
            self.session.close();
        }
    }
    let (entered, ready) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let (cancelled, acknowledged) = mpsc::channel();
    let (retried, retry) = mpsc::channel();
    let server = Server::with_control(Arc::new(Gated {
        session: aad_control::Session::new(Vec::new(), Default::default()),
        entered,
        release: Mutex::new(resume),
        cancelled,
        retried,
    }));
    let (client, input) = mpsc::channel();
    let (output, replies) = mpsc::channel();
    let (published, publication) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        serve_concurrent(
            Input(input, std::io::Cursor::new(Vec::new())),
            Output(output, Some(publication)),
            server,
        )
    });
    let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
    send(
        &client,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"control_close","arguments":{"request_id":"closing"}}}),
    );
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    send(
        &client,
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}),
    );
    acknowledged.recv_timeout(Duration::from_secs(1)).unwrap();
    release.send(()).unwrap();
    assert_eq!(reply(&mut reader)["result"]["isError"], true);
    {
        let (id, name) = (2, "control_close");
        send(
            &client,
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":{}}}),
        );
        // The cancelled reply's newline is visible, but its write has not returned.
        retry.recv_timeout(Duration::from_secs(1)).unwrap();
        published.send(()).unwrap();
        let response = reply(&mut reader);
        assert!(
            response.get("error").is_none() && response["result"]["isError"] != true,
            "{response}"
        );
    }
    drop(client);
    worker.join().unwrap().unwrap();
}

#[test]
fn cancel_after_activation_allows_query_then_close() {
    struct Gated {
        session: aad_control::Session,
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        cancelled: mpsc::Sender<()>,
        retried: mpsc::Sender<()>,
    }
    impl Endpoint for Gated {
        fn session_id(&self) -> &str {
            self.session.id()
        }
        fn request(&self, request: &aad_control::Request) -> aad_control::Response {
            if request.request_id == "closing" {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
            }
            let response = self.session.execute(request);
            if request.request_id != "closing"
                && matches!(request.operation, aad_control::Operation::Close {})
            {
                self.retried.send(()).unwrap();
            }
            if matches!(request.operation, aad_control::Operation::Cancel { .. }) {
                assert_eq!(
                    response.data.as_ref().unwrap()["cancellation_requested"],
                    true
                );
                self.cancelled.send(()).unwrap();
            }
            response
        }
        fn disconnect(&self) {
            self.session.close();
        }
    }
    let (entered, ready) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let (cancelled, acknowledged) = mpsc::channel();
    let (retried, retry) = mpsc::channel();
    let server = Server::with_control(Arc::new(Gated {
        session: aad_control::Session::new(Vec::new(), Default::default()),
        entered,
        release: Mutex::new(resume),
        cancelled,
        retried,
    }));
    let (client, input) = mpsc::channel();
    let (output, replies) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        serve_concurrent(
            Input(input, std::io::Cursor::new(Vec::new())),
            Output(output, None),
            server,
        )
    });
    let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
    send(
        &client,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"control_close","arguments":{"request_id":"closing"}}}),
    );
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    send(
        &client,
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}),
    );
    acknowledged.recv_timeout(Duration::from_secs(1)).unwrap();
    release.send(()).unwrap();
    let cancelled_response = reply(&mut reader);
    assert!(cancelled_response.to_string().contains("CONTROL.CANCELLED"));
    send(
        &client,
        json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"control_targets","arguments":{}}}),
    );
    let query = reply(&mut reader);
    assert!(query.get("error").is_none() && query["result"]["isError"] != true);
    {
        let (id, name) = (2, "control_close");
        send(
            &client,
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":{}}}),
        );
        // The cancelled reply's newline is visible, but its write has not returned.
        retry.recv_timeout(Duration::from_secs(1)).unwrap();
        let response = reply(&mut reader);
        assert!(
            response.get("error").is_none() && response["result"]["isError"] != true,
            "{response}"
        );
    }
    drop(client);
    worker.join().unwrap().unwrap();
}

thread_local! {
    pub(super) static BEFORE_CONTROL_WORKER: std::cell::RefCell<Option<Box<dyn FnOnce() + Send>>> = Default::default();
}

struct Input(mpsc::Receiver<Vec<u8>>, std::io::Cursor<Vec<u8>>);
impl Read for Input {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.1.position() == self.1.get_ref().len() as u64 {
            match self.0.recv_timeout(Duration::from_secs(3)) {
                Ok(bytes) => self.1 = std::io::Cursor::new(bytes),
                Err(_) => return Ok(0),
            }
        }
        self.1.read(out)
    }
}
struct Output(mpsc::Sender<Vec<u8>>, Option<mpsc::Receiver<()>>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.send(bytes.to_vec()).unwrap();
        if bytes.contains(&b'\n') {
            if let Some(gate) = self.1.take() {
                gate.recv_timeout(Duration::from_secs(3)).unwrap();
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct CountEndpoint(aad_control::Session, mpsc::Sender<String>);
impl Endpoint for CountEndpoint {
    fn session_id(&self) -> &str {
        self.0.id()
    }
    fn request(&self, r: &aad_control::Request) -> aad_control::Response {
        if !matches!(r.operation, aad_control::Operation::Capabilities {}) {
            self.1.send(r.request_id.clone()).unwrap();
        }
        self.0.execute(r)
    }
    fn disconnect(&self) {
        self.0.close();
    }
}
fn send(client: &mpsc::Sender<Vec<u8>>, value: Value) {
    client.send(format!("{value}\n").into_bytes()).unwrap();
}
fn reply(reader: &mut impl BufRead) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
fn exercise(queued: bool, cancel: bool) {
    let (calls_tx, calls) = mpsc::channel();
    let server = Server::with_control(Arc::new(CountEndpoint(
        aad_control::Session::new(Vec::new(), Default::default()),
        calls_tx,
    )));
    let (client, input) = mpsc::channel();
    let (output, replies) = mpsc::channel();
    let (newline_tx, newline_rx) = mpsc::channel();
    let (pending_tx, pending_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        BEFORE_CONTROL_WORKER.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            }));
        });
        if queued {
            BEFORE_PENDING_RECEIVE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    pending_tx.send(()).unwrap();
                }));
            });
        }
        serve_concurrent(
            Input(input, std::io::Cursor::new(Vec::new())),
            Output(output, queued.then_some(newline_rx)),
            server,
        )
        .unwrap();
    });
    let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
    if queued {
        send(&client, json!({"jsonrpc":"2.0","id":900,"method":"ping"}));
        assert!(reply(&mut reader).get("result").is_some());
    }
    send(
        &client,
        json!({"jsonrpc":"2.0","id":900,"method":"tools/call","params":{
        "name":"control_act","arguments":{"request_id":"operation","target":{
            "session_id":"s","resource_id":"r","observation_id":"o","node_id":"n"},
            "action":{"type":"click"}}}}),
    );
    if queued {
        pending_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        newline_tx.send(()).unwrap();
    }
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    if cancel {
        send(
            &client,
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":900}}),
        );
    }
    // The ping proves the reader consumed cancellation while this worker is gated.
    send(&client, json!({"jsonrpc":"2.0","id":901,"method":"ping"}));
    assert_eq!(reply(&mut reader)["id"], 901);
    assert!(calls.try_recv().is_err());
    release_tx.send(()).unwrap();
    let response = reply(&mut reader);
    if cancel {
        assert_eq!(response["error"]["code"], -32800);
        assert!(calls.try_recv().is_err());
    } else {
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(
            calls.recv_timeout(Duration::from_secs(1)).unwrap(),
            "operation"
        );
    }
    send(
        &client,
        json!({"jsonrpc":"2.0","id":900,"method":"tools/call","params":{
        "name":"control_targets","arguments":{"request_id":"next-generation"}}}),
    );
    assert!(reply(&mut reader).get("result").is_some());
    assert_eq!(
        calls.recv_timeout(Duration::from_secs(1)).unwrap(),
        "next-generation"
    );
    assert!(calls.try_recv().is_err());
    drop(client);
    worker.join().unwrap();
}
#[test]
fn resumed_control_generation_preserves_cancellation() {
    exercise(true, false);
    exercise(true, true);
}
#[test]
fn direct_control_generation_preserves_cancellation() {
    exercise(false, false);
    exercise(false, true);
}
