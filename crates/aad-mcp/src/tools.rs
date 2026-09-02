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
        Tool {
            name: "list_workflows",
            description: "List the saved workflows that can be run by name. These are \
produced by recording a session in the desktop app.",
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            mutating: false,
        },
        Tool {
            name: "describe_workflow",
            description: "Check a saved workflow and report what it would do: its steps, \
the inputs it expects and the outputs it produces. Does not run anything, so use \
it to understand a workflow before running it.",
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "A name from list_workflows.",
                        "maxLength": 200
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "run_workflow",
            description: "Run a saved workflow and return its outputs. Every step is \
recorded, so a failure reports which step failed and whether it had already taken \
effect. Workflows containing script steps are refused.",
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "A name from list_workflows.",
                        "maxLength": 200
                    },
                    "inputs": {
                        "type": "object",
                        "description": "Values for the workflow's declared inputs, as \
reported by describe_workflow."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            mutating: true,
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

/// Execute a tool that does not need the desktop driver.
///
/// Kept separate from [`call`] so that "which tools work without a desktop" is a
/// single fact stated in one place, rather than something a caller has to infer.
pub fn call_without_driver(name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "probe_environment" => Ok(aad_probe::probe().to_json()),
        "list_workflows" => list_workflows(),
        "describe_workflow" => describe_workflow(arguments),
        other => Err(failure(
            "MCP.DRIVER_REQUIRED",
            &format!("tool {other:?} needs the desktop driver"),
            None,
        )),
    }
}

/// Execute one tool call against a driver.
pub fn call(driver: &UiaDriver, name: &str, arguments: &Value) -> Result<Value, String> {
    // These need no desktop, so they answer even when automation is unavailable.
    if matches!(
        name,
        "probe_environment" | "list_workflows" | "describe_workflow"
    ) {
        return call_without_driver(name, arguments);
    }

    if name == "run_workflow" {
        return run_workflow(arguments);
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

/// Render a structured failure the same way driver errors are rendered, so an
/// agent sees one error shape regardless of which layer refused.
fn failure(code: &str, message: &str, hint: Option<&str>) -> String {
    let mut payload = json!({
        "code": code,
        "message": message,
        "retryable": false,
        "effect": "not_applied",
    });
    if let Some(hint) = hint {
        payload["hint"] = json!(hint);
    }
    serde_json::to_string(&payload).unwrap_or_else(|_| message.to_string())
}

/// The saved workflows, newest first.
fn list_workflows() -> Result<Value, String> {
    let saved = aad_runtime::recordings::list_workflows()
        .map_err(|error| failure(error.code, &error.message, None))?;
    let workflows: Vec<Value> = saved
        .into_iter()
        .map(|entry| json!({"name": entry.name, "modified": entry.modified}))
        .collect();
    Ok(json!({
        "kind": "WorkflowList",
        "count": workflows.len(),
        "workflows": workflows,
        // Saying where they come from turns an empty list from a dead end into
        // something the agent can explain to the person it is helping.
        "directory": aad_runtime::recordings::recordings_dir().to_string_lossy(),
    }))
}

/// Look up a saved workflow by name and compile it.
///
/// Compiling is what makes the name trustworthy: an agent names a workflow, and
/// the strict compiler decides whether it is runnable, so a malformed file is
/// reported rather than half-executed.
fn compiled_workflow(arguments: &Value) -> Result<(String, aad_core::WorkflowDescriptor), String> {
    let name = arguments
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("MCP.INVALID_ARGUMENT", "name is required", None))?;

    let path = aad_runtime::recordings::workflow_path(name)
        .map_err(|error| failure(error.code, &error.message, None))?;
    let document = aad_runtime::recordings::load_recording(&path).map_err(|error| {
        failure(
            error.code,
            &error.message,
            Some("Call list_workflows to see which workflows exist."),
        )
    })?;

    let descriptor = aad_core::compiler::compile_descriptor(document, path.parent().map(Into::into))
        .map_err(|error| {
            failure(
                &error.code,
                &error.message,
                Some("The saved workflow is not valid. Re-record or repair it in the desktop app."),
            )
        })?;
    Ok((name.to_string(), descriptor))
}

/// Report what a workflow would do, without running it.
fn describe_workflow(arguments: &Value) -> Result<Value, String> {
    let (name, descriptor) = compiled_workflow(arguments)?;
    let raw = &descriptor.raw;

    let steps: Vec<Value> = raw["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .map(|step| {
                    json!({
                        "id": step.get("id").cloned().unwrap_or(Value::Null),
                        "type": step.get("type").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "kind": "WorkflowDescription",
        "name": name,
        "planDigest": aad_runtime::plan_digest(&descriptor),
        "inputs": raw.get("inputs").cloned().unwrap_or(json!({})),
        "outputs": raw.get("outputs").cloned().unwrap_or(json!({})),
        "steps": steps,
        "stepCount": steps.len(),
        // Stated up front so an agent learns the limit from a read-only call
        // rather than from a refused run.
        "runnable": !uses_scripts(raw),
    }))
}

/// Whether any step in the document is a script step.
fn uses_scripts(raw: &Value) -> bool {
    fn walk(value: &Value) -> bool {
        match value {
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("script") {
                    return true;
                }
                map.values().any(walk)
            }
            Value::Array(items) => items.iter().any(walk),
            _ => false,
        }
    }
    walk(raw)
}

