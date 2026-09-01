//! The tools this server exposes to an AI agent.
//!
//! The tool set is deliberately shaped around the safe workflow rather than
//! around the driver's internals:
//!
//! 1. `list_apps` — discover what is running;
//! 2. `describe_window` — read a window's interactive elements;
//! 3. `find_element` — resolve a locator to a single addressable target;
//! 4. the action tools — act on a target that was actually observed.
//!
//! Acting requires a target obtained from step 2 or 3, so an agent cannot
//! click at a guessed coordinate or address an element it never saw.

use aad_uia::UiaDriver;
use serde_json::{json, Value};

/// One callable tool.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
    /// Whether calling this changes the state of the desktop.
    pub mutating: bool,
}

fn target_schema() -> Value {
    json!({
        "type": "object",
        "description": "A target obtained from find_element or describe_window. \
Quoting it proves the element was actually observed.",
        "properties": {
            "snapshot_id": {"type": "string"},
            "revision": {"type": "integer"},
            "node_id": {"type": "string"}
        },
        "required": ["snapshot_id", "revision", "node_id"],
        "additionalProperties": false
    })
}

fn locator_schema() -> Value {
    json!({
        "type": "object",
        "description": "Element selector. Every field given must match, so adding \
fields narrows the result. Matching is exact unless `match` is \"contains\".",
        "properties": {
            "role": {"type": "string", "description": "Control type, e.g. Button, Edit."},
            "name": {"type": "string"},
            "value": {"type": "string"},
            "automation_id": {"type": "string"},
            "class_name": {"type": "string"},
            "framework_id": {"type": "string"},
            "states": {
                "type": "object",
                "properties": {
                    "enabled": {"type": "boolean"},
                    "offscreen": {"type": "boolean"},
                    "focusable": {"type": "boolean"},
                    "focused": {"type": "boolean"},
                    "read_only": {"type": "boolean"}
                },
                "additionalProperties": false
            },
            "actions": {
                "type": "array",
                "items": {"enum": ["focus", "invoke", "set_value", "type_text", "pointer_click"]}
            },
            "match": {"enum": ["exact", "contains"], "default": "exact"}
        },
        "minProperties": 1,
        "additionalProperties": false
    })
}

