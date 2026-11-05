//! A Model Context Protocol server exposing desktop discovery and automation.
//!
//! The server speaks JSON-RPC 2.0 over stdio, one message per line, which is
//! what MCP clients expect from a local server launched as a subprocess.
//!
//! There is one way to look at and act on the desktop: the session tools in
//! [`control_tools`]. An agent must observe an element before it can act on
//! it, and input is granted by whoever starts the server, never by the client.
//! Saved workflows run on the same session's providers, gate and input lease.

pub mod browser_tools;
pub mod control_tools;
pub mod durable_tools;
pub mod protocol;
pub mod tools;

#[cfg(test)]
mod cancellation_tests;

pub use protocol::Request;

use aad_control::transport::Endpoint;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::io::{BufReader, Read};
use std::sync::{Arc, Mutex};

pub const SERVER_NAME: &str = "ai-auto-desktop";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The MCP revision this server implements.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Tools that work on the local machine rather than through the session.
const LOCAL_TOOLS: &[&str] = &[
    "probe_environment",
    "list_workflows",
    "describe_workflow",
    "save_workflow",
    "run_workflow",
];

#[derive(Clone)]
enum ControlGeneration {
    Pending { sequence: u64, cancelled: bool },
    Active(String),
}

/// Handles MCP requests against one control session.
pub struct Server {
    control: Arc<dyn Endpoint>,
    /// The local session, when this server owns one. Remote connections have
    /// none, so they cannot reach local files or workflows.
    local: Option<Arc<aad_control::Session>>,
    /// Why no native desktop provider is available, if none is.
    unavailable: Option<String>,
    active_control: Mutex<BTreeMap<String, ControlGeneration>>,
    pending_browser: Mutex<BTreeMap<String, Arc<aad_control::Context>>>,
    /// The browser and recording started through the browser tools.
    browsers: browser_tools::Browsers,
    /// Durable workflow runs this server started or resumed.
    runs: durable_tools::Runs,
}

impl Server {
    /// A local, read-only server. Input needs [`Server::with_options`] with an
    /// explicit grant, as `aad mcp --allow-input` does.
    ///
    /// A missing desktop is not a startup failure: the server still answers
    /// `initialize`, `tools/list` and `probe_environment`, so an agent can
    /// discover *why* automation is unavailable instead of seeing a process
    /// that refuses to start.
    pub fn new() -> Self {
        Self::with_options(aad_control::Policy::default(), None)
    }

    pub fn with_options(policy: aad_control::Policy, browser_port: Option<u16>) -> Self {
        Self::local(aad_control::Session::native_with_browser(
            policy,
            browser_port,
        ))
    }

    /// A local server over an already built session.
    pub fn local(session: aad_control::Session) -> Self {
        let unavailable = session.unavailable().get("desktop").map(|failure| {
            failure["message"]
                .as_str()
                .unwrap_or("no native desktop provider")
                .to_string()
        });
        let session = Arc::new(session);
        Self {
            control: session.clone(),
            local: Some(session),
            unavailable,
            active_control: Mutex::new(BTreeMap::new()),
            pending_browser: Mutex::new(BTreeMap::new()),
            browsers: browser_tools::Browsers::default(),
            runs: durable_tools::Runs::default(),
        }
    }

    /// A local server with input granted over the given Windows driver.
    pub fn with_driver(driver: aad_desktop::UiaDriver) -> Self {
        let driver = Arc::new(driver);
        let backend = aad_control::native::NativeBackend::new(
            driver.clone(),
            aad_control::native::Flavor::Windows,
        );
        Self::local(
            aad_control::Session::new(
                vec![Arc::new(backend)],
                aad_control::Policy {
                    allow_input: true,
                    ..Default::default()
                },
            )
            .with_providers(vec![driver]),
        )
    }

    /// A connection to a session served elsewhere, e.g. over SSH. Its grants
    /// live on the remote server; only the session tools are offered.
    pub fn with_control(control: Arc<dyn Endpoint>) -> Self {
        Self {
            control,
            local: None,
            unavailable: None,
            active_control: Mutex::new(BTreeMap::new()),
            pending_browser: Mutex::new(BTreeMap::new()),
            browsers: browser_tools::Browsers::default(),
            runs: durable_tools::Runs::default(),
        }
    }

    /// Let browser_open copy an existing profile's logins, as
    /// `aad mcp --allow-profile-copy` does.
    pub fn allow_profile_copy(mut self) -> Self {
        self.browsers.allow_profile_copy = true;
        self
    }

    /// Keep durable runs in this store instead of the per-user default.
    pub fn with_run_store(mut self, path: std::path::PathBuf) -> Self {
        self.runs = std::mem::take(&mut self.runs).with_store(path);
        self
    }

    /// Hold durable run leases this long between renewals.
    pub fn with_run_lease_ttl_seconds(mut self, seconds: f64) -> Self {
        self.runs = std::mem::take(&mut self.runs).with_lease_ttl_seconds(seconds);
        self
    }

    pub fn disconnect(&self) {
        for context in self.pending_browser.lock().unwrap().values() {
            context.cancel_browser();
        }
        self.control.disconnect();
        // Durable runs ask to pause at their next step boundary; whatever is
        // still inside a step when the process exits is left for resume.
        self.runs.shutdown();
    }

    /// Local tools are offered only to an unrestricted local session: a
    /// provider or resource grant cannot be enforced on a workflow that finds
    /// its own windows by name.
    fn offers(&self, name: &str) -> bool {
        if control_tools::method(name).is_some() {
            // A remote session holds its own grants and refuses on its side.
            return self
                .local
                .as_ref()
                .is_none_or(|session| control_tools::granted(name, session.policy()));
        }
        (LOCAL_TOOLS.contains(&name)
            || durable_tools::NAMES.contains(&name)
            || browser_tools::NAMES.contains(&name))
            && self.local.as_ref().is_some_and(|session| {
                let policy = session.policy();
                policy.providers.is_empty() && policy.resources.is_none()
            })
    }

    fn allows_input(&self) -> bool {
        self.local
            .as_ref()
            .is_some_and(|session| session.policy().allow_input)
    }

    // The reader and worker change the same generation under one lock. A
    // cancellation cannot disappear between admission and endpoint dispatch.
    fn cancel_control(&self, id: &Value) -> (Option<String>, Option<u64>) {
        match self.active_control.lock().unwrap().get_mut(&id.to_string()) {
            Some(ControlGeneration::Pending {
                sequence,
                cancelled,
            }) => {
                *cancelled = true;
                (None, Some(*sequence))
            }
            Some(ControlGeneration::Active(id)) => (Some(id.clone()), None),
            None => (None, None),
        }
    }

    fn activate_control(&self, key: &str, request_id: &str) -> Result<(), (i64, String)> {
        let mut active = self.active_control.lock().unwrap();
        match active.get(key) {
            Some(ControlGeneration::Pending {
                cancelled: true, ..
            }) => {
                return Err((-32800, "request cancelled before dispatch".into()));
            }
            Some(ControlGeneration::Active(_)) => {
                return Err((
                    protocol::INVALID_REQUEST,
                    "duplicate in-flight JSON-RPC id".into(),
                ));
            }
            _ => {}
        }
        active.insert(key.into(), ControlGeneration::Active(request_id.into()));
        Ok(())
    }

    fn cancel_rpc(&self, id: &Value) {
        let (request_id, _) = self.cancel_control(id);
        if let Some(request_id) = request_id {
            let request = aad_control::Request::new(
                Some(self.control.session_id().into()),
                aad_control::Operation::Cancel { request_id },
            );
            self.control.request(&request);
        }
    }

    /// Handle one request, returning a response unless it was a notification.
    pub fn handle(&self, request: &Request) -> Option<Value> {
        if request.is_notification() {
            if request.method == "notifications/cancelled" {
                if let Some(id) = request.params.get("requestId") {
                    self.cancel_rpc(id);
                }
            }
            return None;
        }
        let id = request.id.clone();

        let outcome = match request.method.as_str() {
            "initialize" => Ok(self.initialize()),
            "ping" => Ok(json!({})),
            "tools/list" => {
                let mut payload = tools::list_payload();
                payload["tools"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|tool| tool["name"].as_str().is_some_and(|name| self.offers(name)));
                Ok(payload)
            }
            "tools/call" => self.call_tool(&request.params, request.id.as_ref()),
            other => {
                return Some(protocol::error_response(
                    id,
                    protocol::METHOD_NOT_FOUND,
                    &format!("unknown method {other:?}"),
                ))
            }
        };

        Some(match outcome {
            Ok(result) => protocol::result_response(id, result),
            Err((code, message)) => protocol::error_response(id, code, &message),
        })
    }

