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

/// Handles MCP requests against one control session.
pub struct Server {
    control: Arc<dyn Endpoint>,
    /// The local session, when this server owns one. Remote connections have
    /// none, so they cannot reach local files or workflows.
    local: Option<Arc<aad_control::Session>>,
    /// Why no native desktop provider is available, if none is.
    unavailable: Option<String>,
    active_control: Mutex<BTreeMap<String, String>>,
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
            browsers: browser_tools::Browsers::default(),
            runs: durable_tools::Runs::default(),
        }
    }

    /// A local server with input granted over the given Windows driver.
    pub fn with_driver(driver: aad_uia::UiaDriver) -> Self {
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
        if name.starts_with("control_") {
            return true;
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

    fn cancel_rpc(&self, id: &Value) {
        if let Some(request_id) = self
            .active_control
            .lock()
            .unwrap()
            .get(&id.to_string())
            .cloned()
        {
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
        if !self.offers(name) {
            return Err((
                protocol::INVALID_PARAMS,
                "this connection exposes only its granted control session".into(),
            ));
        }

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        if name.starts_with("control_") {
            let request = control_tools::prepare(name, &arguments, self.control.as_ref())
                .map_err(|e| (protocol::INVALID_PARAMS, e))?;
            let key = rpc_id
                .map(Value::to_string)
                .unwrap_or_else(|| request.request_id.clone());
            {
                let mut active = self.active_control.lock().unwrap();
                if active.contains_key(&key) {
                    return Err((
                        protocol::INVALID_REQUEST,
                        "duplicate in-flight JSON-RPC id".into(),
                    ));
                }
                active.insert(key.clone(), request.request_id.clone());
            }
            let response = self.control.request(&request);
            self.active_control.lock().unwrap().remove(&key);
            let is_error = response.error.is_some();
            let mut payload = control_tools::payload(response);
            if name == "control_capture" && !is_error {
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
            || (browser_tools::NAMES.contains(&name) && name != "browser_profiles");
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
            return Ok(match self.browsers.call(session, name, &arguments) {
                Ok(result) => content(&result, false),
                Err(payload) => content(&payload, true),
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

/// Production stdio loop: a cancellation notification can be read while a
/// control action is in flight. Legacy operations share the native action gate.
pub fn serve_concurrent(
    input: impl Read,
    output: impl Write + Send + 'static,
    server: Server,
) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    let server = Arc::new(server);
    let output = Arc::new(Mutex::new(output));
    let active = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let mut input = BufReader::new(input);
    loop {
        if failed.load(Ordering::SeqCst) {
            break;
        }
        let frame = match aad_control::transport::read_frame(&mut input, 8 * 1024 * 1024) {
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
                writeln!(&mut *output.lock().unwrap(), "{response}")?;
                output.lock().unwrap().flush()?;
                continue;
            }
        };
        if request.is_notification() {
            server.handle(&request);
            continue;
        }
        if active.load(Ordering::SeqCst) >= 8 {
            let response = protocol::error_response(
                request.id,
                protocol::INTERNAL_ERROR,
                "too many in-flight MCP requests",
            );
            writeln!(&mut *output.lock().unwrap(), "{response}")?;
            output.lock().unwrap().flush()?;
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let (server, output, active, failed) = (
            server.clone(),
            output.clone(),
            active.clone(),
            failed.clone(),
        );
        std::thread::spawn(move || {
            if let Some(response) = server.handle(&request) {
                let mut output = output.lock().unwrap();
                if writeln!(&mut *output, "{response}")
                    .and_then(|()| output.flush())
                    .is_err()
                {
                    failed.store(true, Ordering::SeqCst);
                    server.disconnect();
                }
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    server.disconnect();
    // EOF ends authority to dispatch input. Allow bounded time to flush already
    // completed read-only replies, then let the CLI reclaim its private workers.
    let deadline = Instant::now() + Duration::from_secs(1);
    while active.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
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

        assert_eq!(tools.len(), tools::catalogue().len());
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
    fn restricted_grants_hide_the_local_workflow_tools() {
        // A workflow finds its own windows by name, so a provider or resource
        // grant could not be enforced on it.
        let restricted = Server::local(aad_control::Session::new(
            Vec::new(),
            aad_control::Policy {
                allow_input: true,
                providers: ["desktop.linux_atspi".to_string()].into(),
                resources: None,
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
