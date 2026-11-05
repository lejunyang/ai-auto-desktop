//! Persistent, bounded NDJSON sessions over authenticated inherited stdio.
//!
//! Authentication is delegated to the OS/SSH BEFORE this process is launched.
//! There is deliberately no TCP listener or client-supplied principal/token.
//! Each process owns one session and immutable server-side grants. Never reuse
//! this parser behind an unauthenticated socket or a shared multi-tenant pipe.

use crate::{error, Operation, Request, Response, Result, Session, PROTOCOL_VERSION};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IN_FLIGHT: usize = 8;

pub trait Endpoint: Send + Sync {
    fn session_id(&self) -> &str;
    fn request(&self, request: &Request) -> Response;
    fn disconnect(&self);
    /// Reports authoritative session usability after a recoverable close rejection.
    /// `None` means unknown (including unsupported probes), so the transport
    /// disconnects; only `Some(true)` permits serving further requests.
    fn session_usable(&self) -> Option<bool> {
        None
    }
}

impl Endpoint for Session {
    fn session_usable(&self) -> Option<bool> {
        let _admission = self.active.lock().unwrap();
        Some(
            !self.is_closed()
                && self.created.elapsed() < self.lifetime
                && self.state.lock().unwrap().requests.len() < self.max_requests,
        )
    }
    fn session_id(&self) -> &str {
        self.id()
    }
    fn request(&self, request: &Request) -> Response {
        self.execute(request)
    }
    fn disconnect(&self) {
        self.close();
    }
}

/// Read no more than limit+1 bytes, even when a peer never sends a newline.
pub fn read_frame(input: &mut impl BufRead, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let count = input
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.len() > limit || !bytes.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized or incomplete control frame",
        ));
    }
    Ok(Some(bytes))
}

fn failure(
    request: &Request,
    session_id: &str,
    code: &str,
    message: &str,
    uncertain: bool,
) -> Response {
    let effect = if uncertain && request.mutating() {
        "unknown"
    } else {
        "not_applied"
    };
    Response {
        protocol_version: PROTOCOL_VERSION.into(),
        request_id: request.request_id.clone(),
        session_id: session_id.into(),
        effect: effect.into(),
        commit_state: crate::CommitState::from_effect(effect),
        data: None,
        error: Some(error(code, message).with_effect(effect).to_json()),
    }
}

fn write_response(output: &mut impl Write, response: &Response) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control response exceeded output limit",
        ));
    }
    bytes.push(b'\n');
    output.write_all(&bytes)?;
    output.flush()
}

fn terminal(response: &Response) -> bool {
    response.error.as_ref().is_some_and(|error| {
        matches!(
            error["code"].as_str(),
            Some("CONTROL.SESSION_CLOSED" | "CONTROL.SESSION_LIMIT")
        )
    }) || (response.error.is_none()
        && response
            .data
            .as_ref()
            .is_some_and(|data| data["closed"] == true))
}

#[cfg(test)]
thread_local! {
    static QUEUED_RESPONSES: std::cell::RefCell<Option<Arc<AtomicUsize>>> = Default::default();
    static CLOSE_WAITING: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
    static CLOSE_QUEUE_FULL: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}