    fn initialize(&self) -> Value {
        let mut instructions = String::from(
            "Look at and act on the desktop through one session: control_capabilities \
says which providers and actions exist and whether input was granted; control_targets \
lists windows, applications and browser pages; control_observe returns an outline \
(or, on a crowded window, an overview of its regions first) with a session-bound \
target for each element; control_find resolves a locator to exactly one of them; \
control_act performs a typed action on such a target. Targets prove the element was \
observed. Observe again after any input, and check the result yourself: submission \
is not success. An input interrupted by cancellation or disconnect has unknown \
effect; never retry it automatically.",
        );
        if self.local.is_some() && self.offers("run_workflow") {
            instructions.push_str(
                "\n\nFor work that has been recorded before, prefer a saved workflow: \
list_workflows shows what exists, describe_workflow reports the inputs it takes without \
running it, and run_workflow runs it. That is more reliable than rebuilding the same \
sequence of actions each time. A long workflow can run with durable: true, which \
journals it so it survives this server exiting; follow it with workflow_run_status and \
workflow_run_events, and stop or continue it with workflow_run_pause, \
workflow_run_cancel and workflow_run_resume.",
            );
        }
        if self.offers("desktop_launch") && self.local.is_some() {
            instructions.push_str(
                "\n\nThe operator allowlisted programs you may start with desktop_launch \
(control_capabilities lists them under `launch`); it answers the new windows as resources.",
            );
        }
        if !self.allows_input() && self.local.is_some() {
            instructions.push_str(
                "\n\nThis server is read-only: control_act, save_workflow and run_workflow \
are refused. Input is granted only by starting the server with --allow-input.",
            );
        }
        if let Some(reason) = &self.unavailable {
            instructions.push_str(&format!(
                "\n\nThe native desktop provider is unavailable: {reason}. Call \
probe_environment for details."
            ));
        }
        if self.local.is_none() {
            instructions.push_str(
                "\n\nThis connection exposes one granted remote session only. Use \
notifications/cancelled to request cancellation of an in-flight JSON-RPC request.",
            );
        }

        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            "instructions": instructions,
        })
    }

    fn call_tool(&self, params: &Value, rpc_id: Option<&Value>) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((protocol::INVALID_PARAMS, "name is required".to_string()))?;

        if tools::find(name).is_none() {
            return Err((protocol::INVALID_PARAMS, format!("unknown tool {name:?}")));
        }
        // A session tool always reaches the session, listed or not: an
        // ungranted one is refused there with POLICY.DENIED, not hidden here.
        if control_tools::method(name).is_none() && !self.offers(name) {
            return Err((
                protocol::INVALID_PARAMS,
                "this connection exposes only its granted control session".into(),
            ));
        }

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        if control_tools::method(name).is_some() {
            let request = control_tools::prepare(name, &arguments, self.control.as_ref())
                .map_err(|e| (protocol::INVALID_PARAMS, e))?;
            let key = rpc_id
                .map(Value::to_string)
                .unwrap_or_else(|| request.request_id.clone());
            self.activate_control(&key, &request.request_id)?;
            #[cfg(feature = "lifecycle-test-hooks")]
            if let Ok(path) = std::env::var("AAD_TEST_CONTROL_DISPATCHED") {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .unwrap();
                writeln!(file, "{}", json!(request.request_id)).unwrap();
            }
            let response = self.control.request(&request);
            self.active_control.lock().unwrap().remove(&key);
            let is_error = response.error.is_some();
            let mut payload = control_tools::payload(response);
            if !is_error
                && (name == "control_capture"
                    || (name == "control_watch_qr" && payload["data"]["data"].is_string()))
            {
                return Ok(image_content(&mut payload));
            }
            return Ok(content(&payload, is_error));
        }

        // Local tools only from here on; `offers` guaranteed a local session.
        let session = self
            .local
            .as_ref()
            .expect("local tools need a local session");
        let writes = matches!(name, "save_workflow" | "run_workflow")
            || durable_tools::MUTATING.contains(&name)
            || (browser_tools::NAMES.contains(&name) && !browser_tools::READ_ONLY.contains(&name));
        if writes && !self.allows_input() {
            return Ok(content(
                &json!({
                    "code": "POLICY.DENIED",
                    "effect": "not_applied",
                    "message": "read-only MCP connection",
                    "hint": "Whoever started this server did not grant input (aad mcp --allow-input).",
                }),
                true,
            ));
        }
        if browser_tools::NAMES.contains(&name) {
            let id =
                aad_control::Request::new(None, aad_control::Operation::Capabilities {}).request_id;
            let key = rpc_id.map(Value::to_string).unwrap_or_else(|| id.clone());
            {
                let mut active = self.active_control.lock().unwrap();
                if active.contains_key(&key) {
                    return Err((
                        protocol::INVALID_REQUEST,
                        "duplicate in-flight JSON-RPC id".into(),
                    ));
                }
                active.insert(key.clone(), ControlGeneration::Active(id.clone()));
            }
            let result = if matches!(name, "browser_open" | "browser_record_stop") {
                let context = self.pending_browser.lock().unwrap().get(&key).cloned();
                if let Some(context) = context {
                    if context.remaining().is_err() {
                        self.active_control.lock().unwrap().remove(&key);
                        return Err((-32800, "request cancelled before dispatch".into()));
                    }
                    Ok(self.browsers.call(session, &context, name, &arguments))
                } else {
                    session.browser_request(&id, |context| {
                        self.browsers.call(session, context, name, &arguments)
                    })
                }
            } else {
                Ok(self.browsers.call(
                    session,
                    &aad_control::Context::new(std::time::Duration::from_secs(60), false),
                    name,
                    &arguments,
                ))
            };
            self.active_control.lock().unwrap().remove(&key);
            return Ok(match result {
                Ok(Ok(result)) => content(&result, false),
                Ok(Err(payload)) => content(&payload, true),
                Err(error) => content(&error.to_json(), true),
            });
        }
        if durable_tools::NAMES.contains(&name) {
            return Ok(match self.runs.call(session, name, &arguments) {
                Ok(result) => content(&result, false),
                Err(payload) => content(&parse_payload(&payload), true),
            });
        }
        if name != "run_workflow" {
            // These need no desktop: `probe_environment` exists to explain why
            // the desktop is unavailable, and the workflow tools read the store.
            return Ok(match tools::call_without_driver(name, &arguments) {
                Ok(result) => content(&result, false),
                Err(payload) => content(&parse_payload(&payload), true),
            });
        }

        if !tools::wants_durable(&arguments) {
            let (payload, is_error) = run_in_memory(session, &arguments);
            return Ok(content(&payload, is_error));
        }
        Ok(match self.runs.start(session, &arguments) {
            Ok(result) => content(&result, false),
            Err(durable_tools::StartError::Failed(payload)) => {
                content(&parse_payload(&payload), true)
            }
            Err(durable_tools::StartError::Refused(refusal)) => {
                // Falling back is the caller's decision, never ours: an
                // in-memory run cannot be followed up or resumed.
                if arguments.get("fallback_in_memory").and_then(Value::as_bool) != Some(true) {
                    return Ok(content(&refusal, true));
                }
                let memory = json!({
                    "name": arguments.get("name").cloned().unwrap_or(Value::Null),
                    "inputs": arguments.get("inputs").cloned().unwrap_or(Value::Null),
                });
                let (mut payload, is_error) = run_in_memory(session, &memory);
                payload["durable"] = json!(false);
                payload["durable_refusal"] = refusal;
                content(&payload, is_error)
            }
        })
    }
}

/// Run a saved workflow in memory, as `run_workflow` without `durable` does.
///
/// A workflow drives the desktop like control_act does: one gate, one input
/// owner, the session's own provider instances.
fn run_in_memory(session: &Arc<aad_control::Session>, arguments: &Value) -> (Value, bool) {
    let through_session = match tools::steps_through_session(arguments) {
        Ok(steps) => steps,
        Err(payload) => return (parse_payload(&payload), true),
    };
    let gate = session.gate();
    let _serialized = (!through_session).then(|| gate.lock().unwrap());
    let _lease = match session.input_lease("desktop-input") {
        Ok(lease) => lease,
        Err(error) => return (error.to_json(), true),
    };
    match tools::run_workflow(session, arguments) {
        Ok(result) => (result, false),
        Err(payload) => (parse_payload(&payload), true),
    }
}

/// Recover the structured error a tool returned, falling back to its text.
fn parse_payload(payload: &str) -> Value {
    serde_json::from_str(payload).unwrap_or_else(|_| json!({"message": payload}))
}

#[cfg(test)]
impl Server {
    /// A server with no native provider, as on a machine where automation cannot run.
    fn without_driver(reason: &str) -> Self {
        let mut failures = BTreeMap::new();
        failures.insert(
            "desktop".to_string(),
            json!({"code": "CONTROL.UNAVAILABLE", "message": reason}),
        );
        let mut server = Self::local(
            aad_control::Session::new(Vec::new(), aad_control::Policy::default())
                .with_unavailable(failures),
        );
        server.unavailable = Some(reason.to_string());
        server
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

/// A screenshot as an MCP image, with what was said about it beside it.
fn image_content(payload: &mut Value) -> Value {
    let data = payload["data"]["data"].take();
    let mime = format!(
        "image/{}",
        payload["data"]["format"].as_str().unwrap_or("png")
    );
    json!({
        "content": [
            {"type": "image", "data": data, "mimeType": mime},
            {"type": "text", "text": serde_json::to_string_pretty(payload).unwrap_or_default()},
        ],
        "isError": false,
    })
}

/// Wrap a value as MCP tool content.
fn content(value: &Value, is_error: bool) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        }],
        "isError": is_error,
    })
}

/// Serve MCP over the given streams until end of input.
pub fn serve(input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    let server = Server::new();
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match protocol::parse(&line) {
            Ok(request) => server.handle(&request),
            Err(error) => Some(error),
        };
        if let Some(response) = response {
            writeln!(output, "{response}")?;
            // Flush per message: an MCP client blocks waiting for the reply.
            output.flush()?;
        }
    }
    Ok(())
}

/// Serve MCP over this process's stdio.
pub fn serve_stdio() -> std::io::Result<()> {
    serve_stdio_with(Server::new())
}

pub fn serve_stdio_with(server: Server) -> std::io::Result<()> {
    serve_concurrent(std::io::stdin(), std::io::stdout(), server)
}

// A reservation lives until the complete response line has been written. Rejected
// requests never own a guard, so they cannot release another request's ID.
struct InFlightId {
    ids: Arc<Mutex<std::collections::HashMap<String, bool>>>,
    key: String,
}

impl InFlightId {
    fn reserve(
        ids: &Arc<Mutex<std::collections::HashMap<String, bool>>>,
        id: &Value,
    ) -> Option<Self> {
        let key = id.to_string();
        let mut entries = ids.lock().unwrap();
        if entries.contains_key(&key) {
            return None;
        }
        entries.insert(key.clone(), false);
        drop(entries);
        Some(Self {
            ids: ids.clone(),
            key,
        })
    }
}

impl InFlightId {
    fn publishing(&self) {
        self.ids.lock().unwrap().insert(self.key.clone(), true);
    }
}

impl Drop for InFlightId {
    fn drop(&mut self) {
        self.ids.lock().unwrap().remove(&self.key);
    }
}

