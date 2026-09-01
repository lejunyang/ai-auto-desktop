//! The JSON-RPC 2.0 wire format used by MCP.
//!
//! Kept separate from the tool logic so the framing rules — which requests
//! carry a reply, which errors are protocol-level, how a notification differs
//! from a call — can be tested without a desktop.

use serde_json::{json, Value};

pub const JSONRPC_VERSION: &str = "2.0";

// The standard JSON-RPC error codes MCP inherits.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// One decoded incoming message.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Absent for notifications, which must never be answered.
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

impl Request {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// Decode a single JSON-RPC request.
pub fn parse(line: &str) -> Result<Request, Value> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| error_response(None, PARSE_ERROR, &format!("invalid JSON: {error}")))?;

    let object = value
        .as_object()
        .ok_or_else(|| error_response(None, INVALID_REQUEST, "a request must be an object"))?;

    // Recover the id before validating, so a malformed-but-identified request
    // still gets a reply the client can correlate.
    let id = object.get("id").filter(|id| !id.is_null()).cloned();

    if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "jsonrpc must be \"2.0\"",
        ));
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| error_response(id.clone(), INVALID_REQUEST, "method is required"))?;

    Ok(Request {
        id,
        method: method.to_string(),
        params: object.get("params").cloned().unwrap_or(json!({})),
    })
}

pub fn result_response(id: Option<Value>, result: Value) -> Value {
    json!({"jsonrpc": JSONRPC_VERSION, "id": id, "result": result})
}

pub fn error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {"code": code, "message": message},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_call_is_decoded() {
        let request = parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"a":1}}"#)
            .expect("a valid request");

        assert_eq!(request.id, Some(json!(1)));
        assert_eq!(request.method, "tools/list");
        assert_eq!(request.params, json!({"a": 1}));
        assert!(!request.is_notification());
    }

    #[test]
    fn params_default_to_an_empty_object() {
        let request = parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert_eq!(request.params, json!({}));
    }

    #[test]
    fn a_message_without_an_id_is_a_notification() {
        let request = parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(request.is_notification());
    }

    #[test]
    fn a_null_id_is_treated_as_absent() {
        let request = parse(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).unwrap();
        assert!(request.is_notification());
    }

    #[test]
    fn a_string_id_is_preserved_exactly() {
        // Ids must round-trip unchanged or the client cannot correlate replies.
        let request = parse(r#"{"jsonrpc":"2.0","id":"abc-1","method":"ping"}"#).unwrap();
        assert_eq!(request.id, Some(json!("abc-1")));
    }

    #[test]
    fn malformed_json_reports_a_parse_error() {
        let error = parse("{not json").unwrap_err();
        assert_eq!(error["error"]["code"], PARSE_ERROR);
        assert_eq!(error["id"], Value::Null);
    }

    #[test]
    fn a_wrong_protocol_version_is_rejected_but_still_answered() {
        let error = parse(r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#).unwrap_err();

        assert_eq!(error["error"]["code"], INVALID_REQUEST);
        // The id must survive so the client can match the failure to its call.
        assert_eq!(error["id"], json!(7));
    }

    #[test]
    fn a_missing_method_is_rejected() {
        let error = parse(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err();
        assert_eq!(error["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn a_non_object_request_is_rejected() {
        assert!(parse("[1,2,3]").is_err());
        assert!(parse("\"hello\"").is_err());
    }

    #[test]
    fn responses_carry_the_protocol_version_and_id() {
        let ok = result_response(Some(json!(3)), json!({"value": 1}));
        assert_eq!(ok["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(ok["id"], json!(3));
        assert_eq!(ok["result"]["value"], 1);

        let bad = error_response(Some(json!(3)), METHOD_NOT_FOUND, "nope");
        assert_eq!(bad["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(bad["error"]["message"], "nope");
        // A response is either a result or an error, never both.
        assert!(bad.get("result").is_none());
        assert!(ok.get("error").is_none());
    }
}