/// Run a saved workflow and report its result.
fn run_workflow(arguments: &Value) -> Result<Value, String> {
    let (name, descriptor) = compiled_workflow(arguments)?;

    // Refused by name before anything runs. A script step executes arbitrary
    // code, and an agent choosing to run a workflow is not the same as a person
    // deciding to trust that code.
    if uses_scripts(&descriptor.raw) {
        return Err(failure(
            "MCP.SCRIPTS_REFUSED",
            &format!("workflow {name:?} contains a script step, which this server will not run"),
            Some(
                "Scripts execute arbitrary code, so they must be run deliberately \
by a person: use `aad run <file> --allow-scripts`.",
            ),
        ));
    }

    let inputs = match arguments.get("inputs") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Err(failure(
                "MCP.INVALID_ARGUMENT",
                "inputs must be an object",
                None,
            ))
        }
    };

    let result = aad_runtime::run(
        &descriptor,
        aad_runtime::RunOptions::default().with_inputs(inputs),
    );

    let mut payload = json!({
        "kind": "WorkflowRun",
        "name": name,
        "status": result.status.as_str(),
        "outputs": result.outputs,
        "stepsExecuted": result.executed_steps,
    });
    if let Some(error) = &result.error {
        // Carry the effect through verbatim. Whether a failed step already
        // changed the desktop is the one thing the agent cannot re-derive, and
        // guessing it either way would be worse than saying so.
        payload["error"] = json!({
            "code": error.code,
            "message": error.message,
            "effect": error.effect,
            "stepId": error.location.step_id,
        });
    }
    Ok(payload)
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

    /// Point the recordings store at a scratch directory for one test.
    ///
    /// The store is chosen by an environment variable, which is process-wide, so
    /// these tests hold a lock and run one at a time rather than fighting over it.
    struct ScratchStore {
        directory: std::path::PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl ScratchStore {
        fn new(label: &str) -> Self {
            let lock = STORE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
            let directory = std::env::temp_dir().join(format!(
                "aad-mcp-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&directory);
            std::fs::create_dir_all(&directory).expect("scratch store");
            // SAFETY: serialised by STORE_LOCK.
            unsafe { std::env::set_var("AAD_RECORDINGS_DIR", &directory) };
            Self { directory, _lock: lock }
        }

        fn save(&self, name: &str, descriptor: Value) {
            aad_runtime::recordings::save_workflow(name, &descriptor).expect("save");
        }
    }

    impl Drop for ScratchStore {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AAD_RECORDINGS_DIR") };
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    /// A workflow that needs no desktop, so a run can be asserted end to end.
    fn pure_workflow() -> Value {
        json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "mcp.pure"},
            "budgets": {"max_duration": "30s", "max_executed_steps": 20},
            "inputs": {"who": {"schema": {"type": "string"}, "default": "world"}},
            "variables": {
                "greeting": {"schema": {"type": "string"}, "mutable": true, "initial": ""}
            },
            "steps": [{
                "id": "greet", "type": "set",
                "assign": {"vars.greeting": "${{ inputs.who }}"}
            }],
            "outputs": {"greeting": {"value": "${{ vars.greeting }}"}}
        })
    }

    #[test]
    fn saved_workflows_are_listed_by_name() {
        let store = ScratchStore::new("list");
        store.save("checkout", pure_workflow());

        let listed = call_without_driver("list_workflows", &json!({})).expect("list");

        assert_eq!(listed["count"], json!(1));
        assert_eq!(listed["workflows"][0]["name"], json!("checkout"));
        // The directory is reported so an empty list can be explained.
        assert!(listed["directory"].as_str().is_some());
    }

    #[test]
    fn describing_a_workflow_reports_its_inputs_without_running_it() {
        let store = ScratchStore::new("describe");
        store.save("greeter", pure_workflow());

        let described =
            call_without_driver("describe_workflow", &json!({"name": "greeter"})).expect("describe");

        assert_eq!(described["name"], json!("greeter"));
        assert_eq!(described["stepCount"], json!(1));
        assert!(described["inputs"]["who"].is_object(), "declared inputs are reported");
        assert!(described["outputs"]["greeting"].is_object());
        assert!(described["runnable"].as_bool().unwrap());
        assert!(
            described["planDigest"].as_str().unwrap().starts_with("sha256:"),
            "the digest identifies exactly what was inspected"
        );
    }

    #[test]
    fn an_unknown_workflow_name_is_reported_with_a_way_forward() {
        let _store = ScratchStore::new("missing");

        let error = call_without_driver("describe_workflow", &json!({"name": "nope"}))
            .expect_err("a missing workflow is an error");
        let payload: Value = serde_json::from_str(&error).expect("structured error");

        assert_eq!(payload["code"], json!("STORE.NOT_FOUND"));
        assert!(
            payload["hint"].as_str().unwrap().contains("list_workflows"),
            "the agent must be told how to find the real names"
        );
        assert_eq!(payload["effect"], json!("not_applied"));
    }

    #[test]
    fn a_workflow_name_cannot_escape_the_store() {
        let _store = ScratchStore::new("escape");

        // A name is untrusted input that becomes a path. Traversal must be
        // refused by name, not resolved and then read.
        for name in ["../../secrets", "..\\..\\secrets", "a/b", "a\\b"] {
            let error = call_without_driver("describe_workflow", &json!({"name": name}))
                .unwrap_err();
            let payload: Value = serde_json::from_str(&error).unwrap();
            assert_eq!(
                payload["code"], json!("STORE.NAME_INVALID"),
                "{name:?} must be refused as a name"
            );
        }
    }

    #[test]
    fn running_a_saved_workflow_returns_its_outputs() {
        let store = ScratchStore::new("run");
        store.save("greeter", pure_workflow());

        let result = run_workflow(&json!({"name": "greeter", "inputs": {"who": "agent"}}))
            .expect("run");

        assert_eq!(result["status"], json!("succeeded"));
        assert_eq!(result["outputs"]["greeting"], json!("agent"));
        assert_eq!(result["stepsExecuted"], json!(1));
    }

    #[test]
    fn a_workflow_with_a_script_step_is_refused_before_it_runs() {
        let store = ScratchStore::new("scripts");
        let mut workflow = pure_workflow();
        workflow["steps"] = json!([{
            "id": "shell", "type": "script",
            "runtime": "python",
            "output_schema": {"type": "object"},
            "source": "import json; print(json.dumps({}))"
        }]);
        workflow["outputs"] = json!({});
        store.save("dangerous", workflow);

        let error = run_workflow(&json!({"name": "dangerous"})).expect_err("must be refused");
        let payload: Value = serde_json::from_str(&error).expect("structured error");

        assert_eq!(payload["code"], json!("MCP.SCRIPTS_REFUSED"));
        // Nothing ran, so the refusal must not imply a half-done run.
        assert_eq!(payload["effect"], json!("not_applied"));
        assert!(
            payload["hint"].as_str().unwrap().contains("--allow-scripts"),
            "the refusal must say who can run it and how"
        );
    }

    #[test]
    fn describing_a_script_workflow_says_it_is_not_runnable() {
        // The limit should be discoverable from a read-only call rather than
        // only from a refused run.
        let store = ScratchStore::new("script-describe");
        let mut workflow = pure_workflow();
        workflow["steps"] = json!([{
            "id": "shell", "type": "script",
            "runtime": "python",
            "output_schema": {"type": "object"},
            "source": "print('hi')"
        }]);
        workflow["outputs"] = json!({});
        store.save("dangerous", workflow);

        let described =
            call_without_driver("describe_workflow", &json!({"name": "dangerous"})).unwrap();

        assert_eq!(described["runnable"], json!(false));
    }

    #[test]
    fn a_failing_workflow_reports_the_step_and_whether_it_took_effect() {
        let store = ScratchStore::new("failure");
        let mut workflow = pure_workflow();
        // Reading a variable that does not exist is refused, which is what an
        // agent needs to react to.
        workflow["steps"] = json!([{
            "id": "boom", "type": "set",
            "assign": {"vars.greeting": "${{ vars.missing }}"}
        }]);
        store.save("broken", workflow);

        // Either the compiler refuses it or the run fails; both must name the
        // problem rather than returning a bare failure.
        match run_workflow(&json!({"name": "broken"})) {
            Ok(result) => {
                assert_ne!(result["status"], json!("succeeded"));
                let error = &result["error"];
                assert!(error["code"].as_str().is_some(), "a coded failure");
                assert!(
                    error["effect"].as_str().is_some(),
                    "the agent must be told whether the desktop changed"
                );
            }
            Err(payload) => {
                let value: Value = serde_json::from_str(&payload).unwrap();
                assert!(value["code"].as_str().is_some());
            }
        }
    }

    #[test]
    fn run_workflow_rejects_inputs_that_are_not_an_object() {
        let store = ScratchStore::new("bad-inputs");
        store.save("greeter", pure_workflow());

        let error = run_workflow(&json!({"name": "greeter", "inputs": "who=agent"}))
            .expect_err("must be refused");
        let payload: Value = serde_json::from_str(&error).unwrap();

        assert_eq!(payload["code"], json!("MCP.INVALID_ARGUMENT"));
    }

    #[test]
    fn the_workflow_tools_are_annotated_by_what_they_actually_do() {
        let payload = list_payload();
        let tools = payload["tools"].as_array().unwrap();
        let read_only = |name: &str| -> bool {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["annotations"]["readOnlyHint"].as_bool())
                .unwrap_or_else(|| panic!("missing annotation for {name}"))
        };

        assert!(read_only("list_workflows"));
        assert!(read_only("describe_workflow"));
        // Running a workflow drives the desktop, so it must not be advertised
        // as safe to call speculatively.
        assert!(!read_only("run_workflow"));
    }
}