// Only the terminating newline can make a response reusable by a client.
fn publish_response(
    output: &mut impl Write,
    response: &Value,
    reservation: &Option<InFlightId>,
) -> std::io::Result<()> {
    write!(output, "{response}")?;
    // Buffered stdout must publish the body before ID reuse is allowed.
    output.flush()?;
    if let Some(guard) = reservation {
        guard.publishing();
    }
    output.write_all(b"\n")
}

// A blocked writer cannot retain session authority indefinitely. Generic
// Write cannot be forcibly interrupted; the private writer is reclaimed when
// stdio exits, while authority is revoked independently.
fn output_deadline(server: Arc<Server>) -> std::sync::mpsc::Sender<()> {
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if matches!(
            wait.recv_timeout(std::time::Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            server.disconnect();
        }
    });
    done
}

// Exactly one error writer and a bounded backlog: the reader must never
// wait for stdout, including malformed frames and admission failures.
fn error_output(
    output: Arc<Mutex<impl Write + Send + 'static>>,
    server: Arc<Server>,
    failed: Arc<std::sync::atomic::AtomicBool>,
) -> (
    std::sync::mpsc::SyncSender<(Value, Option<InFlightId>)>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<(Value, Option<InFlightId>)>(16);
    let writer = std::thread::spawn(move || {
        for (response, reservation) in rx {
            let deadline = output_deadline(server.clone());
            let mut output = output.lock().unwrap();
            let written = publish_response(&mut *output, &response, &reservation);
            if written.is_err() {
                failed.store(true, std::sync::atomic::Ordering::SeqCst);
                server.disconnect();
            }
            drop(reservation);
            if written.and_then(|()| output.flush()).is_err() {
                failed.store(true, std::sync::atomic::Ordering::SeqCst);
                server.disconnect();
                break;
            }
            drop(deadline);
        }
    });
    (tx, writer)
}

#[cfg(test)]
thread_local! {
    static BEFORE_PENDING_RECEIVE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
    static CLOSE_LANE_REJECTED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}