/// Every tool this server publishes.
pub fn catalogue() -> Vec<Tool> {
    vec![
        Tool {
            name: "list_apps",
            description: "List the running applications and their top-level windows. \
Start here: it returns the window_id that every other tool needs.",
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            mutating: false,
        },
        Tool {
            name: "describe_window",
            description: "Describe a window's interactive elements, with a target for \
each one. Use this to see what is on screen before acting.",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_id": {"type": "string", "description": "From list_apps."},
                    "limit": {
                        "type": "integer",
                        "description": "Maximum elements to return (default 80).",
                        "minimum": 1, "maximum": 500
                    }
                },
                "required": ["window_id"],
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "find_element",
            description: "Find the single element matching a locator and return a target \
for it. Fails if the locator matches more than one element, listing the \
candidates so the locator can be narrowed.",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_id": {"type": "string"},
                    "snapshot_id": {
                        "type": "string",
                        "description": "Search an existing snapshot instead of taking a new one."
                    },
                    "locator": locator_schema()
                },
                "required": ["locator"],
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "focus",
            description: "Give keyboard focus to an element.",
            schema: json!({
                "type": "object",
                "properties": {"target": target_schema()},
                "required": ["target"],
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "invoke",
            description: "Activate an element's default action, such as pressing a button. \
Prefer this over pointer_click: it does not depend on window position or z-order.",
            schema: json!({
                "type": "object",
                "properties": {"target": target_schema()},
                "required": ["target"],
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "set_value",
            description: "Replace an element's value directly. Prefer this over type_text \
for text fields: it is atomic and does not depend on focus.",
            schema: json!({
                "type": "object",
                "properties": {
                    "target": target_schema(),
                    "value": {"type": "string"}
                },
                "required": ["target", "value"],
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "type_text",
            description: "Type text as keystrokes into an element, after focusing it. \
Use only when set_value is unsupported, since keystrokes go wherever focus lands.",
            schema: json!({
                "type": "object",
                "properties": {
                    "target": target_schema(),
                    "text": {"type": "string", "maxLength": 1024}
                },
                "required": ["target", "text"],
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "pointer_click",
            description: "Click the centre of an element, raising its window first. \
Use only when the element exposes no invokable pattern.",
            schema: json!({
                "type": "object",
                "properties": {"target": target_schema()},
                "required": ["target"],
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "probe_environment",
            description: "Report whether this machine can support desktop automation, and \
what limits apply, such as a remote session or a non-elevated process.",
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            mutating: false,
        },
    ]
}

/// The `tools/list` payload.
pub fn list_payload() -> Value {
    json!({
        "tools": catalogue()
            .into_iter()
            .map(|tool| json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.schema,
                "annotations": {
                    "readOnlyHint": !tool.mutating,
                    "destructiveHint": tool.mutating,
                },
            }))
            .collect::<Vec<_>>()
    })
}

pub fn find(name: &str) -> Option<Tool> {
    catalogue().into_iter().find(|tool| tool.name == name)
}

/// Execute one tool call against a driver.
pub fn call(driver: &UiaDriver, name: &str, arguments: &Value) -> Result<Value, String> {
    // `probe_environment` is answered without the driver, so it still works
    // when the desktop itself is the thing that is unavailable.
    if name == "probe_environment" {
        return Ok(aad_probe::probe().to_json());
    }

    let action = match name {
        "list_apps" => "list_windows",
        "describe_window" => "describe",
        "find_element" => "find",
        "focus" | "invoke" | "set_value" | "type_text" | "pointer_click" => name,
        other => return Err(format!("unknown tool {other:?}")),
    };

    driver
        .call(action, arguments)
        .map_err(|error| serde_json::to_string(&error_payload(&error)).unwrap_or(error.message))
}

/// Shape a driver error so an agent can act on it.
fn error_payload(error: &aad_uia::DriverError) -> Value {
    let mut payload = json!({
        "code": error.code,
        "message": error.message,
        "retryable": error.retryable,
        "effect": error.effect,
    });
    if !error.details.is_empty() {
        payload["details"] = Value::Object(error.details.clone());
    }
    // Point the agent at the recovery that actually works for this failure.
    let hint = match error.code.as_str() {
        "DRIVER.STALE_HANDLE" => Some(
            "The UI changed since it was observed. Call describe_window or \
find_element again to get a fresh target.",
        ),
        "DRIVER.AMBIGUOUS_MATCH" => Some(
            "The locator matched several elements. Add a field such as \
automation_id or role to narrow it; the candidates are listed in details.",
        ),
        "DRIVER.NOT_FOUND" => Some(
            "No element matched. Call describe_window to see what is actually \
present, or wait if the element may still be appearing.",
        ),
        "DRIVER.ACTION_UNSUPPORTED" => Some(
            "This element does not support that action; details.supported lists \
what it does support.",
        ),
        _ => None,
    };
    if let Some(hint) = hint {
        payload["hint"] = json!(hint);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_publishes_a_usable_schema() {
        for tool in catalogue() {
            assert!(!tool.name.is_empty());
            assert!(
                tool.description.len() > 20,
                "{} needs a description an agent can act on",
                tool.name
            );
            assert_eq!(tool.schema["type"], "object", "{}", tool.name);
            // A permissive schema lets an agent send anything and get a
            // confusing failure later; every tool must be strict.
            assert_eq!(
                tool.schema["additionalProperties"],
                json!(false),
                "{} must reject unknown arguments",
                tool.name
            );
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = catalogue().iter().map(|tool| tool.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn discovery_tools_are_marked_read_only_and_actions_are_not() {
        let payload = list_payload();
        let tools = payload["tools"].as_array().unwrap();

        let annotation = |name: &str, key: &str| -> bool {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["annotations"][key].as_bool())
                .unwrap_or_else(|| panic!("missing {key} for {name}"))
        };

        for name in ["list_apps", "describe_window", "find_element", "probe_environment"] {
            assert!(annotation(name, "readOnlyHint"), "{name} only reads");
        }
        for name in ["invoke", "type_text", "set_value", "pointer_click", "focus"] {
            assert!(!annotation(name, "readOnlyHint"), "{name} changes the desktop");
            assert!(annotation(name, "destructiveHint"), "{name} must be flagged");
        }
    }

    #[test]
    fn every_action_tool_requires_an_observed_target() {
        // This is the invariant that stops an agent acting on a guess.
        for name in ["focus", "invoke", "set_value", "type_text", "pointer_click"] {
            let tool = find(name).expect(name);
            let required = tool.schema["required"].as_array().expect(name);
            assert!(
                required.contains(&json!("target")),
                "{name} must require a target"
            );
            let target = &tool.schema["properties"]["target"];
            for field in ["snapshot_id", "revision", "node_id"] {
                assert!(
                    target["required"].as_array().unwrap().contains(&json!(field)),
                    "{name} target must require {field}"
                );
            }
        }
    }

    #[test]
    fn no_tool_accepts_raw_screen_coordinates() {
        // Clicking a coordinate an agent invented is precisely the failure mode
        // the snapshot discipline exists to prevent.
        for tool in catalogue() {
            let properties = tool.schema["properties"].as_object().unwrap();
            for forbidden in ["x", "y", "point", "coordinates", "position"] {
                assert!(
                    !properties.contains_key(forbidden),
                    "{} must not accept {forbidden}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn text_input_is_bounded_in_the_schema() {
        let tool = find("type_text").unwrap();
        assert_eq!(tool.schema["properties"]["text"]["maxLength"], json!(1024));
    }

    #[test]
    fn an_unknown_tool_is_reported_rather_than_guessed() {
        let error = find("teleport");
        assert!(error.is_none());
    }

    #[test]
    fn the_list_payload_matches_the_catalogue() {
        let payload = list_payload();
        assert_eq!(
            payload["tools"].as_array().unwrap().len(),
            catalogue().len()
        );
    }

    #[test]
    fn a_stale_handle_error_tells_the_agent_how_to_recover() {
        let error = aad_uia::DriverError::stale("out of date");
        let payload = error_payload(&error);

        assert_eq!(payload["code"], "DRIVER.STALE_HANDLE");
        let hint = payload["hint"].as_str().expect("a recovery hint");
        assert!(hint.contains("find_element") || hint.contains("describe_window"));
    }

    #[test]
    fn an_ambiguous_match_error_explains_how_to_narrow_it() {
        let error = aad_uia::DriverError::new("DRIVER.AMBIGUOUS_MATCH", "two matches");
        let payload = error_payload(&error);

        assert!(payload["hint"].as_str().unwrap().contains("narrow"));
    }

    #[test]
    fn probing_the_environment_needs_no_desktop() {
        // The tool must answer even where automation itself cannot run.
        let driver = aad_uia::native_driver();
        if let Ok(driver) = driver {
            let report = call(&driver, "probe_environment", &json!({})).unwrap();
            assert_eq!(report["kind"], "CapabilityProbe");
        } else {
            let report = aad_probe::probe().to_json();
            assert_eq!(report["kind"], "CapabilityProbe");
        }
    }
}