/// Cancellation and close bypass the action worker gate. At most eight request
/// workers and sixteen unread input frames exist. Disconnect cancels all work;
/// no action is retried, and an interrupted write's effect stays unknown.
pub fn serve(
    input: impl Read + Send + 'static,
    output: impl Write + Send + 'static,
    endpoint: Arc<dyn Endpoint>,
) -> io::Result<()> {
    struct Disconnect(Arc<dyn Endpoint>);
    impl Drop for Disconnect {
        fn drop(&mut self) {
            self.0.disconnect();
        }
    }
    let _disconnect = Disconnect(endpoint.clone());
    let (tx, rx) = mpsc::sync_channel(16);
    let reader = std::thread::spawn(move || {
        let mut input = BufReader::new(input);
        loop {
            let frame = read_frame(&mut input, MAX_REQUEST_BYTES);
            let done = !matches!(frame, Ok(Some(_)));
            if tx.send(frame).is_err() || done {
                break;
            }
        }
    });
    let (responses, response_queue) = mpsc::sync_channel::<Response>(16);
    #[cfg(test)]
    let queued_responses = QUEUED_RESPONSES.with(|v| v.borrow().clone());
    let writer_failed = Arc::new(AtomicBool::new(false));
    let failure_flag = writer_failed.clone();
    let writer_endpoint = endpoint.clone();
    let writer = std::thread::spawn(move || {
        let mut output = output;
        for response in response_queue {
            let (done, wait) = mpsc::channel::<()>();
            let watchdog = writer_endpoint.clone();
            std::thread::spawn(move || {
                if matches!(
                    wait.recv_timeout(Duration::from_secs(1)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    watchdog.disconnect();
                }
            });
            if write_response(&mut output, &response).is_err() {
                failure_flag.store(true, Ordering::SeqCst);
                writer_endpoint.disconnect();
                break;
            }
            drop(done);
        }
    });
    let cancel_gates = Arc::new(Mutex::new(BTreeMap::<String, Vec<Arc<AtomicBool>>>::new()));

    let active = Arc::new(AtomicUsize::new(0));
    let cancelling = Arc::new(AtomicUsize::new(0));
    let closing = Arc::new(AtomicUsize::new(0));
    let io_failed = Arc::new(AtomicBool::new(false));
    let mut last_message = Instant::now();
    loop {
        if io_failed.load(Ordering::SeqCst) || writer_failed.load(Ordering::SeqCst) {
            endpoint.disconnect();
            break;
        }
        let frame = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let deadline = Instant::now() + Duration::from_secs(1);
                while (cancelling.load(Ordering::SeqCst) + active.load(Ordering::SeqCst)) > 0
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                endpoint.disconnect();
                break;
            }
            Ok(Err(e)) => {
                endpoint.disconnect();
                return Err(e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The connection must send requests/heartbeats while idle. A
                // partial-line peer cannot keep a control process alive forever.
                if last_message.elapsed() > Duration::from_secs(60) {
                    endpoint.disconnect();
                    break;
                }
                continue;
            }
        };
        last_message = Instant::now();
        let request: Request = match serde_json::from_slice(&frame) {
            Ok(request) => request,
            Err(e) => {
                endpoint.disconnect();
                return Err(io::Error::new(io::ErrorKind::InvalidData, e));
            }
        };
        if matches!(request.operation, Operation::Close {}) {
            // Establish the admission boundary in read order. No later frame,
            // including EOF, may race the close response.
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            let closing_endpoint = endpoint.clone();
            let patience =
                Duration::from_millis(request.timeout_ms.clamp(1, crate::MAX_TIMEOUT_MS));
            let predecessors = cancel_gates
                .lock()
                .unwrap()
                .get(&request.request_id)
                .cloned()
                .unwrap_or_default();
            let deadline = Instant::now() + patience;
            let invalid = request.request_id.is_empty()
                || request.request_id.len() > 128
                || request.timeout_ms == 0
                || request.timeout_ms > crate::MAX_TIMEOUT_MS;
            if invalid {
                let _ = done_tx.send(failure(
                    &request,
                    endpoint.session_id(),
                    "CONTROL.INVALID_REQUEST",
                    "bounded id and timeout required",
                    false,
                ));
            } else {
                #[cfg(test)]
                CLOSE_WAITING.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().take() {
                        hook();
                    }
                });
                // Waiting for prior cancellation is not an uncertain dispatched close.
                while predecessors.iter().any(|g| !g.load(Ordering::SeqCst))
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if Instant::now() >= deadline {
                    let _ = done_tx.send(failure(
                        &request,
                        endpoint.session_id(),
                        "CONTROL.CANCELLED",
                        "close not dispatched before deadline",
                        false,
                    ));
                } else {
                    std::thread::spawn(move || {
                        let response = if Instant::now() >= deadline {
                            failure(
                                &request,
                                closing_endpoint.session_id(),
                                "CONTROL.CANCELLED",
                                "close not dispatched before deadline",
                                false,
                            )
                        } else {
                            closing_endpoint.request(&request)
                        };
                        let _ = done_tx.send(response);
                    });
                }
            }
            if let Ok(response) =
                done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                let recoverable = response.error.as_ref().is_some_and(|e| {
                    matches!(
                        e["code"].as_str(),
                        Some(
                            "CONTROL.CANCELLED"
                                | "CONTROL.DUPLICATE_REQUEST"
                                | "CONTROL.SESSION_MISMATCH"
                                | "CONTROL.INVALID_REQUEST"
                        )
                    )
                }) && response.effect == "not_applied";
                // Keep ownership of a close response until it is queued. A
                // permanently blocked writer still terminates this connection.
                let deadline = Instant::now() + patience.min(Duration::from_secs(1));
                let mut pending = response;
                let queued = loop {
                    match responses.try_send(pending) {
                        Ok(()) => break true,
                        Err(mpsc::TrySendError::Disconnected(_)) => break false,
                        Err(mpsc::TrySendError::Full(response)) => {
                            pending = response;
                            #[cfg(test)]
                            CLOSE_QUEUE_FULL.with(|hook| {
                                if let Some(hook) = hook.borrow_mut().take() {
                                    hook();
                                }
                            });
                            if Instant::now() >= deadline || writer_failed.load(Ordering::SeqCst) {
                                break false;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                };
                // Unknown authority is fail-closed: only explicit usability resumes service.
                if recoverable && queued && endpoint.session_usable() == Some(true) {
                    continue;
                }
            }
            break;
        }
        let (lane, limit) = match request.operation {
            Operation::Cancel { .. } => (&cancelling, MAX_IN_FLIGHT + 2),
            Operation::Close {} => (&closing, 1),
            _ => (&active, MAX_IN_FLIGHT),
        };
        if lane.load(Ordering::SeqCst) >= limit {
            if responses
                .try_send(failure(
                    &request,
                    endpoint.session_id(),
                    "CONTROL.BUSY",
                    "too many in-flight requests",
                    false,
                ))
                .is_err()
            {
                endpoint.disconnect();
                break;
            }
            continue;
        }
        cancel_gates.lock().unwrap().retain(|_, gates| {
            gates.retain(|gate| !gate.load(Ordering::SeqCst));
            !gates.is_empty()
        });
        let registration = if let Operation::Cancel { request_id } = &request.operation {
            let gate = Arc::new(AtomicBool::new(false));
            cancel_gates
                .lock()
                .unwrap()
                .entry(request_id.clone())
                .or_default()
                .push(gate.clone());
            Some(gate)
        } else {
            None
        };
        let predecessors = cancel_gates
            .lock()
            .unwrap()
            .get(&request.request_id)
            .cloned()
            .unwrap_or_default();
        lane.fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        let queued_responses = queued_responses.clone();
        let (endpoint, responses, active, io_failed) = (
            endpoint.clone(),
            responses.clone(),
            lane.clone(),
            io_failed.clone(),
        );
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
            while predecessors.iter().any(|g| !g.load(Ordering::SeqCst))
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let response = if predecessors.iter().any(|g| !g.load(Ordering::SeqCst)) {
                failure(
                    &request,
                    endpoint.session_id(),
                    "CONTROL.CANCELLED",
                    "prior cancellation pending",
                    false,
                )
            } else {
                endpoint.request(&request)
            };
            if let Some(gate) = registration {
                gate.store(true, Ordering::SeqCst);
            }
            if responses.try_send(response.clone()).is_err() {
                io_failed.store(true, Ordering::SeqCst);
                endpoint.disconnect();
            }
            // Expiry/quota are terminal authority boundaries. Do not keep a
            // closed server process alive solely on client heartbeat requests.
            if terminal(&response) {
                io_failed.store(true, Ordering::SeqCst);
            }
            active.fetch_sub(1, Ordering::SeqCst);
            #[cfg(test)]
            if let Some(count) = queued_responses {
                count.fetch_add(1, Ordering::SeqCst);
            }
        });
    }
    endpoint.disconnect();
    drop(responses);
    let deadline = Instant::now() + Duration::from_secs(1);
    while !writer.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    // Reader may still be blocked in inherited stdin after an idle timeout.
    // Do not join it; the CLI process exits and reclaims that private pipe.
    drop(reader);
    Ok(())
}

type Pending = Arc<Mutex<BTreeMap<String, (u8, mpsc::SyncSender<Response>)>>>;

struct Packet {
    bytes: Vec<u8>,
    deadline: Instant,
}

// Each class owns its capacity; timeout cleanup and heartbeat traffic cannot
// occupy the slot reserved for close. Close is dequeued first.
#[derive(Clone, Default)]
struct PacketQueue(
    Arc<(
        Mutex<[std::collections::VecDeque<Packet>; 3]>,
        std::sync::Condvar,
    )>,
);
impl PacketQueue {
    fn try_send(&self, packet: Packet) -> std::result::Result<(), ()> {
        self.enqueue(packet, false)
    }
    fn enqueue(&self, packet: Packet, background: bool) -> std::result::Result<(), ()> {
        let request: Request = serde_json::from_slice(&packet.bytes).map_err(|_| ())?;
        let lane = if background {
            2
        } else {
            match request.operation {
                Operation::Close {} => 0,
                Operation::Cancel { .. } => 1,
                _ => 2,
            }
        };
        let mut queues = self.0 .0.lock().unwrap();
        let queue = &mut queues[lane];
        queue.retain(|p| p.deadline > Instant::now());
        let limit = [1, MAX_IN_FLIGHT + 2, MAX_IN_FLIGHT][lane];
        if queue.len() >= limit {
            return Err(());
        }
        queue.push_back(packet);
        self.0 .1.notify_one();
        Ok(())
    }
    fn recv(&self) -> std::result::Result<Packet, ()> {
        let mut queues = self.0 .0.lock().unwrap();
        loop {
            for queue in queues.iter_mut() {
                if let Some(packet) = queue.pop_front() {
                    return Ok(packet);
                }
            }
            let (next, timed) = self
                .0
                 .1
                .wait_timeout(queues, Duration::from_secs(60))
                .unwrap();
            queues = next;
            if timed.timed_out() {
                return Err(());
            }
        }
    }
}

pub struct RemoteClient {
    id: String,
    writer: PacketQueue,
    pending: Pending,
    closed: Arc<AtomicBool>,
    child: Mutex<Option<Child>>,
}

