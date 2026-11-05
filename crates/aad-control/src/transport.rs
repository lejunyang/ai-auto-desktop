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
}

impl Endpoint for Session {
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

/// Cancellation and close bypass the action worker gate. At most eight request
/// workers and sixteen unread input frames exist. Disconnect cancels all work;
/// no action is retried, and an interrupted write's effect stays unknown.
pub fn serve(
    input: impl Read + Send + 'static,
    output: impl Write + Send + 'static,
    endpoint: Arc<dyn Endpoint>,
) -> io::Result<()> {
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
    let output = Arc::new(Mutex::new(output));
    let active = Arc::new(AtomicUsize::new(0));
    let io_failed = Arc::new(AtomicBool::new(false));
    let mut last_message = Instant::now();
    loop {
        if io_failed.load(Ordering::SeqCst) {
            endpoint.disconnect();
            break;
        }
        let frame = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
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
        if matches!(
            request.operation,
            Operation::Cancel { .. } | Operation::Close {}
        ) {
            let closing = matches!(request.operation, Operation::Close {});
            write_response(&mut *output.lock().unwrap(), &endpoint.request(&request))?;
            if closing {
                break;
            }
            continue;
        }
        if active.load(Ordering::SeqCst) >= MAX_IN_FLIGHT {
            write_response(
                &mut *output.lock().unwrap(),
                &failure(
                    &request,
                    endpoint.session_id(),
                    "CONTROL.BUSY",
                    "too many in-flight requests",
                    false,
                ),
            )?;
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let (endpoint, output, active, io_failed) = (
            endpoint.clone(),
            output.clone(),
            active.clone(),
            io_failed.clone(),
        );
        std::thread::spawn(move || {
            let response = endpoint.request(&request);
            if write_response(&mut *output.lock().unwrap(), &response).is_err() {
                io_failed.store(true, Ordering::SeqCst);
                endpoint.disconnect();
            }
            // Expiry/quota are terminal authority boundaries. Do not keep a
            // closed server process alive solely on client heartbeat requests.
            if terminal(&response) {
                io_failed.store(true, Ordering::SeqCst);
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    endpoint.disconnect();
    // Reader may still be blocked in inherited stdin after an idle timeout.
    // Do not join it; the CLI process exits and reclaims that private pipe.
    drop(reader);
    Ok(())
}

type Pending = Arc<Mutex<BTreeMap<String, mpsc::SyncSender<Response>>>>;

struct Packet {
    bytes: Vec<u8>,
    deadline: Instant,
}

pub struct RemoteClient {
    id: String,
    writer: mpsc::SyncSender<Packet>,
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
        let (writer, queue) = mpsc::sync_channel::<Packet>(MAX_IN_FLIGHT);
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
                if let Some(sender) = waiting.lock().unwrap().remove(&response.request_id) {
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
            if pending.len() >= MAX_IN_FLIGHT {
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
            pending.insert(request.request_id.clone(), tx);
        }
        bytes.push(b'\n');
        let written = self.writer.try_send(Packet {
            bytes,
            deadline: Instant::now() + Duration::from_millis(request.timeout_ms),
        });
        if written.is_err() {
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
            Duration::from_millis(request.timeout_ms.min(crate::MAX_TIMEOUT_MS))
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
                        let _ = self.writer.try_send(Packet {
                            bytes: encoded,
                            deadline: Instant::now() + Duration::from_secs(1),
                        });
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
