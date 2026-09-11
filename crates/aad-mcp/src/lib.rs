//! A Model Context Protocol server exposing desktop discovery and automation.
//!
//! The server speaks JSON-RPC 2.0 over stdio, one message per line, which is
//! what MCP clients expect from a local server launched as a subprocess.
//!
//! The [`tools`] module documents the safety model: an agent must observe an
//! element before it can act on it.

pub mod protocol;
pub mod tools;

pub use protocol::Request;

use aad_uia::UiaDriver;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const SERVER_NAME: &str = "ai-auto-desktop";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The MCP revision this server implements.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Handles MCP requests against a driver.
pub struct Server {
    /// Held behind an `Arc` because running a saved workflow hands the driver to
    /// the runtime as a capability provider, which the registry stores by
    /// reference count.
    driver: Option<std::sync::Arc<UiaDriver>>,
    /// Why the driver is missing, if it is.
    unavailable: Option<String>,
}

impl Server {
    /// Build a server on the native driver.
    ///
    /// A missing driver is not a startup failure: the server still answers
    /// `initialize`, `tools/list` and `probe_environment`, so an agent can
    /// discover *why* automation is unavailable instead of seeing a process
    /// that refuses to start.
    pub fn new() -> Self {
        match aad_uia::native_driver() {
            Ok(driver) => Self {
                driver: Some(std::sync::Arc::new(driver)),
                unavailable: None,
            },
            Err(error) => Self {
                driver: None,
                unavailable: Some(error.message),
            },
        }
    }

    pub fn with_driver(driver: UiaDriver) -> Self {
        Self {
            driver: Some(std::sync::Arc::new(driver)),
            unavailable: None,
        }
    }

    /// Handle one request, returning a response unless it was a notification.
    pub fn handle(&self, request: &Request) -> Option<Value> {
        if request.is_notification() {
            return None;
        }
        let id = request.id.clone();

        let outcome = match request.method.as_str() {
            "initialize" => Ok(self.initialize()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools::list_payload()),
            "tools/call" => self.call_tool(&request.params),
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
            "Desktop automation. On Windows, discover applications with list_apps, \
inspect a window with describe_window, resolve an element with find_element, \
then act using the target it returns. Targets prove the element was actually \
observed; if the UI changes, re-observe rather than reusing an old target.\n\n\
For work that has been recorded before, prefer a saved workflow: list_workflows \
shows what exists, describe_workflow reports the inputs it takes without running \
it, and run_workflow runs it. That is more reliable than rebuilding the same \
sequence of clicks each time.",
        );
        if let Some(reason) = &self.unavailable {
            instructions.push_str(&format!(
                "\n\nAutomation is currently unavailable: {reason} \
Call probe_environment for details."
            ));
        }

        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            "instructions": instructions,
        })
    }

    fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((protocol::INVALID_PARAMS, "name is required".to_string()))?;

        if tools::find(name).is_none() && name != "probe_environment" {
            return Err((protocol::INVALID_PARAMS, format!("unknown tool {name:?}")));
        }

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        // Some tools deliberately work without a driver: `probe_environment`
        // exists to explain why the desktop is unavailable, and the workflow
        // tools read the saved store, so listing or inspecting a workflow must
        // not depend on the desktop being reachable.
        const NO_DRIVER_NEEDED: &[&str] = &[
            "probe_environment",
            "list_workflows",
            "describe_workflow",
            "save_workflow",
        ];
        if NO_DRIVER_NEEDED.contains(&name) {
            return Ok(match tools::call_without_driver(name, &arguments) {
                Ok(result) => content(&result, false),
                Err(payload) => content(&parse_payload(&payload), true),
            });
        }
        if name == "run_workflow" {
            return Ok(
                match tools::run_workflow(self.driver.as_ref(), &arguments) {
                    Ok(result) => content(&result, false),
                    Err(payload) => content(&parse_payload(&payload), true),
                },
            );
        }

        let Some(driver) = self.driver.as_ref() else {
            let reason = self.unavailable.as_deref().unwrap_or("no driver");
            return Ok(content(
                &json!({
                    "code": "DRIVER.UNAVAILABLE",
                    "message": reason,
                    "hint": "Call probe_environment to see which prerequisites are missing.",
                }),
                true,
            ));
        };

        // A tool failure is reported as a successful call carrying an error
        // payload: it is a fact about the desktop, not a protocol fault, and an
        // agent should be able to read it and retry.
        match tools::call(driver, name, &arguments) {
            Ok(result) => Ok(content(&result, false)),
            Err(payload) => Ok(content(&parse_payload(&payload), true)),
        }
    }
}

/// Recover the structured error a tool returned, falling back to its text.
fn parse_payload(payload: &str) -> Value {
    serde_json::from_str(payload).unwrap_or_else(|_| json!({"message": payload}))
}

#[cfg(test)]
impl Server {
    /// A server with no driver, as on a machine where automation cannot run.
    fn without_driver(reason: &str) -> Self {
        Self {
            driver: None,
            unavailable: Some(reason.to_string()),
        }
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
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
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), stdout.lock())
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

        for expected in ["list_apps", "describe_window", "find_element"] {
            assert!(instructions.contains(expected), "{instructions}");
        }
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
        assert!(names.contains(&"list_apps"));
        assert!(names.contains(&"invoke"));
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
        // The contrast that makes the test above meaningful: a tool that really
        // does need the desktop must report the missing driver, and point at the
        // probe that explains why.
        let server = Server::without_driver("no UI automation in this session");

        let response = server
            .handle(&request(
                "tools/call",
                json!({"name": "list_apps", "arguments": {}}),
            ))
            .unwrap();
        let (is_error, payload) = tool_payload(&response);

        assert!(is_error);
        assert_eq!(payload["code"], "DRIVER.UNAVAILABLE");
        assert!(payload["message"]
            .as_str()
            .unwrap()
            .contains("no UI automation"));
        assert!(payload["hint"]
            .as_str()
            .unwrap()
            .contains("probe_environment"));
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
        let offline = Server {
            driver: None,
            unavailable: Some("no backend on this platform".to_string()),
        };

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
        let offline = Server {
            driver: None,
            unavailable: Some("UI automation requires Windows".to_string()),
        };

        let response = offline
            .handle(&request(
                "tools/call",
                json!({"name": "list_apps", "arguments": {}}),
            ))
            .unwrap();

        // The call itself succeeds; the payload carries the failure.
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("DRIVER.UNAVAILABLE"));
        assert!(
            text.contains("probe_environment"),
            "must say what to do next"
        );
    }

    #[test]
    fn an_unavailable_driver_is_announced_at_initialize() {
        let offline = Server {
            driver: None,
            unavailable: Some("UI automation requires Windows".to_string()),
        };

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