#[cfg(test)]
thread_local! {
    static BEFORE_REJECTED_OUTPUT: std::cell::RefCell<Option<Box<dyn FnOnce() + Send>>> = const { std::cell::RefCell::new(None) };
}
/// Production stdio loop: a cancellation notification can be read while a
/// control action is in flight. Legacy operations share the native action gate.
pub fn serve_concurrent(
    input: impl Read + Send + 'static,
    output: impl Write + Send + 'static,
    server: Server,
) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    let in_flight = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let server = Arc::new(server);
    let output = Arc::new(Mutex::new(output));
    // Separate bounded lanes: a stalled remote cancellation cannot consume
    // the capacity reserved for closing, or block the input reader.
    let active = Arc::new(AtomicUsize::new(0));
    let cancelling = Arc::new(AtomicUsize::new(0));
    let notified = Arc::new(Mutex::new(std::collections::BTreeSet::new()));
    let closing = Arc::new(AtomicUsize::new(0));
    struct DisconnectOnExit(Arc<Server>);
    impl Drop for DisconnectOnExit {
        fn drop(&mut self) {
            self.0.disconnect();
        }
    }
    let _disconnect = DisconnectOnExit(server.clone());
    let failed = Arc::new(AtomicBool::new(false));
    let (errors, error_writer) = error_output(output.clone(), server.clone(), failed.clone());
    let (frames, incoming) = std::sync::mpsc::sync_channel(16);
    std::thread::spawn(move || {
        let mut input = BufReader::new(input);
        loop {
            let frame = aad_control::transport::read_frame(&mut input, 8 * 1024 * 1024);
            let terminal = !matches!(&frame, Ok(Some(_)));
            if frames.send(frame).is_err() || terminal {
                break;
            }
        }
    });
    let mut pending: std::collections::VecDeque<(Instant, Vec<u8>, String, u64, bool)> =
        Default::default();
    let mut close_received = std::collections::BTreeSet::<u64>::new();
    let (close_completed, close_results) = std::sync::mpsc::channel::<u64>();
    let mut input_sequence = 0u64;
    loop {
        if failed.load(Ordering::SeqCst) {
            break;
        }
        if pending
            .front()
            .is_some_and(|(since, _, _, _, _)| since.elapsed() > Duration::from_secs(1))
        {
            break;
        }
        let ready = pending
            .iter()
            .position(|(_, _, key, _, _)| !in_flight.lock().unwrap().contains_key(key));
        let (next, sequence, cancelled) = if let Some(index) = ready {
            let (_, frame, _, sequence, cancelled) = pending.remove(index).unwrap();
            (Ok(Some(frame)), sequence, cancelled)
        } else {
            #[cfg(test)]
            if !pending.is_empty() {
                BEFORE_PENDING_RECEIVE.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().take() {
                        hook();
                    }
                });
            }
            match incoming.recv_timeout(Duration::from_millis(1)) {
                Ok(value) => {
                    input_sequence += 1;
                    (value, input_sequence, false)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        };
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(e) => {
                server.disconnect();
                return Err(e);
            }
        };
        let request = match protocol::parse(std::str::from_utf8(&frame).unwrap_or("")) {
            Ok(request) => request,
            Err(response) => {
                if errors.try_send((response, None)).is_err() {
                    break;
                }
                continue;
            }
        };
        if request.is_notification() && request.method != "notifications/cancelled" {
            server.handle(&request);
            continue;
        }
        if request.is_notification() {
            if let Some(id) = request.params.get("requestId") {
                let key = id.to_string();
                if let Some(waiting) = pending.iter_mut().find(|entry| entry.2 == key) {
                    waiting.4 = true;
                    close_received.remove(&waiting.3);
                    continue;
                }
            }
            let browser = request.params.get("requestId").and_then(|id| {
                server
                    .pending_browser
                    .lock()
                    .unwrap()
                    .get(&id.to_string())
                    .cloned()
            });
            if let Some(context) = browser {
                // Linearize cancellation against publication before reading another frame.
                context.cancel_browser();
                #[cfg(feature = "lifecycle-test-hooks")]
                if let Ok(path) = std::env::var("AAD_TEST_CANCELLED") {
                    std::fs::write(path, "registered").unwrap();
                }
                continue;
            }
            let (target, cancelled_sequence) = request
                .params
                .get("requestId")
                .map(|id| server.cancel_control(id))
                .unwrap_or_default();
            if let Some(sequence) = cancelled_sequence {
                close_received.remove(&sequence);
                #[cfg(feature = "lifecycle-test-hooks")]
                if let Ok(path) = std::env::var("AAD_TEST_CANCELLED") {
                    std::fs::write(path, "registered").unwrap();
                }
            }
            if let Some(target) = target {
                // One cancellation per actual in-flight operation, bounded by
                // ordinary/control admission. Duplicate notifications coalesce.
                let mut set = notified.lock().unwrap();
                if set.len() >= 10 && !set.contains(&target) {
                    // An unresponsive endpoint exhausted cancellation workers.
                    // Revoke the connection rather than lose a cancellation.
                    drop(set);
                    server.disconnect();
                    break;
                }
                if set.insert(target.clone()) {
                    let server = server.clone();
                    let notified = notified.clone();
                    std::thread::spawn(move || {
                        let cancel = aad_control::Request::new(
                            Some(server.control.session_id().into()),
                            aad_control::Operation::Cancel {
                                request_id: target.clone(),
                            },
                        );
                        if server.control.request(&cancel).error.is_some() {
                            server.disconnect();
                        }
                        #[cfg(feature = "lifecycle-test-hooks")]
                        if let Ok(path) = std::env::var("AAD_TEST_CANCELLED") {
                            std::fs::write(path, &target).unwrap();
                        }
                        notified.lock().unwrap().remove(&target);
                    });
                }
            }
            continue;
        }
        let valid_close = request.method == "tools/call"
            && request.params["name"] == "control_close"
            && control_tools::prepare(
                "control_close",
                request.params.get("arguments").unwrap_or(&json!({})),
                server.control.as_ref(),
            )
            .is_ok();
        let reservation = if let Some(id) = request.id.as_ref() {
            let key = id.to_string();
            // A queued generation owns this ID even after its predecessor's
            // writer releases the active reservation. Resumed frames have
            // already removed their own waiting registration above.
            if pending.iter().any(|entry| entry.2 == key) {
                let response = protocol::error_response(
                    request.id,
                    protocol::INVALID_REQUEST,
                    "duplicate in-flight JSON-RPC id",
                );
                if errors.try_send((response, None)).is_err() {
                    break;
                }
                continue;
            }
            match InFlightId::reserve(&in_flight, id) {
                Some(guard) => Some(guard),
                None => {
                    if in_flight.lock().unwrap().get(&key) != Some(&false) {
                        if pending.len() >= 16 {
                            break;
                        }
                        // Preserve the close's input position even while its ID
                        // waits for the previous response to finish publication.
                        if valid_close {
                            close_received.insert(sequence);
                        }
                        pending.push_back((Instant::now(), frame, id.to_string(), sequence, false));
                        continue;
                    }
                    let response = protocol::error_response(
                        request.id,
                        protocol::INVALID_REQUEST,
                        "duplicate in-flight JSON-RPC id",
                    );
                    if errors.try_send((response, None)).is_err() {
                        break;
                    }
                    continue;
                }
            }
        } else {
            None
        };
        if cancelled {
            let response =
                protocol::error_response(request.id, -32800, "request cancelled before admission");
            if errors.try_send((response, reservation)).is_err() {
                break;
            }
            continue;
        }
        let (lane, limit) = match (
            request.method.as_str(),
            request.params.get("name").and_then(Value::as_str),
        ) {
            ("tools/call", Some("control_close")) => (&closing, 1),
            ("tools/call", Some("control_cancel")) | ("notifications/cancelled", _) => {
                (&cancelling, 1)
            }
            _ => (&active, 8),
        };
        if lane.load(Ordering::SeqCst) >= limit {
            close_received.remove(&sequence);
            #[cfg(test)]
            CLOSE_LANE_REJECTED.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
            if request.is_notification() {
                continue;
            }
            let response = protocol::error_response(
                request.id,
                protocol::INTERNAL_ERROR,
                "too many in-flight MCP requests",
            );
            if errors.try_send((response, reservation)).is_err() {
                break;
            }
            continue;
        }
        for completed in close_results.try_iter() {
            close_received.remove(&completed);
        }
        if close_received
            .first()
            .is_some_and(|boundary| sequence > *boundary)
            && request.method == "tools/call"
            && request.params["name"] != "browser_status"
            && request.params["name"] != "browser_record_stop"
        {
            close_received.remove(&sequence);
            let response =
                protocol::error_response(request.id, protocol::INTERNAL_ERROR, "session closing");
            if errors.try_send((response, reservation)).is_err() {
                break;
            }
            continue;
        }
        if valid_close {
            close_received.insert(sequence);
        }
        let control_key = if request.method == "tools/call"
            && request.params["name"]
                .as_str()
                .is_some_and(|name| control_tools::method(name).is_some())
        {
            let key = request.id.as_ref().expect("non-notification").to_string();
            server.active_control.lock().unwrap().insert(
                key.clone(),
                ControlGeneration::Pending {
                    sequence,
                    cancelled: false,
                },
            );
            Some(key)
        } else {
            None
        };
        let browser_key = if request.method == "tools/call"
            && matches!(
                request.params["name"].as_str(),
                Some("browser_open" | "browser_record_stop")
            )
            && server.local.is_some()
        {
            let key = request.id.as_ref().expect("non-notification").to_string();
            let mut pending = server.pending_browser.lock().unwrap();
            let context = Arc::new(aad_control::Context::new(Duration::from_secs(60), false));
            if let Some(session) = &server.local {
                session.register_browser_context(&context);
            }
            pending.insert(key.clone(), context);
            Some(key)
        } else {
            None
        };
        #[cfg(test)]
        let control_worker_hook = if request.params["name"] == "control_act" {
            cancellation_tests::BEFORE_CONTROL_WORKER.with(|hook| hook.borrow_mut().take())
        } else {
            None
        };
        #[cfg(test)]
        let rejected_output_hook = if valid_close {
            BEFORE_REJECTED_OUTPUT.with(|hook| hook.borrow_mut().take())
        } else {
            None
        };
        lane.fetch_add(1, Ordering::SeqCst);
        let close_completed = close_completed.clone();
        let (server, output, active, failed) =
            (server.clone(), output.clone(), lane.clone(), failed.clone());
        std::thread::spawn(move || {
            #[cfg(test)]
            if let Some(hook) = control_worker_hook {
                hook();
            }
            #[cfg(feature = "lifecycle-test-hooks")]
            if browser_key.is_some() {
                if let Ok(path) = std::env::var("AAD_TEST_OPEN_WORKER_GATE") {
                    let deadline = Instant::now() + Duration::from_secs(20);
                    while !std::path::Path::new(&path).exists() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
            #[cfg(feature = "lifecycle-test-hooks")]
            if let Ok(path) = std::env::var("AAD_TEST_REQUEST_WORKER_GATE") {
                let deadline = Instant::now() + Duration::from_secs(20);
                while request.method == "tools/call"
                    && !std::path::Path::new(&path).exists()
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let response = server.handle(&request);
            let rejected_close = valid_close
                && response
                    .as_ref()
                    .is_some_and(|r| r.get("error").is_some() || r["result"]["isError"] == true);
            if rejected_close {
                let _ = close_completed.send(sequence);
                active.fetch_sub(1, Ordering::SeqCst);
                #[cfg(test)]
                if let Some(hook) = rejected_output_hook {
                    hook();
                }
            }
            // Even validation failures and pre-dispatch cancellations release
            // this generation before its response permits ID reuse.
            if let Some(key) = control_key {
                server.active_control.lock().unwrap().remove(&key);
            }
            if let Some(key) = browser_key {
                server.pending_browser.lock().unwrap().remove(&key);
            }
            let mut reservation = reservation;
            if let Some(response) = response {
                let deadline = output_deadline(server.clone());
                let mut output = output.lock().unwrap();
                let written = publish_response(&mut *output, &response, &reservation);
                if written.is_err() {
                    failed.store(true, Ordering::SeqCst);
                    server.disconnect();
                }
                if !rejected_close {
                    active.fetch_sub(1, Ordering::SeqCst);
                }
                drop(reservation.take());
                if written.and_then(|()| output.flush()).is_err() {
                    failed.store(true, Ordering::SeqCst);
                    server.disconnect();
                }
                drop(deadline);
            } else {
                active.fetch_sub(1, Ordering::SeqCst);
            }
            drop(reservation);
        });
    }
    // EOF terminates admission, including frames waiting for an older ID.
    // Revoke authority before waiting for any already-generated output.
    pending.clear();
    server.disconnect();
    drop(errors);
    // EOF ends authority to dispatch input. Allow bounded time to flush already
    // completed read-only replies, then let the CLI reclaim its private workers.
    let deadline = Instant::now() + Duration::from_secs(1);
    while ((active.load(Ordering::SeqCst)
        + cancelling.load(Ordering::SeqCst)
        + closing.load(Ordering::SeqCst))
        > 0
        || !error_writer.is_finished())
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn eof_discards_publishing_waiters_and_cancels_before_output_unblocks() {
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};
        struct Gate {
            entered: std::sync::mpsc::Sender<()>,
            resume: std::sync::mpsc::Receiver<()>,
            bytes: Arc<Mutex<Vec<u8>>>,
            first: bool,
        }
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.first && bytes.contains(&b'\n') {
                    self.first = false;
                    self.entered.send(()).unwrap();
                    self.resume.recv().unwrap();
                }
                self.bytes.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let server = server();
        let context = Arc::new(aad_control::Context::new(Duration::from_secs(60), false));
        server
            .pending_browser
            .lock()
            .unwrap()
            .insert("other-open".into(), context.clone());
        let (mut client, input) = UnixStream::pair().unwrap();
        let (entered, blocked) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = Gate {
            entered,
            resume,
            bytes: bytes.clone(),
            first: true,
        };
        let worker = std::thread::spawn(move || serve_concurrent(input, output, server));
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"ping\"}\n")
            .unwrap();
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"tools/call\",\"params\":{\"name\":\"control_close\",\"arguments\":{}}}\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let deadline = Instant::now() + Duration::from_millis(500);
        while !context
            .browser_cancellation()
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            assert!(
                Instant::now() < deadline,
                "EOF did not revoke authority before writer deadline"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!bytes.lock().unwrap().ends_with(b"\n"));
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(
            output.lines().count(),
            1,
            "waiting request dispatched after EOF: {output}"
        );
        assert!(serde_json::from_str::<Value>(output.trim())
            .unwrap()
            .get("result")
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn client_reuses_id_after_reading_line_while_flush_is_blocked() {
        use std::os::unix::net::UnixStream;
        use std::time::Duration;
        struct FlushGate {
            line_complete: bool,
            output: UnixStream,
            gate: std::sync::mpsc::Receiver<()>,
            first: bool,
        }
        impl Write for FlushGate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let written = self.output.write(bytes)?;
                self.line_complete |= bytes[..written].contains(&b'\n');
                Ok(written)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                if !std::mem::take(&mut self.line_complete) {
                    return Ok(());
                }
                if self.first {
                    self.first = false;
                    self.gate.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(())
            }
        }
        let (mut client, endpoint) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve_concurrent(
                endpoint.try_clone().unwrap(),
                FlushGate {
                    line_complete: false,
                    output: endpoint,
                    gate,
                    first: true,
                },
                server(),
            )
        });
        let mut replies = BufReader::new(client.try_clone().unwrap());
        let ping = "{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"ping\"}\n";
        client.write_all(ping.as_bytes()).unwrap();
        let mut line = String::new();
        replies.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Value>(&line)
            .unwrap()
            .get("result")
            .is_some());
        client.write_all(ping.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        release.send(()).unwrap();
        line.clear();
        replies.read_line(&mut line).unwrap();
        assert!(
            serde_json::from_str::<Value>(&line)
                .unwrap()
                .get("result")
                .is_some(),
            "{line}"
        );
        client.shutdown(std::net::Shutdown::Write).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn client_reuses_error_id_after_reading_line_while_flush_is_blocked() {
        use std::os::unix::net::UnixStream;
        use std::time::Duration;
        struct FlushGate {
            line_complete: bool,
            output: UnixStream,
            gate: std::sync::mpsc::Receiver<()>,
            skip: usize,
            first: bool,
        }
        impl Write for FlushGate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let written = self.output.write(bytes)?;
                self.line_complete |= bytes[..written].contains(&b'\n');
                Ok(written)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                if !std::mem::take(&mut self.line_complete) {
                    return Ok(());
                }
                if self.skip > 0 {
                    self.skip -= 1;
                    return Ok(());
                }
                if self.first {
                    self.first = false;
                    self.gate.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(())
            }
        }
        let (mut client, endpoint) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve_concurrent(
                endpoint.try_clone().unwrap(),
                FlushGate {
                    line_complete: false,
                    output: endpoint,
                    gate,
                    first: true,
                    skip: 1,
                },
                server(),
            )
        });
        let mut replies = BufReader::new(client.try_clone().unwrap());
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"control_close\",\"arguments\":{}}}\n").unwrap();
        let mut initial = String::new();
        replies.read_line(&mut initial).unwrap();
        let ping = "{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"tools/call\",\"params\":{\"name\":\"control_targets\"}}\n";
        client.write_all(ping.as_bytes()).unwrap();
        let mut line = String::new();
        replies.read_line(&mut line).unwrap();
        assert!(
            serde_json::from_str::<Value>(&line).unwrap()["error"]["message"] == "session closing"
        );
        client.write_all(ping.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        release.send(()).unwrap();
        line.clear();
        replies.read_line(&mut line).unwrap();
        assert!(
            serde_json::from_str::<Value>(&line).unwrap()["error"]["message"] == "session closing",
            "{line}"
        );
        client.shutdown(std::net::Shutdown::Write).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn error_id_is_released_after_complete_line_before_flush_returns() {
        struct FlushGate {
            line_complete: bool,
            readable: std::sync::mpsc::Sender<()>,
            resume: std::sync::mpsc::Receiver<()>,
        }
        impl Write for FlushGate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.line_complete |= bytes.contains(&b'\n');
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                if !std::mem::take(&mut self.line_complete) {
                    return Ok(());
                }
                self.readable.send(()).unwrap();
                self.resume.recv().unwrap();
                Ok(())
            }
        }
        let ids = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let guard = InFlightId::reserve(&ids, &json!(73)).unwrap();
        assert!(InFlightId::reserve(&ids, &json!(73)).is_none());
        let (readable, published) = std::sync::mpsc::channel();
        let (resume, gate) = std::sync::mpsc::channel();
        let (tx, writer) = error_output(
            Arc::new(Mutex::new(FlushGate {
                line_complete: false,
                readable,
                resume: gate,
            })),
            Arc::new(server()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        assert!(tx
            .try_send((
                protocol::error_response(
                    Some(json!(73)),
                    protocol::INTERNAL_ERROR,
                    "too many in-flight MCP requests"
                ),
                Some(guard)
            ))
            .is_ok());
        published
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(InFlightId::reserve(&ids, &json!(73)).is_some());
        resume.send(()).unwrap();
        drop(tx);
        writer.join().unwrap();
        assert!(InFlightId::reserve(&ids, &json!(73)).is_some());
    }

    #[test]
    fn id_stays_reserved_before_first_byte_and_eof_drains_error_output() {
        struct Gate {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
            bytes: Arc<Mutex<Vec<u8>>>,
            first: bool,
        }
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.first {
                    self.first = false;
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                self.bytes.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, seen) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let ids = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let guard = InFlightId::reserve(&ids, &json!(17)).unwrap();
        let (tx, writer) = error_output(
            Arc::new(Mutex::new(Gate {
                entered,
                release: gate,
                bytes: bytes.clone(),
                first: true,
            })),
            Arc::new(server()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        tx.send((json!({"id":17}), Some(guard))).unwrap();
        seen.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(bytes.lock().unwrap().is_empty());
        assert!(InFlightId::reserve(&ids, &json!(17)).is_none());
        release.send(()).unwrap();
        drop(tx);
        writer.join().unwrap();
        assert!(InFlightId::reserve(&ids, &json!(17)).is_some());

        let (entered, seen) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = Gate {
            entered,
            release: gate,
            bytes: bytes.clone(),
            first: true,
        };
        let worker = std::thread::spawn(move || {
            serve_concurrent(
                std::io::Cursor::new(b"invalid-json\n".to_vec()),
                output,
                server(),
            )
            .unwrap()
        });
        seen.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!worker.is_finished(), "EOF discarded pending error");
        release.send(()).unwrap();
        worker.join().unwrap();
        let data = bytes.lock().unwrap();
        assert!(data.ends_with(b"\n"));
        assert!(serde_json::from_slice::<Value>(&data)
            .unwrap()
            .get("error")
            .is_some());
    }

    struct BlockedWriter(std::sync::mpsc::Receiver<()>);
    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.recv().unwrap();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn full_error_queue_disconnects_and_cancels_pending_open() {
        let server = server();
        let context = Arc::new(aad_control::Context::new(
            std::time::Duration::from_secs(60),
            false,
        ));
        server
            .pending_browser
            .lock()
            .unwrap()
            .insert("900".into(), context.clone());
        let (release, blocked) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // More malformed frames than the bounded writer can hold, with
            // no EOF required to make progress or revoke the pending open.
            let input = "invalid\n".repeat(40);
            let result = serve_concurrent(
                std::io::Cursor::new(input.into_bytes()),
                BlockedWriter(blocked),
                server,
            );
            done.send(result.is_ok()).unwrap();
        });
        assert!(finished
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap());
        assert!(context
            .browser_cancellation()
            .load(std::sync::atomic::Ordering::SeqCst));
        for _ in 0..100 {
            let _ = release.send(());
        }
    }

    fn request(method: &str, params: Value) -> Request {
        Request {
            id: Some(json!(1)),
            method: method.to_string(),
            params,
        }
    }

    fn server() -> Server {
        Server::new()
    }

    #[test]
    fn initialize_advertises_the_protocol_and_the_tool_capability() {
        let response = server().handle(&request("initialize", json!({}))).unwrap();
        let result = &response["result"];

        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_explains_the_observe_then_act_workflow() {
        let response = server().handle(&request("initialize", json!({}))).unwrap();
        let instructions = response["result"]["instructions"].as_str().unwrap();

        for expected in [
            "control_targets",
            "control_observe",
            "control_find",
            "control_act",
        ] {
            assert!(instructions.contains(expected), "{instructions}");
        }
        // The default server is read-only, and says how input is granted.
        assert!(instructions.contains("--allow-input"), "{instructions}");
    }

    #[test]
    fn initialize_points_at_saved_workflows_as_the_better_path() {
        // A capability an agent is never told about may as well not exist: it
        // would rebuild the same click sequence from scratch every time.
        let response = server().handle(&request("initialize", json!({}))).unwrap();
        let instructions = response["result"]["instructions"].as_str().unwrap();

        for expected in ["list_workflows", "describe_workflow", "run_workflow"] {
            assert!(
                instructions.contains(expected),
                "{expected} must be discoverable from initialize: {instructions}"
            );
        }
    }

    #[test]
    fn tools_list_returns_the_full_catalogue() {
        let response = server().handle(&request("tools/list", json!({}))).unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();

        // Everything but the tools whose extra grant this server lacks.
        let ungranted = ["desktop_launch", "clipboard_read", "control_global_key"];
        assert_eq!(tools.len(), tools::catalogue().len() - ungranted.len());
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"control_observe"));
        assert!(names.contains(&"run_workflow"));
        // One way to act: the per-action Windows tools are gone.
        for removed in [
            "list_apps",
            "describe_window",
            "find_element",
            "invoke",
            "drag",
        ] {
            assert!(!names.contains(&removed), "{removed} must not be offered");
        }
    }

    /// Read the JSON a tool call returned, plus whether it was flagged an error.
    fn tool_payload(response: &Value) -> (bool, Value) {
        let result = &response["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        let payload = serde_json::from_str(text).unwrap_or_else(|_| json!({"raw": text}));
        (result["isError"].as_bool().unwrap_or(false), payload)
    }

    #[test]
    fn the_workflow_tools_answer_even_when_the_desktop_is_unavailable() {
        // Listing and inspecting saved workflows reads a folder, so it must not
        // be gated behind a working desktop. An agent on a machine where
        // automation is broken should still be able to see what exists and
        // explain the situation, rather than getting DRIVER.UNAVAILABLE for a
        // question that never needed the driver.
        let server = Server::without_driver("no UI automation in this session");

        let response = server
            .handle(&request(
                "tools/call",
                json!({"name": "list_workflows", "arguments": {}}),
            ))
            .unwrap();
        let (is_error, payload) = tool_payload(&response);

        assert!(!is_error, "listing needs no driver: {payload}");
        assert_eq!(payload["kind"], "WorkflowList");
        assert!(payload["directory"].as_str().is_some());
    }

    #[test]
    fn acting_on_the_desktop_without_a_driver_says_so_rather_than_failing_quietly() {
        // A machine with no native provider still answers: listing targets
        // says why the desktop is missing instead of returning a bare empty list.
        let server = Server::without_driver("no UI automation in this session");

        let response = server
            .handle(&request(
                "tools/call",
                json!({"name": "control_targets", "arguments": {}}),
            ))
            .unwrap();
        let (is_error, payload) = tool_payload(&response);

        assert!(!is_error, "{payload}");
        assert_eq!(payload["data"]["resources"], json!([]));
        assert!(payload["data"]["unavailable"]["desktop"]["message"]
            .as_str()
            .unwrap()
            .contains("no UI automation"));
    }

    #[test]
    fn input_and_workflow_writes_are_refused_unless_granted() {
        let server = Server::without_driver("no desktop");
        for (name, arguments) in [
            ("run_workflow", json!({"name": "anything"})),
            ("save_workflow", json!({"name": "anything", "steps": []})),
        ] {
            let response = server
                .handle(&request(
                    "tools/call",
                    json!({"name": name, "arguments": arguments}),
                ))
                .unwrap();
            let (is_error, payload) = tool_payload(&response);
            assert!(is_error, "{name}");
            assert_eq!(payload["code"], "POLICY.DENIED", "{name}");
        }
    }

    #[test]
    fn listing_downloads_is_read_only_and_says_when_no_browser_is_attached() {
        let server = Server::without_driver("no desktop");
        let listed = server.handle(&request("tools/list", json!({}))).unwrap();
        let tool = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "browser_downloads")
            .cloned()
            .expect("offered on a local session");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        let response = server
            .handle(&request(
                "tools/call",
                json!({"name": "browser_downloads", "arguments": {}}),
            ))
            .unwrap();
        let (is_error, payload) = tool_payload(&response);
        assert!(is_error);
        // Not refused as a write on this read-only server: there is just no browser.
        assert_eq!(payload["code"], "BROWSER.NOT_CONNECTED", "{payload}");
    }

    #[test]
    fn restricted_grants_hide_the_local_workflow_tools() {
        // A workflow finds its own windows by name, so a provider or resource
        // grant could not be enforced on it.
        let restricted = Server::local(aad_control::Session::new(
            Vec::new(),
            aad_control::Policy {
                allow_input: true,
                providers: ["desktop.linux_atspi".to_string()].into(),
                ..Default::default()
            },
        ));
        let listed = restricted
            .handle(&request("tools/list", json!({})))
            .unwrap();
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(
            names.iter().all(|name| name.starts_with("control_")),
            "{names:?}"
        );
        let refused = restricted
            .handle(&request(
                "tools/call",
                json!({"name": "run_workflow", "arguments": {"name": "x"}}),
            ))
            .unwrap();
        assert_eq!(refused["error"]["code"], protocol::INVALID_PARAMS);
    }

    fn listed(server: &Server) -> Vec<String> {
        server.handle(&request("tools/list", json!({}))).unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn launching_is_listed_only_when_granted_and_refused_by_policy_otherwise() {
        let plain = Server::without_driver("no desktop");
        assert!(!listed(&plain).contains(&"desktop_launch".to_string()));
        let response = plain
            .handle(&request(
                "tools/call",
                json!({"name": "desktop_launch", "arguments": {"program": "notepad.exe"}}),
            ))
            .unwrap();
        let (is_error, payload) = tool_payload(&response);
        assert!(is_error);
        assert_eq!(payload["error"]["code"], "POLICY.DENIED");
        assert_eq!(payload["commit_state"], "not_committed");

        let granted = Server::local(aad_control::Session::new(
            Vec::new(),
            aad_control::Policy {
                allow_input: true,
                launch: ["notepad.exe".to_string()].into(),
                ..Default::default()
            },
        ));
        assert!(listed(&granted).contains(&"desktop_launch".to_string()));
        let instructions = granted.handle(&request("initialize", json!({}))).unwrap()["result"]
            ["instructions"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(instructions.contains("desktop_launch"), "{instructions}");
        // Asking for a program off the list is refused before anything starts.
        let response = granted
            .handle(&request(
                "tools/call",
                json!({"name": "desktop_launch", "arguments": {"program": "calc.exe"}}),
            ))
            .unwrap();
        let (is_error, payload) = tool_payload(&response);
        assert!(is_error);
        assert_eq!(payload["error"]["code"], "POLICY.DENIED");
        assert_eq!(
            payload["error"]["details"]["allowlist"],
            json!(["notepad.exe"])
        );
    }

    #[test]
    fn global_keys_and_the_clipboard_are_listed_only_with_their_own_grants() {
        let input_only = Server::local(aad_control::Session::new(
            Vec::new(),
            aad_control::Policy {
                allow_input: true,
                ..Default::default()
            },
        ));
        let names = listed(&input_only);
        for hidden in ["control_global_key", "clipboard_read"] {
            assert!(!names.contains(&hidden.to_string()), "{hidden}");
        }
        for (name, arguments) in [
            ("control_global_key", json!({"keys": ["win", "d"]})),
            ("clipboard_read", json!({})),
        ] {
            let response = input_only
                .handle(&request(
                    "tools/call",
                    json!({"name": name, "arguments": arguments}),
                ))
                .unwrap();
            let (is_error, payload) = tool_payload(&response);
            assert!(is_error, "{name}");
            assert_eq!(payload["error"]["code"], "POLICY.DENIED", "{name}");
        }
        let granted = Server::local(aad_control::Session::new(
            Vec::new(),
            aad_control::Policy {
                allow_input: true,
                allow_global_keys: true,
                allow_clipboard_read: true,
                ..Default::default()
            },
        ));
        let names = listed(&granted);
        for shown in ["control_global_key", "clipboard_read"] {
            assert!(names.contains(&shown.to_string()), "{shown}");
        }
    }

    #[test]
    fn a_notification_is_never_answered() {
        let notification = Request {
            id: None,
            method: "notifications/initialized".to_string(),
            params: json!({}),
        };
        assert!(server().handle(&notification).is_none());
    }

    #[test]
    fn an_unknown_method_reports_method_not_found() {
        let response = server()
            .handle(&request("does/not/exist", json!({})))
            .unwrap();

        assert_eq!(response["error"]["code"], protocol::METHOD_NOT_FOUND);
        assert_eq!(response["id"], json!(1));
    }

    #[test]
    fn ping_is_answered() {
        let response = server().handle(&request("ping", json!({}))).unwrap();
        assert!(response.get("result").is_some());
    }

    #[test]
    fn calling_an_unknown_tool_is_a_parameter_error() {
        let response = server()
            .handle(&request("tools/call", json!({"name": "teleport"})))
            .unwrap();

        assert_eq!(response["error"]["code"], protocol::INVALID_PARAMS);
    }

    #[test]
    fn a_tool_call_without_a_name_is_rejected() {
        let response = server().handle(&request("tools/call", json!({}))).unwrap();
        assert_eq!(response["error"]["code"], protocol::INVALID_PARAMS);
    }

    #[test]
    fn probe_environment_answers_even_without_a_driver() {
        let offline = Server::without_driver("no backend on this platform");

        let response = offline
            .handle(&request(
                "tools/call",
                json!({"name": "probe_environment", "arguments": {}}),
            ))
            .unwrap();

        assert_eq!(response["result"]["isError"], false);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("CapabilityProbe"));
    }

    #[test]
    fn an_unavailable_driver_yields_an_actionable_error_not_a_crash() {
        let offline = Server::without_driver("UI automation requires Windows");

        let response = offline
            .handle(&request(
                "tools/call",
                json!({"name": "control_observe", "arguments": {"resource_id": "desktop.windows_uia:x"}}),
            ))
            .unwrap();

        // The call itself succeeds; the payload carries the failure and the way on.
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("CONTROL.TARGET_UNKNOWN"), "{text}");
        assert!(text.contains("control_targets"), "must say what to do next");
    }

    #[test]
    fn an_unavailable_driver_is_announced_at_initialize() {
        let offline = Server::without_driver("UI automation requires Windows");

        let response = offline.handle(&request("initialize", json!({}))).unwrap();
        let instructions = response["result"]["instructions"].as_str().unwrap();

        assert!(instructions.contains("unavailable"), "{instructions}");
    }

    #[test]
    fn a_full_session_round_trips_over_the_stream() {
        let script = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"probe_environment","arguments":{}}}"#,
        ]
        .join("\n");

        let mut output = Vec::new();
        serve(std::io::Cursor::new(script), &mut output).expect("the session completes");

        let text = String::from_utf8(output).unwrap();
        let responses: Vec<Value> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each line is a JSON response"))
            .collect();

        // Three calls, one notification: the notification must not be answered.
        assert_eq!(responses.len(), 3, "got {text}");
        assert_eq!(responses[0]["id"], json!(1));
        assert_eq!(responses[1]["id"], json!(2));
        assert_eq!(responses[2]["id"], json!(3));
    }

    #[test]
    fn malformed_input_is_answered_without_ending_the_session() {
        let script = ["{oops", r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#].join("\n");

        let mut output = Vec::new();
        serve(std::io::Cursor::new(script), &mut output).expect("the session survives");

        let text = String::from_utf8(output).unwrap();
        let responses: Vec<Value> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["error"]["code"], protocol::PARSE_ERROR);
        // The session keeps going and still serves the next request.
        assert!(responses[1].get("result").is_some());
    }

    #[test]
    fn blank_lines_are_ignored() {
        let script = "\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n";

        let mut output = Vec::new();
        serve(std::io::Cursor::new(script), &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.lines().filter(|l| !l.trim().is_empty()).count(), 1);
    }

    #[test]
    fn every_response_is_a_single_line_of_json() {
        // A client reads one message per line; an embedded newline desynchronises it.
        let script = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let mut output = Vec::new();
        serve(std::io::Cursor::new(script), &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.trim_end().lines().count(), 1);
        assert!(serde_json::from_str::<Value>(text.trim_end()).is_ok());
    }
}