impl RemoteClient {
    /// The argv is trusted operator configuration (for example ssh -T), never
    /// accepted from a control request or MCP tool. No local shell expansion.
    pub fn spawn(argv: &[String]) -> Result<Self> {
        let program = argv
            .first()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| error("CONTROL.INVALID_REQUEST", "remote command argv is empty"))?;
        let mut child = Command::new(program)
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| error("CONTROL.CONNECT_FAILED", e.to_string()))?;
        let input = child.stdout.take().unwrap();
        let output = child.stdin.take().unwrap();
        match Self::from_streams(input, output) {
            Ok(client) => {
                *client.child.lock().unwrap() = Some(child);
                Ok(client)
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    /// Stream injection is for already authenticated transports and isolated
    /// fixtures. It does not authenticate an arbitrary socket by itself.
    pub fn from_streams(
        input: impl Read + Send + 'static,
        output: impl Write + Send + 'static,
    ) -> Result<Self> {
        let pending: Pending = Arc::new(Mutex::new(BTreeMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let writer = PacketQueue::default();
        let queue = writer.clone();
        let (failed, writing) = (closed.clone(), pending.clone());
        std::thread::spawn(move || {
            let mut output = output;
            while let Ok(packet) = queue.recv() {
                if failed.load(Ordering::SeqCst) {
                    break;
                }
                if Instant::now() >= packet.deadline {
                    continue;
                }
                if output
                    .write_all(&packet.bytes)
                    .and_then(|()| output.flush())
                    .is_err()
                {
                    failed.store(true, Ordering::SeqCst);
                    writing.lock().unwrap().clear();
                    break;
                }
            }
        });
        let (waiting, dead) = (pending.clone(), closed.clone());
        std::thread::spawn(move || {
            let mut input = BufReader::new(input);
            while let Ok(Some(frame)) = read_frame(&mut input, MAX_RESPONSE_BYTES) {
                let response: Response = match serde_json::from_slice(&frame) {
                    Ok(r) => r,
                    Err(_) => break,
                };
                if response.protocol_version != PROTOCOL_VERSION
                    || response.data.is_some() == response.error.is_some()
                    || !matches!(
                        response.effect.as_str(),
                        "not_applied" | "applied" | "contextual" | "unknown"
                    )
                    || response.commit_state != crate::CommitState::from_effect(&response.effect)
                {
                    break;
                }
                let terminal = terminal(&response);
                if let Some((_, sender)) = waiting.lock().unwrap().remove(&response.request_id) {
                    let _ = sender.send(response);
                }
                if terminal {
                    break;
                }
            }
            dead.store(true, Ordering::SeqCst);
            // Dropping senders wakes callers, which classify their own pending
            // write as unknown rather than fabricating a successful response.
            waiting.lock().unwrap().clear();
        });
        let mut client = Self {
            id: String::new(),
            writer,
            pending,
            closed,
            child: Mutex::new(None),
        };
        let request = Request::new(None, Operation::Capabilities {});
        let response = client.exchange(&request);
        if response.error.is_some()
            || response.protocol_version != PROTOCOL_VERSION
            || response.session_id.is_empty()
        {
            return Err(error("CONTROL.CONNECT_FAILED", "remote handshake failed"));
        }
        if response
            .data
            .as_ref()
            .and_then(|v| v["session_id"].as_str())
            != Some(response.session_id.as_str())
        {
            return Err(error(
                "CONTROL.CONNECT_FAILED",
                "remote session handshake mismatch",
            ));
        }
        client.id = response.session_id;
        let (heartbeat, closed, id) = (
            client.writer.clone(),
            client.closed.clone(),
            client.id.clone(),
        );
        std::thread::spawn(move || {
            while !closed.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(20));
                if closed.load(Ordering::SeqCst) {
                    break;
                }
                let request = Request::new(Some(id.clone()), Operation::Capabilities {});
                if let Ok(mut bytes) = serde_json::to_vec(&request) {
                    bytes.push(b'\n');
                    let _ = heartbeat.try_send(Packet {
                        bytes,
                        deadline: Instant::now() + Duration::from_secs(5),
                    });
                }
            }
        });
        Ok(client)
    }

    fn exchange(&self, request: &Request) -> Response {
        if request.request_id.is_empty()
            || request.request_id.len() > 128
            || request.timeout_ms == 0
            || request.timeout_ms > crate::MAX_TIMEOUT_MS
        {
            return failure(
                request,
                &self.id,
                "CONTROL.INVALID_REQUEST",
                "bounded id and timeout required",
                false,
            );
        }
        if self.closed.load(Ordering::SeqCst) {
            return failure(
                request,
                &self.id,
                "CONTROL.DISCONNECTED",
                "control transport is closed",
                false,
            );
        }
        let mut bytes = match serde_json::to_vec(request) {
            Ok(bytes) => bytes,
            Err(e) => {
                return failure(
                    request,
                    &self.id,
                    "CONTROL.INVALID_REQUEST",
                    &e.to_string(),
                    false,
                )
            }
        };
        if bytes.len() + 1 > MAX_REQUEST_BYTES {
            return failure(
                request,
                &self.id,
                "CONTROL.INVALID_REQUEST",
                "request exceeds frame limit",
                false,
            );
        }
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut pending = self.pending.lock().unwrap();
            let lane = match request.operation {
                Operation::Cancel { .. } => 1,
                Operation::Close {} => 2,
                _ => 0,
            };
            let limit = match lane {
                0 => MAX_IN_FLIGHT,
                1 => MAX_IN_FLIGHT + 2,
                _ => 1,
            };
            if pending.values().filter(|(kind, _)| *kind == lane).count() >= limit {
                return failure(
                    request,
                    &self.id,
                    "CONTROL.BUSY",
                    "too many in-flight requests",
                    false,
                );
            }
            if pending.contains_key(&request.request_id) {
                return failure(
                    request,
                    &self.id,
                    "CONTROL.DUPLICATE_REQUEST",
                    "request id is already pending",
                    false,
                );
            }
            pending.insert(request.request_id.clone(), (lane, tx));
        }
        bytes.push(b'\n');
        let written = self.writer.try_send(Packet {
            bytes,
            deadline: Instant::now() + Duration::from_millis(request.timeout_ms),
        });
        if written.is_err() {
            if matches!(
                request.operation,
                Operation::Close {} | Operation::Cancel { .. }
            ) {
                self.disconnect();
            }
            self.pending.lock().unwrap().remove(&request.request_id);
            return failure(
                request,
                &self.id,
                "CONTROL.DISCONNECTED",
                "control request write failed",
                false,
            );
        }
        match rx.recv_timeout(
            Duration::from_millis(request.timeout_ms.clamp(1, crate::MAX_TIMEOUT_MS))
                .saturating_add(Duration::from_millis(250)),
        ) {
            Ok(response) => {
                if !self.id.is_empty() && response.session_id != self.id {
                    return failure(
                        request,
                        &self.id,
                        "CONTROL.SESSION_MISMATCH",
                        "response belongs to another session",
                        true,
                    );
                }
                response
            }
            Err(reason) => {
                if matches!(request.operation, Operation::Close {}) {
                    self.disconnect();
                }
                self.pending.lock().unwrap().remove(&request.request_id);
                if matches!(reason, mpsc::RecvTimeoutError::Timeout) {
                    // Cancellation is best effort after a client deadline. Its
                    // acknowledgement cannot prove that an input was not applied.
                    let cancel = Request::new(
                        Some(self.id.clone()),
                        Operation::Cancel {
                            request_id: request.request_id.clone(),
                        },
                    );
                    if let Ok(mut encoded) = serde_json::to_vec(&cancel) {
                        encoded.push(b'\n');
                        let _ = self.writer.enqueue(
                            Packet {
                                bytes: encoded,
                                deadline: Instant::now() + Duration::from_secs(1),
                            },
                            true,
                        );
                    }
                }
                failure(
                    request,
                    &self.id,
                    if matches!(reason, mpsc::RecvTimeoutError::Timeout) {
                        "CONTROL.TIMEOUT"
                    } else {
                        "CONTROL.DISCONNECTED"
                    },
                    "remote request interrupted; never automatically retry a write",
                    true,
                )
            }
        }
    }
}

