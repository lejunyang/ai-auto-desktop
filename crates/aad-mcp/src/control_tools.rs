//! Provider-independent tools. Schemas and dispatch both reject extra fields.
use crate::tools::Tool;
use aad_control::{transport::Endpoint, Operation, Request, Response};
use serde_json::{json, Map, Value};

fn target_schema() -> Value {
    let mut properties = Map::new();
    for field in ["session_id", "resource_id", "observation_id", "node_id"] {
        properties.insert(
            field.into(),
            json!({"type":"string","minLength":1,"maxLength":256}),
        );
    }
    json!({"type":"object","properties":properties,"required":["session_id","resource_id","observation_id","node_id"],"additionalProperties":false})
}
fn action_schema() -> Value {
    let mut variants = Vec::new();
    for name in ["focus", "invoke", "click", "fill", "type_text", "key"] {
        let mut properties = Map::from_iter([("type".into(), json!({"const":name}))]);
        let mut required = vec!["type"];
        match name {
            "fill" => {
                properties.insert("value".into(), json!({"type":"string","maxLength":4096}));
                required.push("value");
            }
            "type_text" => {
                properties.insert("text".into(), json!({"type":"string","maxLength":1024}));
                required.push("text");
            }
            "key" => {
                properties.insert("keys".into(),json!({"type":"array","minItems":1,"maxItems":6,"items":{"type":"string","maxLength":32}}));
                required.push("keys");
            }
            _ => {}
        }
        variants.push(json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}));
    }
    json!({"oneOf":variants})
}
pub fn catalogue() -> Vec<Tool> {
    let definitions=[
        ("control_capabilities","Negotiate this persistent session's native/browser providers and server-side input grants. Unsupported capabilities are never synthesized.",json!({}),vec![],false),
        ("control_targets","List explicit desktop or browser page/frame resources available to this session. Resource IDs are selected by the server, not supplied as OS handles or URLs.",json!({}),vec![],false),
        ("control_observe","Observe one discovered resource. Returns observation-scoped targets; a new observation invalidates previous targets for that provider.",json!({"resource_id":{"type":"string"},"max_nodes":{"type":"integer","minimum":1,"maximum":2000}}),vec!["resource_id"],false),
        ("control_find","Find exactly one node in an existing observation. Ambiguity, truncation and expired observations fail closed.",json!({"observation_id":{"type":"string"},"locator":{"type":"object","properties":{"role":{"type":"string"},"name":{"type":"string"},"node_id":{"type":"string"}},"minProperties":1,"additionalProperties":false}}),vec!["observation_id","locator"],false),
        ("control_act","Perform a negotiated typed action on an observed, freshly revalidated target. Every dispatched attempt consumes that provider's observation. Browser click uses DOM activation; consult resource metadata and node capabilities. Submission is not proof of business success; observe again. Never retry an unknown-effect write automatically.",json!({"target":target_schema(),"action":action_schema()}),vec!["target","action"],true),
        ("control_cancel","Request cancellation of an in-flight control request. If input was already dispatched, its effect is unknown; cancellation never means rollback.",json!({"target_request_id":{"type":"string"}}),vec!["target_request_id"],false),
        ("control_close","Close the control session, invalidate its observations and request cancellation of all in-flight work.",json!({}),vec![],false),
    ];
    definitions.into_iter().map(|(name,description,properties,required,mutating)|{
        let mut properties=properties.as_object().unwrap().clone();
        properties.insert("request_id".into(),json!({"type":"string","minLength":1,"maxLength":128}));
        properties.insert("timeout_ms".into(),json!({"type":"integer","minimum":1,"maximum":30000,"default":10000}));
        Tool {name,description,schema:json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),mutating}
    }).collect()
}

pub fn prepare(name: &str, arguments: &Value, endpoint: &dyn Endpoint) -> Result<Request, String> {
    let mut args = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| "arguments must be an object".to_string())?;
    let mut request = Request::new(
        Some(endpoint.session_id().into()),
        Operation::Capabilities {},
    );
    if let Some(id) = args.remove("request_id") {
        request.request_id = id
            .as_str()
            .ok_or("request_id must be a string")?
            .to_string();
    }
    if let Some(timeout) = args.remove("timeout_ms") {
        request.timeout_ms = timeout.as_u64().ok_or("timeout_ms must be an integer")?;
    }
    let method = name
        .strip_prefix("control_")
        .ok_or("unknown control tool")?;
    if method == "cancel" {
        let id = args
            .remove("target_request_id")
            .ok_or("target_request_id is required")?;
        args.insert("request_id".into(), id);
    }
    args.insert("method".into(), json!(method));
    request.operation = serde_json::from_value(Value::Object(args)).map_err(|e| e.to_string())?;
    Ok(request)
}

pub fn payload(response: Response) -> Value {
    serde_json::to_value(response).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schemas_require_session_observation_and_resource_bound_targets() {
        let tool = catalogue()
            .into_iter()
            .find(|t| t.name == "control_act")
            .unwrap();
        assert!(tool.schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("target")));
        for field in ["session_id", "resource_id", "observation_id", "node_id"] {
            assert!(tool.schema["properties"]["target"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field)));
        }
    }
}