#[cfg(all(test, unix))]
mod admission_tests {
    use super::*;

    use std::sync::{mpsc, Condvar};
    use std::time::Duration;

    struct Controlled {
        session: aad_control::Session,
        entered: mpsc::Sender<String>,
        stall_cancel: bool,
        gate: (Mutex<bool>, Condvar),
    }
    impl Endpoint for Controlled {
        fn session_id(&self) -> &str {
            self.session.id()
        }
        fn request(&self, r: &aad_control::Request) -> aad_control::Response {
            if matches!(r.operation, aad_control::Operation::Capabilities {}) {
                return self.session.execute(r);
            }
            self.entered.send(r.request_id.clone()).unwrap();
            if !matches!(r.operation, aad_control::Operation::Close {})
                && (self.stall_cancel || r.request_id != "cancel")
            {
                let (lock, cv) = &self.gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cv.wait(released).unwrap();
                }
            }
            self.session.execute(r)
        }
        fn disconnect(&self) {
            self.session.close();
            let _ = self.entered.send("disconnected".into());
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
        }
    }
    fn send(stream: &mut mpsc::Sender<Vec<u8>>, name: &str, id: &str, extra: Value) {
        let mut args = extra;
        args["request_id"] = json!(id);

        stream
            .send(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":args}})
                )
                .into_bytes(),
            )
            .unwrap();
    }
    struct Input(mpsc::Receiver<Vec<u8>>, std::io::Cursor<Vec<u8>>);
    impl Read for Input {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.1.position() == self.1.get_ref().len() as u64 {
                match self.0.recv() {
                    Ok(bytes) => self.1 = std::io::Cursor::new(bytes),
                    Err(_) => return Ok(0),
                }
            }
            self.1.read(out)
        }
    }

    #[test]
    fn full_workers_and_stalled_cancel_leave_close_capacity() {
        check_capacity(false, false);
    }
    #[test]
    fn remote_full_workers_and_stalled_cancel_leave_close_capacity() {
        check_capacity(true, false);
    }
    #[test]
    fn remote_cancel_notification_does_not_block_close() {
        check_capacity(true, true);
    }
    struct Output(mpsc::Sender<Vec<u8>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .send(bytes.to_vec())
                .map_err(|_| std::io::ErrorKind::BrokenPipe)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn publishing_close_blocks_later_input_before_write_returns() {
        struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, bool);
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                if self.2 && bytes.contains(&b'\n') {
                    self.2 = false;
                    self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, calls) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered,
            stall_cancel: false,
            gate: (Mutex::new(true), Condvar::new()),
        });
        let server = Server::with_control(endpoint);
        let (client, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve_concurrent(
                Input(input, std::io::Cursor::new(Vec::new())),
                Gate(output, gate, true),
                server,
            )
        });
        let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
        client
            .send(b"{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"ping\"}\n".to_vec())
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Value>(&line)
            .unwrap()
            .get("result")
            .is_some());
        for (id, name, arguments) in [
            (73, "control_close", json!({})),
            (
                74,
                "control_act",
                json!({"target":{"session_id":"s","resource_id":"r","observation_id":"o","node_id":"n"},"action":{"type":"click"}}),
            ),
        ] {
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})).into_bytes()).unwrap();
        }
        assert!(calls.recv_timeout(Duration::from_millis(100)).is_err());
        release.send(()).unwrap();
        let mut results = Vec::new();
        for _ in 0..2 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            results.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(
            results.iter().find(|r| r["id"] == 74).unwrap()["error"]["message"],
            "session closing"
        );
        assert!(results
            .iter()
            .find(|r| r["id"] == 73)
            .unwrap()
            .get("result")
            .is_some());
        calls.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(calls.try_recv().is_err());
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn rejected_close_releases_lane_while_its_error_write_is_blocked() {
        struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, usize);
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                if bytes.contains(&b'\n') {
                    self.2 += 1;
                    if self.2 == 1 {
                        self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                    }
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, calls) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered,
            stall_cancel: false,
            gate: (Mutex::new(true), Condvar::new()),
        });
        let (client, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (waiting, waited) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            BEFORE_REJECTED_OUTPUT.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    waiting.send(()).unwrap();
                }))
            });

            serve_concurrent(
                Input(input, std::io::Cursor::new(Vec::new())),
                Gate(output, gate, 0),
                Server::with_control(endpoint),
            )
        });
        let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
        let send = |id, name, request_id| {
            client
                .send(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":name,"arguments":{"request_id":request_id}}})
                    )
                    .into_bytes(),
                )
                .unwrap();
        };
        send(1, "control_targets", "used");
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        calls.recv_timeout(Duration::from_secs(1)).unwrap();
        send(2, "control_close", "used");
        waited.recv_timeout(Duration::from_millis(500)).unwrap();
        calls.recv_timeout(Duration::from_secs(1)).unwrap();
        send(3, "control_close", "fresh");
        assert_eq!(
            calls.recv_timeout(Duration::from_millis(500)).unwrap(),
            "fresh"
        );
        release.send(()).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let first: Value = serde_json::from_str(&line).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let second: Value = serde_json::from_str(&line).unwrap();
        for reply in [first, second] {
            assert!(reply.get("error").is_none());
            assert_eq!(reply["result"]["isError"] == true, reply["id"] == 2);
        }
        for (id, name) in [(4, "control_targets"), (5, "control_close")] {
            send(id, name, "after-close");
            line.clear();
            reader.read_line(&mut line).unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert!(
                reply["result"]["isError"] == true
                    || reply["error"]["message"] == "session closing"
            );
        }
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn rejected_close_restores_admission() {
        struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, bool);
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                if self.2 && bytes.contains(&b'\n') {
                    self.2 = false;
                    self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for arguments in [
            json!({"timeout_ms":0}),
            json!({"request_id":"used"}),
            json!({"request_id":""}),
        ] {
            let (entered, _calls) = mpsc::channel();
            let endpoint = Arc::new(Controlled {
                session: aad_control::Session::new(Vec::new(), Default::default()),
                entered,
                stall_cancel: false,
                gate: (Mutex::new(true), Condvar::new()),
            });
            let (client, input) = mpsc::channel();
            let (output, replies) = mpsc::channel();
            let (release, gate) = mpsc::channel();
            let (waiting, waited) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                BEFORE_PENDING_RECEIVE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        waiting.send(()).unwrap();
                    }));
                });
                serve_concurrent(
                    Input(input, std::io::Cursor::new(Vec::new())),
                    Gate(output, gate, true),
                    Server::with_control(endpoint),
                )
            });
            let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
            for (id, name, args, failed) in [
                (1, "control_targets", json!({"request_id":"used"}), false),
                (1, "control_close", arguments, true),
                (3, "control_targets", json!({}), false),
                (4, "control_close", json!({}), false),
            ] {
                client.send(format!("{}\n",json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})).into_bytes()).unwrap();
                if name == "control_close" && failed {
                    waited.recv_timeout(Duration::from_secs(1)).unwrap();
                    release.send(()).unwrap();
                }
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let reply: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(
                    reply["result"]["isError"] == true || reply.get("error").is_some(),
                    failed,
                    "{reply}"
                );
            }
            drop(client);
            worker.join().unwrap().unwrap();
        }
    }

    #[test]
    fn queued_close_rejected_by_full_lane_releases_boundary() {
        struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, usize);
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                if self.2 > 0 && bytes.contains(&b'\n') {
                    self.2 -= 1;
                    self.1.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, _calls) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered,
            stall_cancel: false,
            gate: (Mutex::new(true), Condvar::new()),
        });
        let (client, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (waiting, waited) = mpsc::channel();
        let (rejected, rejection) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            CLOSE_LANE_REJECTED
                .with(|h| *h.borrow_mut() = Some(Box::new(move || rejected.send(()).unwrap())));
            BEFORE_PENDING_RECEIVE
                .with(|h| *h.borrow_mut() = Some(Box::new(move || waiting.send(()).unwrap())));
            serve_concurrent(
                Input(input, std::io::Cursor::new(Vec::new())),
                Gate(output, gate, 2),
                Server::with_control(endpoint),
            )
        });
        let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
        client
            .send(b"{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"ping\"}\n".to_vec())
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        for (id, args) in [(74, json!({"timeout_ms":0})), (73, json!({}))] {
            client.send(format!("{}\n",json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"control_close","arguments":args}})).into_bytes()).unwrap();
        }
        waited.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let first: Value = serde_json::from_str(&line).unwrap();
        // The invalid close remains in its lane until this response's write returns.
        // Wait for the queued close's admission error to enter the error queue.
        rejection.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let second: Value = serde_json::from_str(&line).unwrap();
        assert!([first, second]
            .iter()
            .any(|r| r["id"] == 73 && r["error"]["message"] == "too many in-flight MCP requests"));
        for (id, name) in [(75, "control_targets"), (76, "control_close")] {
            client.send(format!("{}\n",json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":{}}})).into_bytes()).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert!(
                reply.get("error").is_none() && reply["result"]["isError"] != true,
                "{reply}"
            );
        }
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn parameterized_close_blocks_later_input() {
        struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, bool);
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                if self.2 && bytes.contains(&b'\n') {
                    self.2 = false;
                    self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, calls) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered,
            stall_cancel: false,
            gate: (Mutex::new(true), Condvar::new()),
        });
        let server = Server::with_control(endpoint);
        let (client, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve_concurrent(
                Input(input, std::io::Cursor::new(Vec::new())),
                Gate(output, gate, true),
                server,
            )
        });
        let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
        client
            .send(b"{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"ping\"}\n".to_vec())
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Value>(&line)
            .unwrap()
            .get("result")
            .is_some());
        for (id, name, arguments) in [
            (
                73,
                "control_close",
                json!({"timeout_ms":5000,"request_id":"close-with-options"}),
            ),
            (
                74,
                "control_act",
                json!({"target":{"session_id":"s","resource_id":"r","observation_id":"o","node_id":"n"},"action":{"type":"click"}}),
            ),
        ] {
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})).into_bytes()).unwrap();
        }
        assert!(calls.recv_timeout(Duration::from_millis(100)).is_err());
        release.send(()).unwrap();
        let mut results = Vec::new();
        for _ in 0..2 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            results.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(
            results.iter().find(|r| r["id"] == 74).unwrap()["error"]["message"],
            "session closing"
        );
        assert!(results
            .iter()
            .find(|r| r["id"] == 73)
            .unwrap()
            .get("result")
            .is_some());
        calls.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(calls.try_recv().is_err());
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn publishing_id_accepts_only_one_waiting_generation() {
        struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, bool);
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                if self.2 && bytes.contains(&b'\n') {
                    self.2 = false;
                    self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, calls) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered,
            stall_cancel: false,
            gate: (Mutex::new(true), Condvar::new()),
        });
        let server = Server::with_control(endpoint);
        let (client, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve_concurrent(
                Input(input, std::io::Cursor::new(Vec::new())),
                Gate(output, gate, true),
                server,
            )
        });
        let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
        client
            .send(b"{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"ping\"}\n".to_vec())
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Value>(&line)
            .unwrap()
            .get("result")
            .is_some());
        for _ in 0..2 {
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":"control_targets","arguments":{}}})).into_bytes()).unwrap();
        }
        assert!(calls.recv_timeout(Duration::from_millis(100)).is_err());
        release.send(()).unwrap();
        let mut results = Vec::new();
        for _ in 0..2 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            results.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(
            results
                .iter()
                .filter(|r| r["error"]["message"] == "duplicate in-flight JSON-RPC id")
                .count(),
            1
        );
        assert_eq!(
            results.iter().filter(|r| r.get("result").is_some()).count(),
            1
        );
        calls.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(calls.try_recv().is_err());
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn waiting_id_stays_reserved_across_receive_race() {
        struct Gate(
            mpsc::Sender<Vec<u8>>,
            mpsc::Receiver<()>,
            bool,
            mpsc::Sender<()>,
        );
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                if self.2 && bytes.contains(&b'\n') {
                    self.2 = false;
                    self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                let _ = self.3.send(());
                Ok(())
            }
        }
        let (entered, calls) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered,
            stall_cancel: false,
            gate: (Mutex::new(true), Condvar::new()),
        });
        let server = Server::with_control(endpoint);
        let (client, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (paused_tx, paused_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (flushed_tx, flushed_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            BEFORE_PENDING_RECEIVE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    paused_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                }));
            });
            serve_concurrent(
                Input(input, std::io::Cursor::new(Vec::new())),
                Gate(output, gate, true, flushed_tx),
                server,
            )
        });
        let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
        client
            .send(b"{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"ping\"}\n".to_vec())
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<Value>(&line)
            .unwrap()
            .get("result")
            .is_some());
        for index in 0..2 {
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":"control_targets","arguments":{}}})).into_bytes()).unwrap();
            if index == 0 {
                paused_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            }
        }
        release.send(()).unwrap();
        // flush runs after the old reservation is dropped.
        flushed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        resume_tx.send(()).unwrap();
        let mut results = Vec::new();
        for _ in 0..2 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            results.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(
            results
                .iter()
                .filter(|r| r["error"]["message"] == "duplicate in-flight JSON-RPC id")
                .count(),
            1
        );
        assert_eq!(
            results.iter().filter(|r| r.get("result").is_some()).count(),
            1
        );
        calls.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(calls.try_recv().is_err());
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn duplicate_before_first_byte_is_not_dispatched() {
        struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, bool);
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.2 {
                    self.0.send(Vec::new()).unwrap();
                    self.2 = false;
                    self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                self.0.send(bytes.to_vec()).unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, calls) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered,
            stall_cancel: false,
            gate: (Mutex::new(true), Condvar::new()),
        });
        let server = Server::with_control(endpoint);
        let (client, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve_concurrent(
                Input(input, std::io::Cursor::new(Vec::new())),
                Gate(output, gate, true),
                server,
            )
        });
        let frame = format!("{}\n", json!({"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":"control_targets","arguments":{"request_id":"first"}}})).into_bytes();
        client.send(frame.clone()).unwrap();
        assert_eq!(calls.recv_timeout(Duration::from_secs(1)).unwrap(), "first");
        assert!(replies
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_empty());
        client.send(frame).unwrap();
        client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":74,"method":"tools/call","params":{"name":"control_targets","arguments":{"request_id":"marker"}}})).into_bytes()).unwrap();
        assert_eq!(
            calls.recv_timeout(Duration::from_secs(1)).unwrap(),
            "marker"
        );
        let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
        let mut line = String::new();
        release.send(()).unwrap();
        let mut results = Vec::new();
        for _ in 0..3 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            results.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(
            results
                .iter()
                .filter(|r| r["error"]["message"] == "duplicate in-flight JSON-RPC id")
                .count(),
            1
        );
        assert_eq!(
            results.iter().filter(|r| r.get("result").is_some()).count(),
            2
        );
        assert!(calls.try_recv().is_err());
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn buffered_duplicate_before_first_byte_is_not_dispatched() {
        struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, bool);
        impl Write for Gate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.2 {
                    self.0.send(Vec::new()).unwrap();
                    self.2 = false;
                    self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                self.0.send(bytes.to_vec()).unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, calls) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered,
            stall_cancel: false,
            gate: (Mutex::new(true), Condvar::new()),
        });
        let server = Server::with_control(endpoint);
        let (client, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve_concurrent(
                Input(input, std::io::Cursor::new(Vec::new())),
                std::io::LineWriter::new(Gate(output, gate, true)),
                server,
            )
        });
        let frame = format!("{}\n", json!({"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":"control_targets","arguments":{"request_id":"first"}}})).into_bytes();
        client.send(frame.clone()).unwrap();
        assert_eq!(calls.recv_timeout(Duration::from_secs(1)).unwrap(), "first");
        assert!(replies
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_empty());
        client.send(frame).unwrap();
        client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":74,"method":"tools/call","params":{"name":"control_targets","arguments":{"request_id":"marker"}}})).into_bytes()).unwrap();
        assert_eq!(
            calls.recv_timeout(Duration::from_secs(1)).unwrap(),
            "marker"
        );
        let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
        let mut line = String::new();
        release.send(()).unwrap();
        let mut results = Vec::new();
        for _ in 0..3 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            results.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(
            results
                .iter()
                .filter(|r| r["error"]["message"] == "duplicate in-flight JSON-RPC id")
                .count(),
            1
        );
        assert_eq!(
            results.iter().filter(|r| r.get("result").is_some()).count(),
            2
        );
        assert!(calls.try_recv().is_err());
        drop(client);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn cancelled_publishing_generation_never_reaches_endpoint() {
        for operation in ["control_act", "browser_open", "control_close"] {
            struct Gate(mpsc::Sender<Vec<u8>>, mpsc::Receiver<()>, bool);
            impl Write for Gate {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.0.send(bytes.to_vec()).unwrap();
                    if self.2 && bytes.contains(&b'\n') {
                        self.2 = false;
                        self.1.recv_timeout(Duration::from_secs(2)).unwrap();
                    }
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let (entered, calls) = mpsc::channel();
            let endpoint = Arc::new(Controlled {
                session: aad_control::Session::new(Vec::new(), Default::default()),
                entered,
                stall_cancel: false,
                gate: (Mutex::new(true), Condvar::new()),
            });
            let server = Server::with_control(endpoint);
            let (client, input) = mpsc::channel();
            let (output, replies) = mpsc::channel();
            let (release, gate) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                serve_concurrent(
                    Input(input, std::io::Cursor::new(Vec::new())),
                    Gate(output, gate, true),
                    server,
                )
            });
            let mut reader = BufReader::new(Input(replies, std::io::Cursor::new(Vec::new())));
            client
                .send(b"{\"jsonrpc\":\"2.0\",\"id\":73,\"method\":\"ping\"}\n".to_vec())
                .unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(serde_json::from_str::<Value>(&line)
                .unwrap()
                .get("result")
                .is_some());
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":operation,"arguments":if operation == "control_close" { json!({}) } else { json!({"target":{"session_id":"s","resource_id":"r","observation_id":"o","node_id":"n"},"action":{"type":"click"}}) }}})).into_bytes()).unwrap();
            client.send(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":73}}\n".to_vec()).unwrap();
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":74,"method":"tools/call","params":{"name":"control_targets","arguments":{"request_id":"marker"}}})).into_bytes()).unwrap();
            assert_eq!(
                calls.recv_timeout(Duration::from_secs(1)).unwrap(),
                "marker"
            );
            release.send(()).unwrap();
            let mut results = Vec::new();
            for _ in 0..2 {
                line.clear();
                reader.read_line(&mut line).unwrap();
                results.push(serde_json::from_str::<Value>(&line).unwrap());
            }
            assert_eq!(
                results.iter().find(|r| r["id"] == 73).unwrap()["error"]["code"],
                -32800
            );
            assert!(calls.try_recv().is_err());
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":"control_targets","arguments":{"request_id":"next-generation"}}})).into_bytes()).unwrap();
            assert_eq!(
                calls.recv_timeout(Duration::from_secs(1)).unwrap(),
                "next-generation"
            );
            line.clear();
            reader.read_line(&mut line).unwrap();
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","id":75,"method":"tools/call","params":{"name":"control_close","arguments":{}}})).into_bytes()).unwrap();
            calls.recv_timeout(Duration::from_secs(1)).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert!(serde_json::from_str::<Value>(&line)
                .unwrap()
                .get("result")
                .is_some());
            drop(client);
            worker.join().unwrap().unwrap();
        }
    }

    fn check_capacity(remote: bool, notification: bool) {
        let (tx, rx) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered: tx,
            stall_cancel: true,
            gate: (Mutex::new(false), Condvar::new()),
        });
        let (mut client, rx_input) = mpsc::channel();
        let input = Input(rx_input, std::io::Cursor::new(Vec::new()));
        let control: Arc<dyn Endpoint> = if remote {
            let (request_tx, request_rx) = mpsc::channel();
            let (response_tx, response_rx) = mpsc::channel();
            let receiver = endpoint.clone();
            std::thread::spawn(move || {
                aad_control::transport::serve(
                    Input(request_rx, std::io::Cursor::new(Vec::new())),
                    Output(response_tx),
                    receiver,
                )
            });
            Arc::new(
                aad_control::transport::RemoteClient::from_streams(
                    Input(response_rx, std::io::Cursor::new(Vec::new())),
                    Output(request_tx),
                )
                .unwrap(),
            )
        } else {
            endpoint.clone()
        };
        let server = Server::with_control(control);
        let worker = std::thread::spawn(move || serve_concurrent(input, std::io::sink(), server));
        for n in 0..8 {
            let id = format!("ordinary-{n}");
            send(&mut client, "control_targets", &id, json!({}));
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), id);
        }
        if notification {
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"ordinary-0"}})).into_bytes()).unwrap();
            let id = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(!id.starts_with("ordinary"));
            client.send(format!("{}\n", json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"ordinary-1"}})).into_bytes()).unwrap();
            let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_ne!(id, second);
        } else {
            send(
                &mut client,
                "control_cancel",
                "cancel",
                json!({"target_request_id":"ordinary-0"}),
            );
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "cancel");
        }
        send(&mut client, "control_close", "close", json!({}));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "close");
        endpoint.disconnect();
        drop(client);
        worker.join().unwrap().unwrap();
    }
    struct FailWriter;
    impl Write for FailWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn cancel_response_write_failure_disconnects_inflight_session() {
        let (tx, rx) = mpsc::channel();
        let endpoint = Arc::new(Controlled {
            session: aad_control::Session::new(Vec::new(), Default::default()),
            entered: tx,
            stall_cancel: false,
            gate: (Mutex::new(false), Condvar::new()),
        });
        let (mut client, rx_input) = mpsc::channel();
        let input = Input(rx_input, std::io::Cursor::new(Vec::new()));
        let server = Server::with_control(endpoint.clone());
        let worker = std::thread::spawn(move || serve_concurrent(input, FailWriter, server));
        send(&mut client, "control_targets", "ordinary", json!({}));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "ordinary");
        send(
            &mut client,
            "control_cancel",
            "cancel",
            json!({"target_request_id":"ordinary"}),
        );
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "cancel");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "disconnected"
        );
        assert!(*endpoint.gate.0.lock().unwrap());
        drop(client);
        worker.join().unwrap().unwrap();
    }
}