impl Endpoint for RemoteClient {
    fn session_usable(&self) -> Option<bool> {
        // Ask the authority after the rejected close has been queued downstream.
        // This bounded read consumes a request ID, including the last quota slot;
        // the authority reports usability after admitting that read.
        let mut probe = Request::new(Some(self.id.clone()), Operation::Capabilities {});
        probe.timeout_ms = 1000;
        let response = self.exchange(&probe);
        if response.error.is_some() {
            return Some(false);
        }
        response
            .data
            .and_then(|data| data["session_usable"].as_bool())
    }
    fn session_id(&self) -> &str {
        &self.id
    }
    fn request(&self, request: &Request) -> Response {
        let mut request = request.clone();
        if request.session_id.is_none() {
            request.session_id = Some(self.id.clone());
        }
        self.exchange(&request)
    }
    fn disconnect(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().unwrap().clear();
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Drop for RemoteClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    fn packet(operation: Operation) -> Packet {
        Packet {
            bytes: serde_json::to_vec(&Request::new(None, operation)).unwrap(),
            deadline: Instant::now() + Duration::from_secs(10),
        }
    }
    #[test]
    fn close_has_reserved_capacity_and_priority_over_timeout_cleanup() {
        let queue = PacketQueue::default();
        for _ in 0..MAX_IN_FLIGHT {
            assert!(queue.try_send(packet(Operation::Capabilities {})).is_ok());
        }
        for n in 0..MAX_IN_FLIGHT + 2 {
            assert!(queue
                .try_send(packet(Operation::Cancel {
                    request_id: n.to_string()
                }))
                .is_ok());
        }
        assert!(queue.try_send(packet(Operation::Capabilities {})).is_err());
        assert!(queue.try_send(packet(Operation::Close {})).is_ok());
        let sent: Request = serde_json::from_slice(&queue.recv().unwrap().bytes).unwrap();
        assert!(matches!(sent.operation, Operation::Close {}));
    }
    #[test]
    fn same_target_cancel_frames_keep_their_independent_deadlines() {
        let queue = PacketQueue::default();
        let first = packet(Operation::Cancel {
            request_id: "victim".into(),
        });
        let first_id: Request = serde_json::from_slice(&first.bytes).unwrap();
        queue.try_send(first).unwrap();
        let mut second = packet(Operation::Cancel {
            request_id: "victim".into(),
        });
        second.deadline = Instant::now() - Duration::from_millis(1);
        queue.try_send(second).unwrap();
        let sent: Request = serde_json::from_slice(&queue.recv().unwrap().bytes).unwrap();
        assert_eq!(sent.request_id, first_id.request_id);
    }

    #[test]
    fn pending_cancel_orders_real_session_admission_before_eof() {
        struct CancelGate {
            session: Session,
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            normal: AtomicUsize,
        }
        impl Endpoint for CancelGate {
            fn session_usable(&self) -> Option<bool> {
                self.session.session_usable()
            }
            fn session_id(&self) -> &str {
                self.session.id()
            }
            fn disconnect(&self) {
                self.session.close();
            }
            fn request(&self, request: &Request) -> Response {
                if matches!(request.operation, Operation::Cancel { .. }) {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                } else {
                    self.normal.fetch_add(1, Ordering::SeqCst);
                }
                self.session.execute(request)
            }
        }
        let (entered, seen) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let ep = Arc::new(CancelGate {
            session: Session::new(Vec::new(), Default::default()),
            entered,
            release: Mutex::new(gate),
            normal: AtomicUsize::new(0),
        });
        let mut victim = Request::new(Some(ep.session_id().into()), Operation::Capabilities {});
        victim.request_id = "victim".into();
        let cancel = Request::new(
            Some(ep.session_id().into()),
            Operation::Cancel {
                request_id: "victim".into(),
            },
        );
        let mut input = Vec::new();
        for request in [cancel, victim] {
            serde_json::to_writer(&mut input, &request).unwrap();
            input.push(b'\n');
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let (endpoint, bytes) = (ep.clone(), output.clone());
        let worker =
            std::thread::spawn(move || serve(std::io::Cursor::new(input), Output(bytes), endpoint));
        seen.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(ep.normal.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        let bytes = output.lock().unwrap();
        let replies: Vec<Response> = bytes
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect();
        assert_eq!(replies.len(), 2);
        let victim = replies.iter().find(|r| r.request_id == "victim").unwrap();
        assert!(matches!(
            victim.error.as_ref().and_then(|e| e["code"].as_str()),
            Some("CONTROL.CANCELLED")
        ));
        assert!(replies.iter().any(|r| r
            .data
            .as_ref()
            .is_some_and(|v| v["cancellation_requested"] == true)));
    }

    #[test]
    fn pending_cancel_orders_close_before_eof() {
        struct CancelGate {
            session: Session,
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            normal: AtomicUsize,
        }
        impl Endpoint for CancelGate {
            fn session_usable(&self) -> Option<bool> {
                self.session.session_usable()
            }
            fn session_id(&self) -> &str {
                self.session.id()
            }
            fn disconnect(&self) {
                self.session.close();
            }
            fn request(&self, request: &Request) -> Response {
                if matches!(request.operation, Operation::Cancel { .. }) {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                } else {
                    self.normal.fetch_add(1, Ordering::SeqCst);
                }
                self.session.execute(request)
            }
        }
        let (entered, seen) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let ep = Arc::new(CancelGate {
            session: Session::new(Vec::new(), Default::default()),
            entered,
            release: Mutex::new(gate),
            normal: AtomicUsize::new(0),
        });
        let mut victim = Request::new(Some(ep.session_id().into()), Operation::Close {});
        victim.request_id = "victim".into();
        let cancel = Request::new(
            Some(ep.session_id().into()),
            Operation::Cancel {
                request_id: "victim".into(),
            },
        );
        let mut input = Vec::new();
        for request in [cancel, victim] {
            serde_json::to_writer(&mut input, &request).unwrap();
            input.push(b'\n');
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let (endpoint, bytes) = (ep.clone(), output.clone());
        let worker =
            std::thread::spawn(move || serve(std::io::Cursor::new(input), Output(bytes), endpoint));
        seen.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(ep.normal.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        let bytes = output.lock().unwrap();
        let replies: Vec<Response> = bytes
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect();
        assert_eq!(replies.len(), 2);
        let victim = replies.iter().find(|r| r.request_id == "victim").unwrap();
        assert!(matches!(
            victim.error.as_ref().and_then(|e| e["code"].as_str()),
            Some("CONTROL.CANCELLED")
        ));
        assert!(replies.iter().any(|r| r
            .data
            .as_ref()
            .is_some_and(|v| v["cancellation_requested"] == true)));
    }

    #[cfg(unix)]
    #[test]
    fn close_timeout_terminates_blocked_remote_transport_and_real_session() {
        use std::os::unix::net::UnixStream;
        struct Slow {
            stream: UnixStream,
            armed: Arc<AtomicBool>,
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Drop for Slow {
            fn drop(&mut self) {
                let _ = self.stream.shutdown(std::net::Shutdown::Write);
            }
        }
        impl Write for Slow {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                self.stream.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.stream.flush()
            }
        }
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let session = Arc::new(Session::new(Vec::new(), Default::default()));
        let endpoint = session.clone();
        let server = std::thread::spawn(move || {
            serve(server_socket.try_clone().unwrap(), server_socket, endpoint).unwrap()
        });
        let armed = Arc::new(AtomicBool::new(false));
        let (entered, seen) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let client = Arc::new(
            RemoteClient::from_streams(
                client_socket.try_clone().unwrap(),
                Slow {
                    stream: client_socket,
                    armed: armed.clone(),
                    entered,
                    release: gate,
                },
            )
            .unwrap(),
        );
        armed.store(true, Ordering::SeqCst);
        let ordinary_client = client.clone();
        let ordinary = std::thread::spawn(move || {
            ordinary_client.request(&Request::new(None, Operation::Capabilities {}))
        });
        seen.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut close = Request::new(None, Operation::Close {});
        close.timeout_ms = 100;
        let started = Instant::now();
        assert!(client.request(&close).error.is_some());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(client
            .request(&Request::new(None, Operation::Capabilities {}))
            .error
            .is_some());
        release.send(()).unwrap();
        ordinary.join().unwrap();
        server.join().unwrap();
        assert_eq!(
            session
                .execute(&Request::new(
                    Some(session.id().into()),
                    Operation::Capabilities {}
                ))
                .error
                .unwrap()["code"],
            "CONTROL.SESSION_CLOSED"
        );
    }

    #[test]
    fn blocked_output_does_not_block_close_or_retain_authority() {
        struct Gate {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            first: bool,
        }
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let session = Arc::new(Session::new(Vec::new(), Default::default()));
        let mut input = Vec::new();
        for operation in
            std::iter::repeat_n(Operation::Capabilities {}, 9).chain([Operation::Close {}])
        {
            let request = Request::new(Some(session.id().into()), operation);
            serde_json::to_writer(&mut input, &request).unwrap();
            input.push(b'\n');
        }
        let (entered, seen) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let endpoint = session.clone();
        let worker = std::thread::spawn(move || {
            serve(
                std::io::Cursor::new(input),
                Gate {
                    entered,
                    release: gate,
                    first: true,
                },
                endpoint,
            )
        });
        seen.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let response = session.execute(&Request::new(
                Some(session.id().into()),
                Operation::Capabilities {},
            ));
            if response.error.is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "close blocked behind output");
            std::thread::sleep(Duration::from_millis(5));
        }
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_close_survives_full_response_queue() {
        use std::os::unix::net::UnixStream;
        struct Gate {
            socket: UnixStream,
            armed: Arc<AtomicBool>,
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.send(()).unwrap();
                    self.release.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                self.socket.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.socket.flush()
            }
        }
        let (mut client, socket) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let session = Arc::new(Session::new(Vec::new(), Default::default()));
        let id = session.id().to_string();
        let armed = Arc::new(AtomicBool::new(false));
        let queued = Arc::new(AtomicUsize::new(0));
        let (release, gate) = mpsc::channel();
        let (entered, writer_blocked) = mpsc::channel();
        let (full, observed_full) = mpsc::channel();
        let (endpoint, flag, count) = (session.clone(), armed.clone(), queued.clone());
        let server = std::thread::spawn(move || {
            QUEUED_RESPONSES.with(|v| *v.borrow_mut() = Some(count));
            CLOSE_QUEUE_FULL
                .with(|v| *v.borrow_mut() = Some(Box::new(move || full.send(()).unwrap())));
            serve(
                socket.try_clone().unwrap(),
                Gate {
                    socket,
                    armed: flag,
                    entered,
                    release: gate,
                },
                endpoint,
            )
        });
        let send = |client: &mut UnixStream, name: &str, operation| {
            let mut request = Request::new(Some(id.clone()), operation);
            request.request_id = name.into();
            serde_json::to_writer(&mut *client, &request).unwrap();
            client.write_all(b"\n").unwrap();
        };
        let receive = |reader: &mut BufReader<UnixStream>| {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            serde_json::from_str::<Response>(&line).unwrap()
        };
        send(
            &mut client,
            "cancel",
            Operation::Cancel {
                request_id: "closing".into(),
            },
        );
        assert_eq!(
            receive(&mut reader).data.unwrap()["cancellation_requested"],
            true
        );
        armed.store(true, Ordering::SeqCst);
        for index in 0..17 {
            let operation = if index < 8 {
                Operation::Capabilities {}
            } else {
                Operation::Cancel {
                    request_id: format!("unused-{index}"),
                }
            };
            send(&mut client, &format!("load-{index}"), operation);
            if index == 0 {
                writer_blocked.recv_timeout(Duration::from_secs(1)).unwrap();
            }
            // A completed enqueue, not endpoint entry, establishes queue occupancy.
            let deadline = Instant::now() + Duration::from_secs(1);
            while queued.load(Ordering::SeqCst) < index + 2 {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
        send(&mut client, "closing", Operation::Close {});
        observed_full.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        let mut cancelled = false;
        for _ in 0..18 {
            let response = receive(&mut reader);
            if response.request_id == "closing" {
                assert_eq!(response.error.unwrap()["code"], "CONTROL.CANCELLED");
                cancelled = true;
            }
        }
        assert!(cancelled);
        assert!(!session.is_closed());
        send(&mut client, "query", Operation::Capabilities {});
        assert!(receive(&mut reader).error.is_none());
        send(&mut client, "final-close", Operation::Close {});
        assert_eq!(receive(&mut reader).data.unwrap()["closed"], true);
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejected_close_rechecks_authority_after_queue_wait() {
        for mode in 0..4 {
            for rejection in 0..4 {
                use std::os::unix::net::UnixStream;
                struct Gate {
                    socket: UnixStream,
                    armed: Arc<AtomicBool>,
                    entered: mpsc::Sender<()>,
                    release: mpsc::Receiver<()>,
                }
                impl Write for Gate {
                    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                        if self.armed.swap(false, Ordering::SeqCst) {
                            self.entered.send(()).unwrap();
                            self.release.recv_timeout(Duration::from_secs(2)).unwrap();
                        }
                        self.socket.write(bytes)
                    }
                    fn flush(&mut self) -> io::Result<()> {
                        self.socket.flush()
                    }
                }
                let (mut client, socket) = UnixStream::pair().unwrap();
                client
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(client.try_clone().unwrap());
                let mut raw = Session::new(Vec::new(), Default::default())
                    .with_limits(Duration::from_secs(60), Duration::from_secs(30), 20)
                    .unwrap();
                if mode == 2 {
                    raw.created = Instant::now() - Duration::from_millis(59700);
                }
                if mode == 3 {
                    raw.max_requests = 100;
                }
                let session = Arc::new(raw);
                let lease_key = format!("queued-{}", session.id());
                drop(session.input_lease(&lease_key).unwrap());
                session.state.lock().unwrap().observations.insert(
                    "retained".into(),
                    crate::Observation {
                        scope: None,
                        max_nodes: 1,
                        provider: "fixture".into(),
                        resource_id: "fixture:one".into(),
                        raw: serde_json::json!({}),
                        nodes: vec![serde_json::json!({"node_id":"n"})],
                        created: Instant::now(),
                    },
                );
                let id = session.id().to_string();
                let armed = Arc::new(AtomicBool::new(false));
                let queued = Arc::new(AtomicUsize::new(0));
                let (release, gate) = mpsc::channel();
                let (entered, writer_blocked) = mpsc::channel();
                let (full, observed_full) = mpsc::channel();
                let (endpoint, flag, count) = (session.clone(), armed.clone(), queued.clone());
                let server = std::thread::spawn(move || {
                    QUEUED_RESPONSES.with(|v| *v.borrow_mut() = Some(count));
                    CLOSE_QUEUE_FULL
                        .with(|v| *v.borrow_mut() = Some(Box::new(move || full.send(()).unwrap())));
                    serve(
                        socket.try_clone().unwrap(),
                        Gate {
                            socket,
                            armed: flag,
                            entered,
                            release: gate,
                        },
                        endpoint,
                    )
                });
                let send = |client: &mut UnixStream, name: &str, operation| {
                    let mut request = Request::new(Some(id.clone()), operation);
                    request.request_id = name.into();
                    serde_json::to_writer(&mut *client, &request).unwrap();
                    client.write_all(b"\n").unwrap();
                };
                let receive = |reader: &mut BufReader<UnixStream>| {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    serde_json::from_str::<Response>(&line).unwrap()
                };
                send(
                    &mut client,
                    "cancel",
                    Operation::Cancel {
                        request_id: "closing".into(),
                    },
                );
                assert_eq!(
                    receive(&mut reader).data.unwrap()["cancellation_requested"],
                    true
                );
                armed.store(true, Ordering::SeqCst);
                for index in 0..17 {
                    let operation = if index < 8 {
                        Operation::Capabilities {}
                    } else {
                        Operation::Cancel {
                            request_id: format!("unused-{index}"),
                        }
                    };
                    send(&mut client, &format!("load-{index}"), operation);
                    if index == 0 {
                        writer_blocked.recv_timeout(Duration::from_secs(1)).unwrap();
                    }
                    // A completed enqueue, not endpoint entry, establishes queue occupancy.
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while queued.load(Ordering::SeqCst) < index + 2 {
                        assert!(Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                }
                let mut close = Request::new(Some(id.clone()), Operation::Close {});
                close.request_id = "closing".into();
                let expected = match rejection {
                    0 => "CONTROL.CANCELLED",
                    1 => {
                        close.request_id = "load-0".into();
                        "CONTROL.DUPLICATE_REQUEST"
                    }
                    2 => {
                        close.session_id = Some("wrong".into());
                        "CONTROL.SESSION_MISMATCH"
                    }
                    _ => {
                        close.request_id.clear();
                        "CONTROL.INVALID_REQUEST"
                    }
                };
                let (admit, admission) = mpsc::channel();
                let (started, in_flight) = mpsc::channel();
                let concurrent = session.clone();
                let quota_worker = std::thread::spawn(move || {
                    started.send(()).unwrap();
                    if admission.recv().unwrap() {
                        while concurrent.state.lock().unwrap().requests.len()
                            < concurrent.max_requests
                        {
                            assert!(concurrent
                                .execute(&Request::new(None, Operation::Capabilities {}))
                                .error
                                .is_none());
                        }
                    }
                });
                in_flight.recv_timeout(Duration::from_secs(1)).unwrap();
                serde_json::to_writer(&mut client, &close).unwrap();
                client.write_all(b"\n").unwrap();
                observed_full.recv_timeout(Duration::from_secs(1)).unwrap();
                admit.send(mode == 1).unwrap();
                quota_worker.join().unwrap();
                send(&mut client, "later-query", Operation::Capabilities {});
                if mode == 1 {
                    assert_eq!(session.session_usable(), Some(false));
                } else if mode == 2 {
                    while session.created.elapsed() < session.lifetime {
                        std::thread::yield_now();
                    }
                } else if mode == 0 {
                    session.close();
                }
                release.send(()).unwrap();
                let mut cancelled = false;
                for _ in 0..18 {
                    let response = receive(&mut reader);
                    if response.request_id == close.request_id && response.error.is_some() {
                        assert_eq!(
                            response.error.as_ref().map(|e| &e["code"]),
                            Some(&serde_json::json!(expected))
                        );
                        cancelled = true;
                    }
                }
                assert!(cancelled);
                if mode == 3 {
                    assert!(receive(&mut reader).error.is_none());
                    assert!(!session.state.lock().unwrap().observations.is_empty());
                    send(&mut client, "final-close", Operation::Close {});
                    assert!(receive(&mut reader).error.is_none());
                }
                let deadline = Instant::now() + Duration::from_millis(500);
                while !server.is_finished() {
                    assert!(
                        Instant::now() < deadline,
                        "serve resumed after terminal change"
                    );
                    std::thread::yield_now();
                }
                client.shutdown(std::net::Shutdown::Write).unwrap();
                server.join().unwrap().unwrap();
                assert!(session.state.lock().unwrap().observations.is_empty());
                if mode != 3 {
                    assert!(!session
                        .state
                        .lock()
                        .unwrap()
                        .requests
                        .contains("later-query"));
                }
                assert!(crate::lease::InputLease::acquire(&lease_key).is_ok());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_remote_close_preserves_connection() {
        use std::os::unix::net::UnixStream;
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let session = Arc::new(Session::new(Vec::new(), Default::default()));
        let endpoint = session.clone();
        let server = std::thread::spawn(move || {
            serve(server_socket.try_clone().unwrap(), server_socket, endpoint)
        });
        let client =
            RemoteClient::from_streams(client_socket.try_clone().unwrap(), client_socket).unwrap();
        let response = client.request(&Request::new(
            None,
            Operation::Cancel {
                request_id: "closing".into(),
            },
        ));
        assert_eq!(response.data.unwrap()["cancellation_requested"], true);
        let mut request = Request::new(None, Operation::Close {});
        request.request_id = "closing".into();
        let response = client.request(&request);
        assert_eq!(response.error.unwrap()["code"], "CONTROL.CANCELLED");
        assert!(!session.is_closed());
        assert!(client
            .request(&Request::new(None, Operation::Capabilities {}))
            .error
            .is_none());
        assert!(client
            .request(&Request::new(None, Operation::Close {}))
            .error
            .is_none());
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pending_close_timeout_preserves_real_connection() {
        use std::os::unix::net::UnixStream;
        struct Gate {
            session: Session,
            release: Mutex<mpsc::Receiver<()>>,
            closes: AtomicUsize,
        }
        impl Endpoint for Gate {
            fn session_id(&self) -> &str {
                self.session.id()
            }
            fn session_usable(&self) -> Option<bool> {
                self.session.session_usable()
            }
            fn disconnect(&self) {
                self.session.close();
            }
            fn request(&self, r: &Request) -> Response {
                if matches!(r.operation, Operation::Cancel { .. }) {
                    self.release.lock().unwrap().recv().unwrap();
                }
                if matches!(r.operation, Operation::Close {}) {
                    self.closes.fetch_add(1, Ordering::SeqCst);
                }
                self.session.execute(r)
            }
        }
        let (mut client, socket) = UnixStream::pair().unwrap();
        let (release, gate) = mpsc::channel();
        let (waiting, waited) = mpsc::channel();
        let endpoint = Arc::new(Gate {
            session: Session::new(Vec::new(), Default::default()),
            release: Mutex::new(gate),
            closes: AtomicUsize::new(0),
        });
        let ep = endpoint.clone();
        let worker = std::thread::spawn(move || {
            CLOSE_WAITING.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    waiting.send(()).unwrap();
                }))
            });
            serve(socket.try_clone().unwrap(), socket, ep)
        });
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut send = |r: Request| {
            serde_json::to_writer(&mut client, &r).unwrap();
            client.write_all(b"\n").unwrap();
        };
        let mut cancel = Request::new(
            Some(endpoint.session_id().into()),
            Operation::Cancel {
                request_id: "close".into(),
            },
        );
        cancel.request_id = "cancel".into();
        send(cancel);
        let mut close = Request::new(Some(endpoint.session_id().into()), Operation::Close {});
        close.request_id = "close".into();
        close.timeout_ms = 100;
        send(close);
        waited.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(endpoint.closes.load(Ordering::SeqCst), 0);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(response.error.unwrap()["code"], "CONTROL.CANCELLED");
        assert_eq!(response.effect, "not_applied");
        assert!(!endpoint.session.is_closed());
        release.send(()).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(response.data.unwrap()["cancellation_requested"], true);
        send(Request::new(None, Operation::Capabilities {}));
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Response>(&line)
            .unwrap()
            .error
            .is_none());
        send(Request::new(
            Some(endpoint.session_id().into()),
            Operation::Close {},
        ));
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Response>(&line)
            .unwrap()
            .error
            .is_none());
        worker.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pending_close_released_before_deadline_preserves_connection() {
        use std::os::unix::net::UnixStream;
        struct Gate {
            session: Session,
            release: Mutex<mpsc::Receiver<()>>,
            closes: AtomicUsize,
        }
        impl Endpoint for Gate {
            fn session_id(&self) -> &str {
                self.session.id()
            }
            fn session_usable(&self) -> Option<bool> {
                self.session.session_usable()
            }
            fn disconnect(&self) {
                self.session.close();
            }
            fn request(&self, r: &Request) -> Response {
                if matches!(r.operation, Operation::Cancel { .. }) {
                    self.release.lock().unwrap().recv().unwrap();
                }
                if matches!(r.operation, Operation::Close {}) {
                    self.closes.fetch_add(1, Ordering::SeqCst);
                }
                self.session.execute(r)
            }
        }
        let (mut client, socket) = UnixStream::pair().unwrap();
        let (release, gate) = mpsc::channel();
        let (waiting, waited) = mpsc::channel();
        let endpoint = Arc::new(Gate {
            session: Session::new(Vec::new(), Default::default()),
            release: Mutex::new(gate),
            closes: AtomicUsize::new(0),
        });
        let ep = endpoint.clone();
        let worker = std::thread::spawn(move || {
            CLOSE_WAITING.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    waiting.send(()).unwrap();
                }))
            });
            serve(socket.try_clone().unwrap(), socket, ep)
        });
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut send = |r: Request| {
            serde_json::to_writer(&mut client, &r).unwrap();
            client.write_all(b"\n").unwrap();
        };
        let mut cancel = Request::new(
            Some(endpoint.session_id().into()),
            Operation::Cancel {
                request_id: "close".into(),
            },
        );
        cancel.request_id = "cancel".into();
        send(cancel);
        let mut close = Request::new(Some(endpoint.session_id().into()), Operation::Close {});
        close.request_id = "close".into();
        close.timeout_ms = 2000;
        send(close);
        waited.recv_timeout(Duration::from_secs(1)).unwrap();
        let probe_deadline = Instant::now() + Duration::from_millis(100);
        while endpoint.closes.load(Ordering::SeqCst) == 0 && Instant::now() < probe_deadline {
            std::thread::yield_now();
        }
        assert_eq!(endpoint.closes.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let mut line = String::new();
        let mut replies = Vec::new();
        for _ in 0..2 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            replies.push(serde_json::from_str::<Response>(&line).unwrap());
        }
        let cancelled = replies.iter().find(|r| r.request_id == "close").unwrap();
        assert_eq!(
            cancelled.error.as_ref().unwrap()["code"],
            "CONTROL.CANCELLED"
        );
        assert_eq!(cancelled.effect, "not_applied");
        let registered = replies.iter().find(|r| r.request_id == "cancel").unwrap();
        assert_eq!(
            registered.data.as_ref().unwrap()["cancellation_requested"],
            true
        );
        assert!(!endpoint.session.is_closed());
        send(Request::new(None, Operation::Capabilities {}));
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Response>(&line)
            .unwrap()
            .error
            .is_none());
        send(Request::new(
            Some(endpoint.session_id().into()),
            Operation::Close {},
        ));
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Response>(&line)
            .unwrap()
            .error
            .is_none());
        worker.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn invalid_close_respects_session_authority() {
        use std::os::unix::net::UnixStream;
        for mode in 0..5 {
            let (mut client, socket) = UnixStream::pair().unwrap();
            let mut raw = Session::new(Vec::new(), Default::default());
            if mode == 2 {
                raw = raw
                    .with_limits(Duration::from_secs(60), Duration::from_secs(30), 1)
                    .unwrap();
            }
            if mode == 3 {
                raw.created = Instant::now() - raw.lifetime;
            }
            if mode == 4 {
                raw.close();
            }
            let session = Arc::new(raw);
            let endpoint = session.clone();
            let server =
                std::thread::spawn(move || serve(socket.try_clone().unwrap(), socket, endpoint));
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let mut exchange = |request: Request| {
                serde_json::to_writer(&mut client, &request).unwrap();
                client.write_all(b"\n").unwrap();
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                serde_json::from_str::<Response>(&line).unwrap()
            };
            let lease_key = format!("quota-{}", session.id());
            if mode == 2 {
                drop(session.input_lease(&lease_key).unwrap());
                assert!(crate::lease::InputLease::acquire(&lease_key).is_err());
                assert!(exchange(Request::new(None, Operation::Capabilities {}))
                    .error
                    .is_none());
            }
            let mut request = Request::new(Some("wrong".into()), Operation::Close {});
            if mode == 0 {
                request.timeout_ms = 0;
            }
            if mode == 1 {
                request.request_id.clear();
            }
            assert!(exchange(request).error.is_some());
            if mode < 2 {
                assert!(exchange(Request::new(None, Operation::Capabilities {}))
                    .error
                    .is_none());
                assert!(
                    exchange(Request::new(Some(session.id().into()), Operation::Close {}))
                        .error
                        .is_none()
                );
            }
            server.join().unwrap().unwrap();
            assert!(session.is_closed());
            if mode == 2 {
                assert!(crate::lease::InputLease::acquire(&lease_key).is_ok());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn authority_probe_preserves_last_business_slot() {
        use std::os::unix::net::UnixStream;
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let session = Arc::new(
            Session::new(Vec::new(), Default::default())
                .with_limits(Duration::from_secs(60), Duration::from_secs(30), 2)
                .unwrap(),
        );
        let endpoint = session.clone();
        let server = std::thread::spawn(move || {
            serve(server_socket.try_clone().unwrap(), server_socket, endpoint)
        });
        let client =
            RemoteClient::from_streams(client_socket.try_clone().unwrap(), client_socket).unwrap();
        let usable = client.session_usable();
        let closed = client.request(&Request::new(None, Operation::Close {}));
        client.disconnect();
        server.join().unwrap().unwrap();
        assert_eq!(
            usable,
            Some(true),
            "authority probe spent the last business slot"
        );
        assert!(closed.error.is_none(), "{closed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn authority_probe_bypasses_eight_pending_business_requests() {
        use std::os::unix::net::UnixStream;
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let session = Arc::new(Session::new(Vec::new(), Default::default()));
        let endpoint = session.clone();
        let server = std::thread::spawn(move || {
            serve(server_socket.try_clone().unwrap(), server_socket, endpoint)
        });
        let client = Arc::new(
            RemoteClient::from_streams(client_socket.try_clone().unwrap(), client_socket).unwrap(),
        );
        let gate = session.gate.lock().unwrap();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let client = client.clone();
                std::thread::spawn(move || {
                    client.request(&Request::new(None, Operation::Capabilities {}))
                })
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(2);
        while session.active.lock().unwrap().len() != 8 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let usable = client.session_usable();
        drop(gate);
        for worker in workers {
            assert!(worker.join().unwrap().error.is_none());
        }
        assert!(client
            .request(&Request::new(None, Operation::Close {}))
            .error
            .is_none());
        server.join().unwrap().unwrap();
        assert_eq!(
            usable,
            Some(true),
            "ordinary request lane blocked authority probe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_remote_close_preserves_connection() {
        use std::os::unix::net::UnixStream;
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let session = Arc::new(Session::new(Vec::new(), Default::default()));
        let endpoint = session.clone();
        let server = std::thread::spawn(move || {
            serve(server_socket.try_clone().unwrap(), server_socket, endpoint)
        });
        let client =
            RemoteClient::from_streams(client_socket.try_clone().unwrap(), client_socket).unwrap();
        let mut query = Request::new(None, Operation::Capabilities {});
        query.request_id = "closing".into();
        assert!(client.request(&query).error.is_none());
        let mut request = Request::new(None, Operation::Close {});
        request.request_id = "closing".into();
        let response = client.request(&request);
        assert_eq!(response.error.unwrap()["code"], "CONTROL.DUPLICATE_REQUEST");
        assert!(!session.is_closed());
        assert!(client
            .request(&Request::new(None, Operation::Capabilities {}))
            .error
            .is_none());
        assert!(client
            .request(&Request::new(None, Operation::Close {}))
            .error
            .is_none());
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejected_close_survives_two_transport_layers() {
        for cancelled in [false, true] {
            use std::os::unix::net::UnixStream;
            let (client_socket, server_socket) = UnixStream::pair().unwrap();
            let session = Arc::new(Session::new(Vec::new(), Default::default()));
            let endpoint = session.clone();
            let server = std::thread::spawn(move || {
                serve(server_socket.try_clone().unwrap(), server_socket, endpoint)
            });
            let client =
                RemoteClient::from_streams(client_socket.try_clone().unwrap(), client_socket)
                    .unwrap();
            let (downstream, upstream) = UnixStream::pair().unwrap();
            let proxy = std::thread::spawn(move || {
                serve(upstream.try_clone().unwrap(), upstream, Arc::new(client))
            });
            let client =
                RemoteClient::from_streams(downstream.try_clone().unwrap(), downstream).unwrap();
            let mut query = Request::new(None, Operation::Capabilities {});
            query.request_id = "closing".into();
            if cancelled {
                query.operation = Operation::Cancel {
                    request_id: "closing".into(),
                };
                query.request_id = "cancel".into();
            }
            assert!(client.request(&query).error.is_none());
            let mut request = Request::new(None, Operation::Close {});
            request.request_id = "closing".into();
            let response = client.request(&request);
            assert_eq!(
                response.error.unwrap()["code"],
                if cancelled {
                    "CONTROL.CANCELLED"
                } else {
                    "CONTROL.DUPLICATE_REQUEST"
                }
            );
            assert!(!session.is_closed());
            assert!(client
                .request(&Request::new(None, Operation::Capabilities {}))
                .error
                .is_none());
            assert!(client
                .request(&Request::new(None, Operation::Close {}))
                .error
                .is_none());
            proxy.join().unwrap().unwrap();
            server.join().unwrap().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn eight_remote_requests_are_registered_in_real_session_then_cancelled() {
        use std::os::unix::net::UnixStream;
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let session = Arc::new(Session::new(Vec::new(), Default::default()));
        let endpoint = session.clone();
        let server = std::thread::spawn(move || {
            serve(server_socket.try_clone().unwrap(), server_socket, endpoint).unwrap()
        });
        let client = Arc::new(
            RemoteClient::from_streams(client_socket.try_clone().unwrap(), client_socket).unwrap(),
        );
        let gate = session.gate.lock().unwrap();
        let mut workers = Vec::new();
        for index in 0..8 {
            let client = client.clone();
            workers.push(std::thread::spawn(move || {
                let mut request = Request::new(None, Operation::Capabilities {});
                request.request_id = format!("real-active-{index}");
                client.request(&request)
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if session.active.lock().unwrap().len() == 8 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "eight distinct IDs never admitted"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        for index in 0..8 {
            let response = client.request(&Request::new(
                None,
                Operation::Cancel {
                    request_id: format!("real-active-{index}"),
                },
            ));
            assert_eq!(response.data.unwrap()["cancellation_requested"], true);
        }
        drop(gate);
        for worker in workers {
            let response = worker.join().unwrap();
            assert_eq!(response.error.unwrap()["code"], "CONTROL.CANCELLED");
            assert_eq!(response.commit_state, crate::CommitState::NotCommitted);
        }
        assert!(client
            .request(&Request::new(None, Operation::Close {}))
            .error
            .is_none());
        server.join().unwrap();
    }

    struct Closing {
        session: Session,
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        calls: AtomicUsize,
    }
    impl Endpoint for Closing {
        fn session_id(&self) -> &str {
            self.session.id()
        }
        fn request(&self, request: &Request) -> Response {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if matches!(request.operation, Operation::Close {}) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            self.session.execute(request)
        }
        fn disconnect(&self) {
            self.session.close();
        }
    }
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn close_precedes_later_frames_and_eof_and_flushes_response() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let endpoint = Arc::new(Closing {
            session: Session::new(Vec::new(), Default::default()),
            entered: entered_tx,
            release: Mutex::new(release_rx),
            calls: AtomicUsize::new(0),
        });
        let mut bytes = Vec::new();
        for operation in [Operation::Close {}, Operation::Capabilities {}] {
            let request = Request::new(Some(endpoint.session_id().into()), operation);
            serde_json::to_writer(&mut bytes, &request).unwrap();
            bytes.push(b'\n');
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let (ep, out) = (endpoint.clone(), output.clone());
        let worker =
            std::thread::spawn(move || serve(std::io::Cursor::new(bytes), Output(out), ep));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(endpoint.calls.load(Ordering::SeqCst), 1);
        assert!(output.lock().unwrap().is_empty());
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        let response: Response = serde_json::from_slice(&output.lock().unwrap()).unwrap();
        assert!(response.error.is_none(), "{response:?}");
        assert_eq!(response.data.unwrap()["closed"], true);
        assert_eq!(endpoint.calls.load(Ordering::SeqCst), 1);
    }
}
