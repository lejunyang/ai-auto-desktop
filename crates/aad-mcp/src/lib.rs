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

pub use protocol::Request;

use aad_control::transport::Endpoint;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
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

/// Handles MCP requests against one control session.
pub struct Server {
    control: Arc<dyn Endpoint>,
    /// The local session, when this server owns one. Remote connections have
    /// none, so they cannot reach local files or workflows.
    local: Option<Arc<aad_control::Session>>,
    /// Why no native desktop provider is available, if none is.
    unavailable: Option<String>,
    /// The calls a `notifications/cancelled` can still reach.
    calls: Mutex<Calls>,
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
            calls: Mutex::default(),
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
            calls: Mutex::default(),
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

    /// Note a call read ahead of the one being answered, so a cancellation
    /// that arrives while it waits is not lost.
    fn queue(&self, id: &Value) {
        let mut calls = self.calls.lock().unwrap();
        calls.waiting.insert(id.to_string(), false);
    }

    /// Start answering a call, unless it was cancelled while it waited. A
    /// control call becomes the current one, which a cancellation reaches
    /// through the session.
    fn start(&self, rpc_id: Option<&Value>, control_request: Option<&str>) -> bool {
        let Some(id) = rpc_id else {
            return true;
        };
        let key = id.to_string();
        let mut calls = self.calls.lock().unwrap();
        if calls.waiting.remove(&key) == Some(true) {
            return false;
        }
        if let Some(request_id) = control_request {
            calls.current = Some((key, request_id.to_string()));
        }
        true
    }

    fn cancel_rpc(&self, id: &Value) {
        let key = id.to_string();
        let request_id = {
            let mut calls = self.calls.lock().unwrap();
            if let Some(cancelled) = calls.waiting.get_mut(&key) {
                *cancelled = true;
                return;
            }
            match &calls.current {
                Some((at, request_id)) if *at == key => request_id.clone(),
                _ => return,
            }
        };
        let request = aad_control::Request::new(
            Some(self.control.session_id().into()),
            aad_control::Operation::Cancel { request_id },
        );
        self.control.request(&request);
    }

    /// The session's own answer to a control call cancelled before admission.
    fn not_started(&self, request: &aad_control::Request) -> aad_control::Response {
        aad_control::Response {
            protocol_version: aad_control::PROTOCOL_VERSION.into(),
            request_id: request.request_id.clone(),
            session_id: self.control.session_id().into(),
            effect: "not_applied".into(),
            commit_state: aad_control::CommitState::NotCommitted,
            data: None,
            error: Some(cancelled_before_start()),
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
        let outcome = self.answer(request);
        if let Some(id) = &id {
            // However the call ended, a later cancellation has nothing to reach.
            self.calls.lock().unwrap().waiting.remove(&id.to_string());
        }
        Some(match outcome {
            Ok(result) => protocol::result_response(id, result),
            Err((code, message)) => protocol::error_response(id, code, &message),
        })
    }

    fn answer(&self, request: &Request) -> Result<Value, (i64, String)> {
        match request.method.as_str() {
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
            other => Err((
                protocol::METHOD_NOT_FOUND,
                format!("unknown method {other:?}"),
            )),
        }
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
            if !self.start(rpc_id, Some(&request.request_id)) {
                return Ok(content(
                    &control_tools::payload(self.not_started(&request)),
                    true,
                ));
            }
            let response = self.control.request(&request);
            self.calls.lock().unwrap().current = None;
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
        if !self.start(rpc_id, None) {
            return Ok(content(&cancelled_before_start(), true));
        }
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
            let result = if matches!(name, "browser_open" | "browser_record_stop") {
                session.browser_request(&id, |context| {
                    self.browsers.call(session, context, name, &arguments)
                })
            } else {
                Ok(self.browsers.call(
                    session,
                    &aad_control::Context::new(std::time::Duration::from_secs(60), false),
                    name,
                    &arguments,
                ))
            };
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

/// What a `notifications/cancelled` can reach.
#[derive(Default)]
struct Calls {
    /// JSON-RPC ids read ahead and not started yet, with whether a
    /// cancellation arrived while they waited.
    waiting: std::collections::HashMap<String, bool>,
    /// The control call being answered, as (JSON-RPC id, control request id).
    current: Option<(String, String)>,
}

fn cancelled_before_start() -> Value {
    aad_control::error("CONTROL.CANCELLED", "cancelled before it started").to_json()
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
        let mut failures = std::collections::BTreeMap::new();
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
pub fn serve(input: impl BufRead + Send + 'static, output: impl Write) -> std::io::Result<()> {
    serve_with(input, output, Server::new())
}

/// Serve MCP over this process's stdio.
pub fn serve_stdio() -> std::io::Result<()> {
    serve_stdio_with(Server::new())
}

pub fn serve_stdio_with(server: Server) -> std::io::Result<()> {
    serve_with(
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
        server,
    )
}

/// Serve MCP over the given streams with this server until end of input.
///
/// One call at a time: each request is answered before the next one starts,
/// so no two calls run at once and replies come back in order. The reader
/// thread reads ahead and handles `notifications/cancelled` itself: one for
/// the control call being answered reaches the session, and a call cancelled
/// while it waits is answered CONTROL.CANCELLED (not_applied) without running.
pub fn serve_with(
    input: impl BufRead + Send + 'static,
    mut output: impl Write,
    server: Server,
) -> std::io::Result<()> {
    let server = Arc::new(server);
    let (lines, queue) = std::sync::mpsc::channel::<std::io::Result<String>>();
    let reader = server.clone();
    std::thread::spawn(move || {
        for line in input.lines() {
            if let Ok(text) = &line {
                if let Ok(request) = protocol::parse(text) {
                    if request.method == "notifications/cancelled" && request.is_notification() {
                        reader.handle(&request);
                        continue;
                    }
                    if let Some(id) = &request.id {
                        reader.queue(id);
                    }
                }
            }
            let failed = line.is_err();
            if lines.send(line).is_err() || failed {
                break;
            }
        }
    });
    let served = serve_lines(queue, &mut output, &server);
    // End of input (or a broken pipe) ends whatever the session still holds.
    server.disconnect();
    served
}

fn serve_lines(
    queue: std::sync::mpsc::Receiver<std::io::Result<String>>,
    output: &mut impl Write,
    server: &Server,
) -> std::io::Result<()> {
    for line in queue {
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

#[cfg(test)]
mod tests {
    use super::*;

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
