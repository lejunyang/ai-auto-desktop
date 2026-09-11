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
use std::sync::Arc;

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
    fields narrows the result. Matching is exact unless `match` is \"contains\". \
    When several elements share every attribute -- rows of a table each with their own \
    Edit button, for instance -- narrow with `within`, `near` or `nth` rather than \
    guessing.",
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
                    "read_only": {"type": "boolean"},
                    "protected": {
                        "type": "boolean",
                        "description": "True for password fields. Their value cannot be read back, so this is how to identify one."
                    }
                },
                "additionalProperties": false
            },
            "actions": {
                "type": "array",
                "items": {"enum": ["focus", "invoke", "set_value", "type_text", "pointer_click"]}
            },
            "match": {"enum": ["exact", "contains"], "default": "exact"},
            "within": {
                "type": "object",
                "description": "Only match inside the element this describes -- \
    itself a locator, so it can name a container by its own text. Prefer this over \
    `nth` when the container has a name: the row labelled \"Order for Ada\" is still \
    that row after the list is reordered, whereas a position is not.",
                "properties": {},
                "additionalProperties": true
            },
            "near": {
                "type": "object",
                "description": "Only match elements beside another one, for \
    \"the field next to the Password label\".",
                "properties": {
                    "anchor": {
                        "type": "object",
                        "description": "A locator for the element to measure from.",
                        "additionalProperties": true
                    },
                    "direction": {
                        "enum": ["any", "left", "right", "above", "below"],
                        "default": "any",
                        "description": "Where the wanted element sits relative to \
    the anchor. A direction also requires them to share a row or a column."
                    },
                    "within": {
                        "type": "integer",
                        "description": "Maximum gap in pixels between their edges."
                    }
                },
                "required": ["anchor"],
                "additionalProperties": false
            },
            "nth": {
                "description": "Which one to take when several still match, \
    counting from 1 in reading order (top to bottom, then left to right), or \
    \"last\". A last resort: position shifts whenever the interface reflows, and in \
    a browser most of what the platform reports as buttons belongs to the browser \
    itself rather than the page, so counting rarely means what it appears to.",
                "oneOf": [
                    {"type": "integer", "minimum": 1},
                    {"const": "last"}
                ]
            }
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
            name: "overview_window",
            description: "Map a window's regions and their sizes without listing \
their contents. Start here on any window of substance: measured across this \
desktop, nine windows out of twenty hold more interactive elements than \
describe_window will return, and the largest holds 367. An overview of a whole \
window costs about 2000 characters, where listing 500 elements costs 127000. Each \
region is named as the interface names it -- a file tree, a toolbar, a form panel, \
a row of a table -- and describe_window takes that name to read one.",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_id": {"type": "string", "description": "From list_apps."}
                },
                "required": ["window_id"],
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "describe_window",
            description: "Describe a window's interactive elements, with a target for \
each one. Reading a whole window is only practical when it is small: this returns \
the first `limit` elements and reports `truncated`, so on a crowded window it \
shows the top of the tree and never reaches the rest. Call overview_window first \
and pass one of its regions as `region`.",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_id": {"type": "string", "description": "From list_apps."},
                    "region": {
                        "type": "string",
                        "description": "List only this region, named as \
            overview_window reports it. This is the same name a locator carries in `within`, \
            so a region drilled into and a locator written against it agree."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum elements to return (default 80). \
            Check `truncated` and `matched` in the answer: `matched` says how many were \
            eligible, so it tells you whether raising this would show more.",
                        "minimum": 1, "maximum": 500
                    },
                    "max_characters": {
                        "type": "integer",
                        "description": "Stop after roughly this many characters \
            (default 20000). An element count does not bound the answer's size -- elements \
            run from 169 to 4890 characters here, and 500 of them reached 188147 on one \
            window. When the answer is truncated, `stopped_by` says which ceiling bit: \
            raising `limit` helps when it says `limit` and does nothing when it says \
            `characters`, where narrowing to a region is the way forward.",
                        "minimum": 500, "maximum": 200000
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
candidates so the locator can be narrowed. To check whether something has gone \
away, pass expect=optional and read `found` instead of treating the miss as an \
error.",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_id": {"type": "string"},
                    "snapshot_id": {
                        "type": "string",
                        "description": "Search an existing snapshot instead of taking a new one."
                    },
                    "locator": locator_schema(),
                    "expect": {
                        "type": "string",
                        "enum": ["one", "optional"],
                        "description": "one (default): a miss is an error, which is what \
            you want when acquiring something to act on. optional: a miss is an ordinary \
            result with found=false, for asking whether a dialog has closed or a spinner \
            has gone. Neither one lets an ambiguous match through."
                    }
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
            name: "save_workflow",
            description: "Turn actions you have already performed into a named \
workflow that run_workflow can replay. Give the steps in the order they were done, \
each with the locator you used to find the element -- describe_window and \
find_element both return one. The saved workflow re-finds every element as it \
runs, so it keeps working after the target application restarts, which a target \
reference does not. Steps are checked before anything is written: a workflow that \
saves is a workflow that runs.",
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "What to call it. run_workflow and \
            describe_workflow take this name."
                    },
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "description": "The actions in the order they were performed.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "action": {
                                    "type": "string",
                                    "enum": [
                                        "focus", "invoke", "set_value",
                                        "type_text", "pointer_click"
                                    ],
                                    "description": "What was done to the element."
                                },
                                "locator": {
                                    "type": "object",
                                    "description": "How to find the element again. \
            Copy the `locator` describe_window or find_element gave for it. An element whose \
            locator came back null cannot be told apart from its siblings and cannot be saved."
                                },
                                "window": {
                                    "type": "object",
                                    "description": "Which window, by \
            `process_name` and a stable part of `title`. Not a window_id: ids are assigned per \
            session, so a workflow using one works once and then fails.",
                                    "properties": {
                                        "process_name": {"type": "string"},
                                        "title": {
                                            "type": "string",
                                            "description": "A part of the title that \
            does not change as the application is used -- many titles carry the open document \
            or a status that varies."
                                        }
                                    },
                                    "required": ["process_name"],
                                    "additionalProperties": false
                                },
                                "argument": {
                                    "type": "string",
                                    "description": "The text written or typed. Only \
            for set_value and type_text; giving it for another action is refused rather than \
            ignored."
                                },
                                "protected": {
                                    "type": "boolean",
                                    "description": "True if the text was a \
            credential. It is then stored as a required input instead of being written into \
            the file, and whoever runs the workflow supplies it."
                                }
                            },
                            "required": ["action", "locator", "window"],
                            "additionalProperties": false
                        }
                    },
                    "overwrite": {
                        "type": "boolean",
                        "description": "Replace an existing workflow of the same \
            name. Without this a name already in use is refused, so two different workflows \
            cannot silently collapse into one."
                    }
                },
                "required": ["name", "steps"],
                "additionalProperties": false
            }),
            mutating: true,
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
            description: "Report what a saved workflow would do, without running \
it: each action, the element it finds, the window it acts in, and the text it \
writes. Any input it needs is named rather than its value shown, so a workflow \
carrying a credential says what to supply without revealing it. Read this before \
run_workflow when it matters what gets touched.",
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
pub fn call(driver: &Arc<UiaDriver>, name: &str, arguments: &Value) -> Result<Value, String> {
    // These need no desktop, so they answer even when automation is unavailable.
    if matches!(
        name,
        "probe_environment" | "list_workflows" | "describe_workflow"
    ) {
        return call_without_driver(name, arguments);
    }

    if name == "run_workflow" {
        return run_workflow(driver, arguments);
    }

    // Saving needs no desktop: the steps describe elements rather than reaching
    // for them, which is what lets a saved workflow outlive the session.
    if name == "save_workflow" {
        return save_workflow(arguments);
    }

    let action = match name {
        "list_apps" => "list_windows",
        "overview_window" => "overview",
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

/// Assemble performed actions into a named workflow and save it.
///
/// Compiled before it is written. Saving something the engine would reject leaves
/// an agent believing it has a reusable workflow until the moment it tries to run
/// it, and the reason is harder to read from a run failure than from here.
fn save_workflow(arguments: &Value) -> Result<Value, String> {
    let name = arguments
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("MCP.INVALID_ARGUMENT", "name is required", None))?;
    let given = arguments
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("MCP.INVALID_ARGUMENT", "steps is required", None))?;

    let mut performed = Vec::new();
    for (index, step) in given.iter().enumerate() {
        let ordinal = index + 1;
        let action = step.get("action").and_then(Value::as_str).ok_or_else(|| {
            failure(
                "MCP.INVALID_ARGUMENT",
                &format!("step {ordinal} needs an action"),
                None,
            )
        })?;
        performed.push(aad_runtime::assemble::PerformedStep {
            action: action.to_string(),
            locator: step.get("locator").cloned().unwrap_or(Value::Null),
            window: step.get("window").cloned().unwrap_or(Value::Null),
            argument: step
                .get("argument")
                .and_then(Value::as_str)
                .map(str::to_string),
            protected: step
                .get("protected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }

    let descriptor = aad_runtime::assemble::assemble(name, &performed)
        .map_err(|problem| failure(&problem.code, &problem.message, None))?;

    // The same compiler `run_workflow` uses, so "it saved" and "it runs" are the
    // same claim rather than two hopes.
    let compiled = aad_core::compiler::compile_descriptor(
        descriptor.clone(),
        Some(aad_runtime::recordings::recordings_dir()),
    )
    .map_err(|error| {
        // The issues carry the path of each problem, and without them the caller
        // is told only that something is wrong. Measured: a bad name plus a bad
        // step yields three located issues, all of which used to be dropped.
        let located: Vec<Value> = error
            .issues
            .iter()
            .map(|issue| json!({"path": issue.path, "message": issue.message}))
            .collect();

        // Blame follows the path. A problem under $.metadata.name is the name the
        // caller chose; telling them it is a defect in the assembler would send
        // them to report a bug instead of renaming. Names must be lower case:
        // measured, "SubmitButton" and "UPPER" are both refused.
        let names_the_name = error
            .issues
            .iter()
            .any(|issue| issue.path.starts_with("$.metadata.name"));
        let hint = if names_the_name {
            "The name is not allowed. It has to start with a lower-case letter and \
carry only lower-case letters, digits, and . _ - between them, so \
\"submit_button\" where \"SubmitButton\" was refused."
        } else {
            "This is a defect in how the steps were assembled rather than in what \
you supplied. Report the steps you passed, and see `issues` for where it went wrong."
        };

        let mut payload = json!({
            "code": error.code,
            "message": format!("the assembled workflow would not compile: {}", error.message),
            "retryable": false,
            "effect": "not_applied",
            "hint": hint,
            "issues": located,
        });
        if payload["issues"].as_array().is_some_and(Vec::is_empty) {
            payload.as_object_mut().map(|map| map.remove("issues"));
        }
        serde_json::to_string(&payload).unwrap_or_else(|_| error.message.clone())
    })?;

    let existing = aad_runtime::recordings::workflow_path(name).ok();
    let replacing = existing.as_ref().is_some_and(|path| path.exists());
    let overwrite = arguments
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if replacing && !overwrite {
        return Err(failure(
            "WORKFLOW.NAME_IN_USE",
            &format!("a workflow named {name:?} already exists"),
            Some(
                "Pass overwrite: true to replace it, or choose another name. \
Replacing silently would let two different workflows collapse into one with \
nothing to show it happened.",
            ),
        ));
    }

    let path = aad_runtime::recordings::save_workflow(name, &descriptor)
        .map_err(|error| failure(error.code, &error.message, None))?;

    Ok(json!({
        "kind": "WorkflowSaved",
        "name": name,
        "path": path.to_string_lossy(),
        "performed_steps": performed.len(),
        // Three executed steps per performed action, so the number a caller sees
        // in `describe_workflow` matches what it reads here.
        "executed_steps": compiled
            .raw
            .get("steps")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
        "replaced": replacing,
        "inputs_required": descriptor
            .get("inputs")
            .and_then(Value::as_object)
            .map(|inputs| inputs.keys().cloned().collect::<Vec<String>>())
            .unwrap_or_default(),
    }))
}

/// The saved workflows, newest first.
fn list_workflows() -> Result<Value, String> {
    let saved = aad_runtime::recordings::list_workflows()
        .map_err(|error| failure(error.code, &error.message, None))?;
    // Whether each one would run belongs here rather than one describe call
    // later: an agent picks from this list, and a workflow that cannot run is
    // indistinguishable from one that can until it is tried. Compiling all of
    // them costs about 2ms each. The issues themselves stay in
    // `describe_workflow` -- a listing is for choosing, not for repairing.
    let workflows: Vec<Value> = saved
        .into_iter()
        .map(|entry| {
            let mut row = json!({"name": entry.name, "modified": entry.modified});
            match std::fs::read_to_string(&entry.path)
                .ok()
                .and_then(|body| serde_json::from_str::<Value>(&body).ok())
            {
                Some(document) => {
                    let directory = std::path::Path::new(&entry.path)
                        .parent()
                        .map(std::path::Path::to_path_buf);
                    match aad_core::compiler::compile_descriptor(document, directory) {
                        Ok(_) => row["runnable"] = json!(true),
                        Err(error) => {
                            row["runnable"] = json!(false);
                            row["issue_count"] = json!(error.issues.len());
                        }
                    }
                }
                None => row["runnable"] = json!(false),
            }
            row
        })
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

    let descriptor =
        aad_core::compiler::compile_descriptor(document, path.parent().map(Into::into)).map_err(
            |error| {
                // Where each problem is, not just that there is one: "invalid" alone
                // cannot be acted on, and the paths are what make a repair possible.
                let located: Vec<Value> = error
                    .issues
                    .iter()
                    .map(|issue| json!({"path": issue.path, "message": issue.message}))
                    .collect();
                let payload = json!({
                    "code": error.code,
                    "message": error.message,
                    "retryable": false,
                    "effect": "not_applied",
                    "hint": "The saved workflow is not valid. `issues` says where; the \
                desktop app can repair it, or save it again.",
                    "issues": located,
                });
                serde_json::to_string(&payload).unwrap_or_else(|_| error.message.clone())
            },
        )?;
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
        "step_count": steps.len(),
        // What it will actually touch. Step ids and types do not answer the
        // question asked before running something, so an agent deciding whether to
        // run a saved workflow had only its name and a step count to go on.
        "actions": what_it_does(raw),
        // Stated up front so an agent learns the limit from a read-only call
        // rather than from a refused run.
        "runnable": !uses_scripts(raw),
    }))
}

/// Fold the executed steps back into the actions they came from.
///
/// One saved action expands into snapshot/find/act, so reading a workflow back
/// step by step shows three entries for every one that was recorded, none of them
/// naming the element or the window.
fn what_it_does(raw: &Value) -> Vec<Value> {
    let Some(steps) = raw.get("steps").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut actions = Vec::new();
    for step in steps {
        let id = step.get("id").and_then(Value::as_str).unwrap_or_default();
        // The suffixes `assemble` emits. A step carrying neither is the action.
        if id.ends_with("_window") || id.ends_with("_element") {
            continue;
        }

        let uses = step.get("uses").and_then(Value::as_str).unwrap_or_default();
        let action = uses
            .rsplit_once('.')
            .map(|(_, tail)| tail.split('@').next().unwrap_or(tail))
            .unwrap_or(uses);

        let borrowed = |suffix: &str, key: &str| -> Option<Value> {
            let wanted = format!("{id}{suffix}");
            steps
                .iter()
                .find(|other| other.get("id").and_then(Value::as_str) == Some(wanted.as_str()))
                .and_then(|other| other.get("with"))
                .and_then(|with| with.get(key))
                .cloned()
        };

        let mut entry = serde_json::Map::new();
        entry.insert("id".into(), json!(id));
        entry.insert("action".into(), json!(action));
        if let Some(window) = borrowed("_window", "window") {
            entry.insert("window".into(), window);
        }
        if let Some(locator) = borrowed("_element", "locator") {
            entry.insert("finds".into(), locator);
        }

        let with = step.get("with");
        for key in ["value", "text"] {
            let Some(argument) = with.and_then(|with| with.get(key)).and_then(Value::as_str) else {
                continue;
            };
            // A credential is stored as a reference to an input rather than as
            // text. Reporting the reference says what has to be supplied without
            // handing the secret to whoever asked what the workflow does.
            if argument.contains("inputs.") {
                entry.insert("needsInput".into(), json!(argument));
            } else {
                entry.insert("writes".into(), json!(argument));
            }
        }
        actions.push(Value::Object(entry));
    }
    actions
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
///
/// Takes the driver because a recorded workflow is made of `action` steps that
/// dispatch through the desktop provider. Running one without registering that
/// provider would fail on the first step with a missing-provider error, which
/// Fold the run's events into one entry per logical step.
///
/// A saved action expands into snapshot/find/act, and an agent that saved one step
/// should read one step back. Only the last of the three says what was done; the
/// first two are how the element was reached.
///
/// Steps that never ran are reported as such rather than omitted. Leaving them out
/// makes a run that stopped halfway look like a shorter workflow that finished,
/// which is the reading that leads to a blind retry.
fn walk_through(result: &aad_runtime::RunResult) -> Vec<Value> {
    use std::collections::BTreeMap;

    // id -> (status, error), in the order the engine reported them.
    let mut seen: Vec<String> = Vec::new();
    let mut states: BTreeMap<String, (String, Option<Value>)> = BTreeMap::new();

    for event in &result.events {
        if event.event_type != "step.finished" {
            continue;
        }
        let Some(id) = event.payload.get("id").and_then(Value::as_str) else {
            continue;
        };
        let status = event
            .payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let error = event.payload.get("error").cloned();
        if !states.contains_key(id) {
            seen.push(id.to_string());
        }
        states.insert(id.to_string(), (status, error));
    }

    // Group the three executed steps back under the action they came from. The
    // suffixes are the ones `assemble` emits.
    let mut order: Vec<String> = Vec::new();
    let mut grouped: BTreeMap<String, Vec<(String, String, Option<Value>)>> = BTreeMap::new();
    for id in &seen {
        let logical = id
            .strip_suffix("_window")
            .or_else(|| id.strip_suffix("_element"))
            .unwrap_or(id)
            .to_string();
        if !grouped.contains_key(&logical) {
            order.push(logical.clone());
        }
        let (status, error) = states.get(id).cloned().unwrap_or_default();
        grouped
            .entry(logical)
            .or_default()
            .push((id.clone(), status, error));
    }

    let mut steps = Vec::new();
    for logical in order {
        let parts = grouped.get(&logical).cloned().unwrap_or_default();

        // Failed anywhere in the group means the action did not complete, and the
        // phase says whether the element was even reached -- "could not find it"
        // and "found it but the action was refused" call for different next moves.
        let failing = parts.iter().find(|(_, status, _)| status == "failed");
        let mut entry = serde_json::Map::new();
        entry.insert("id".into(), json!(logical));

        match failing {
            Some((id, _, error)) => {
                entry.insert("status".into(), json!("failed"));
                entry.insert(
                    "phase".into(),
                    json!(if id.ends_with("_window") {
                        "locating the window"
                    } else if id.ends_with("_element") {
                        "finding the element"
                    } else {
                        "performing the action"
                    }),
                );
                if let Some(error) = error {
                    entry.insert("error".into(), error.clone());
                }
            }
            None => {
                let done =
                    parts.len() == 3 && parts.iter().all(|(_, status, _)| status == "succeeded");
                entry.insert(
                    "status".into(),
                    json!(if done { "succeeded" } else { "incomplete" }),
                );
            }
        }
        steps.push(Value::Object(entry));
    }

    steps
}

/// Look again at what the failing step was reaching for.
///
/// An error code says an action failed; it does not say why. On this desktop a
/// disabled button reports `0x80040200`, and reading the element back shows
/// `enabled: false` -- that is the answer, and it is not in the code.
///
/// The reading can itself fail: an element removed from the page gives
/// `DRIVER.NOT_FOUND`, a closed window `DRIVER.WINDOW_NOT_FOUND`. Both were
/// measured. So a failed look is reported as a failed look and never replaces the
/// original error -- an agent shown a diagnosis error instead of the real one is
/// worse off than an agent shown no diagnosis at all.
fn why_it_failed(
    driver: &Arc<UiaDriver>,
    descriptor: &aad_core::WorkflowDescriptor,
    failing_step: &str,
) -> Option<Value> {
    // The window and locator come from the descriptor rather than from the run:
    // they say what was being reached for, which is what needs looking at again.
    let steps = descriptor.raw.get("steps")?.as_array()?;
    let find_id = format!("{failing_step}_element");
    let window_id = format!("{failing_step}_window");

    let locator = steps
        .iter()
        .find(|step| step.get("id").and_then(Value::as_str) == Some(find_id.as_str()))
        .and_then(|step| step.get("with"))
        .and_then(|with| with.get("locator"))
        .cloned();
    let window = steps
        .iter()
        .find(|step| step.get("id").and_then(Value::as_str) == Some(window_id.as_str()))
        .and_then(|step| step.get("with"))
        .and_then(|with| with.get("window"))
        .cloned();

    let mut report = serde_json::Map::new();

    // The window first: minimised or gone explains a whole class of failures, and
    // it explains them for every element inside it at once.
    if let Some(window) = &window {
        match driver.call("list_windows", &json!({})) {
            Ok(listing) => {
                let wanted_title = window.get("title").and_then(Value::as_str);
                let wanted_process = window.get("process_name").and_then(Value::as_str);
                let found = listing
                    .get("windows")
                    .and_then(Value::as_array)
                    .and_then(|windows| {
                        windows.iter().find(|candidate| {
                            let title_matches = wanted_title.is_none_or(|want| {
                                candidate
                                    .get("title")
                                    .and_then(Value::as_str)
                                    .is_some_and(|title| title.contains(want))
                            });
                            let process_matches = wanted_process.is_none_or(|want| {
                                candidate
                                    .get("process_name")
                                    .and_then(Value::as_str)
                                    .is_some_and(|name| name.eq_ignore_ascii_case(want))
                            });
                            title_matches && process_matches
                        })
                    });
                match found {
                    Some(candidate) => {
                        report.insert(
                            "window".into(),
                            json!({
                                "present": true,
                                "title": candidate.get("title").cloned().unwrap_or(Value::Null),
                                "is_minimized": candidate
                                    .get("is_minimized")
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                "is_foreground": candidate
                                    .get("is_foreground")
                                    .cloned()
                                    .unwrap_or(Value::Null),
                            }),
                        );
                    }
                    None => {
                        report.insert(
                            "window".into(),
                            json!({
                                "present": false,
                                "note": "The window the step names is not open. It may \
have been closed, or its title may have changed past the part the workflow matches on."
                            }),
                        );
                    }
                }
            }
            Err(error) => {
                report.insert(
                    "window".into(),
                    json!({"unreadable": error_payload(&error)}),
                );
            }
        }
    }

    // Then the element. `enabled`, `offscreen` and `read_only` are the three states
    // that turn "the action failed" into a reason.
    if let (Some(locator), Some(window)) = (&locator, &window) {
        let request = json!({"window": window, "locator": locator, "expect": "one"});
        match driver.call("find", &request) {
            Ok(found) => {
                let node = found.get("node").cloned().unwrap_or(Value::Null);
                report.insert(
                    "element".into(),
                    json!({
                        "present": true,
                        "states": node.get("states").cloned().unwrap_or(Value::Null),
                        "actions": node.get("actions").cloned().unwrap_or(Value::Null),
                        "name": node.get("name").cloned().unwrap_or(Value::Null),
                        "value": node.get("value").cloned().unwrap_or(Value::Null),
                    }),
                );
            }
            Err(error) => {
                // Not the original failure -- the original stays where it is. This
                // only says the element could not be looked at now.
                report.insert(
                    "element".into(),
                    json!({
                        "present": false,
                        "looking_failed": error_payload(&error),
                    }),
                );
            }
        }
    }

    if report.is_empty() {
        None
    } else {
        Some(Value::Object(report))
    }
}

/// looks like a broken recording rather than a mis-wired caller.
fn run_workflow(driver: &Arc<UiaDriver>, arguments: &Value) -> Result<Value, String> {
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

    // The same registry the CLI builds, so a workflow behaves identically
    // whether a person ran it or an agent did.
    let mut providers = aad_runtime::ProviderRegistry::new();
    providers.insert(driver.clone());

    let result = aad_runtime::run(
        &descriptor,
        aad_runtime::RunOptions::default()
            .with_inputs(inputs)
            .with_providers(providers),
    );

    let progress = walk_through(&result);

    let mut payload = json!({
        "kind": "WorkflowRun",
        "name": name,
        "status": result.status.as_str(),
        "outputs": result.outputs,
        "executed_steps": result.executed_steps,
        // What actually happened, step by step. `executed_steps` alone cannot tell
        // "all six succeeded" from "it reached the sixth and failed there", and
        // that difference decides whether retrying is safe: a workflow that wrote
        // a value and failed to submit will write it twice.
        "steps": progress,
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

    // Only when something actually failed. A successful run should not pay for a
    // second look, nor carry one in its answer.
    if result.error.is_some() {
        let stumbled = progress.iter().find_map(|step| {
            (step.get("status").and_then(Value::as_str) == Some("failed"))
                .then(|| step.get("id").and_then(Value::as_str))
                .flatten()
        });
        if let Some(failing) = stumbled {
            if let Some(context) = why_it_failed(driver, &descriptor, failing) {
                // Deliberately beside the error rather than inside it: this is a
                // fresh reading taken afterwards, not part of what the engine
                // reported, and conflating the two would misdate it.
                payload["at_the_time_of_failure"] = context;
            }
        }
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
            // Recommending automation_id and role was actively misleading: when
            // several elements collide it is usually because they already share
            // both -- five Edit buttons in a table have the same role and no
            // automation_id at all. Ordered by how well each survives the
            // interface changing.
            "The locator matched several elements, listed in details.candidates. \
Narrow it with `within` to name the container the wanted one sits in -- a row \
labelled with its own text stays correct after the list reorders -- or with \
`near` to place it against a neighbouring label. Use `nth` only when nothing \
distinguishes them but position.",
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

    #[test]
    fn an_agent_can_ration_the_answer_by_size_not_only_by_count() {
        // `limit` counts elements, which says nothing about how much comes back:
        // elements run from 169 to 4890 characters here, and 500 of them reached
        // 188147 characters on one window. An agent paying for context needs the
        // bound in the unit it is actually rationing.
        let tools = catalogue();
        let describe = tools
            .iter()
            .find(|tool| tool.name == "describe_window")
            .expect("describe_window");
        let ceiling = &describe.schema["properties"]["max_characters"];
        assert!(
            ceiling.is_object(),
            "describe_window must accept a size bound"
        );

        let text = ceiling["description"].as_str().unwrap_or_default();
        // The description has to say what to do about it, since `limit` and this
        // one fail in ways that call for opposite responses.
        assert!(
            text.contains("stopped_by"),
            "tell the agent how to learn which ceiling bit: {text:?}"
        );
        assert!(
            text.contains("region"),
            "and that narrowing is the answer when size ran out: {text:?}"
        );
    }

    #[test]
    fn writing_to_a_file_is_not_offered_over_mcp() {
        // Reading a window is declared read_only. Landing bytes on disk breaks
        // that, and an MCP client has no trusted directory convention -- handing
        // it an arbitrary path puts the permission decision with the party that
        // has the least context. The CLI is where a person supplies the path.
        for tool in catalogue() {
            let properties = tool.schema["properties"].as_object();
            let Some(properties) = properties else {
                continue;
            };
            for forbidden in ["out", "path", "file", "output_path", "written_to"] {
                assert!(
                    !properties.contains_key(forbidden),
                    "{} must not take {forbidden:?}",
                    tool.name
                );
            }
        }
    }

    /// Build a RunResult carrying the events a real run emits, so the folding is
    /// tested against the shape the engine actually produces rather than a guess.
    /// Measured on this desktop: 26 events for a two-action workflow, with `id`
    /// and `status` on `step.finished`.
    fn run_with(steps: &[(&str, &str)]) -> aad_runtime::RunResult {
        let events = steps
            .iter()
            .enumerate()
            .map(|(index, (id, status))| aad_runtime::RunEvent {
                run_id: "r".into(),
                seq: index as u64 + 1,
                event_type: "step.finished".into(),
                payload: json!({"id": id, "status": status}),
                created_at: "2026-01-01T00:00:00Z".into(),
            })
            .collect();
        aad_runtime::RunResult {
            run_id: "r".into(),
            workflow: "w".into(),
            plan_digest: "d".into(),
            status: aad_runtime::RunStatus::Failed,
            outputs: serde_json::Map::new(),
            return_value: None,
            error: None,
            executed_steps: steps.len() as u64,
            duration_seconds: 0.0,
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:00:00Z".into(),
            events,
            artifacts: None,
        }
    }

    #[test]
    fn a_run_that_stopped_halfway_says_which_step_did_it() {
        // The problem this solves: `executed_steps` alone cannot tell "all of them
        // succeeded" from "it reached the last one and failed there". An agent
        // reading the first meaning retries blindly, and a workflow that wrote a
        // value and failed to submit writes it twice.
        let result = run_with(&[
            ("step_1_window", "succeeded"),
            ("step_1_element", "succeeded"),
            ("step_1", "succeeded"),
            ("step_2_window", "succeeded"),
            ("step_2_element", "succeeded"),
            ("step_2", "failed"),
        ]);
        let steps = walk_through(&result);

        assert_eq!(
            steps.len(),
            2,
            "two performed actions, not six executed steps"
        );
        assert_eq!(steps[0]["id"], "step_1");
        assert_eq!(steps[0]["status"], "succeeded");
        assert_eq!(steps[1]["id"], "step_2");
        assert_eq!(steps[1]["status"], "failed");
    }

    #[test]
    fn where_it_failed_distinguishes_not_finding_from_not_working() {
        // "could not find the element" and "found it but the action was refused"
        // call for different next moves, and the error code alone does not
        // separate them.
        let missing = walk_through(&run_with(&[
            ("step_1_window", "succeeded"),
            ("step_1_element", "failed"),
        ]));
        assert_eq!(missing[0]["phase"], "finding the element");

        let refused = walk_through(&run_with(&[
            ("step_1_window", "succeeded"),
            ("step_1_element", "succeeded"),
            ("step_1", "failed"),
        ]));
        assert_eq!(refused[0]["phase"], "performing the action");

        let no_window = walk_through(&run_with(&[("step_1_window", "failed")]));
        assert_eq!(no_window[0]["phase"], "locating the window");
    }

    #[test]
    fn an_action_reported_only_in_part_is_not_called_done() {
        // A group missing its third entry means the run stopped before the action
        // itself. Calling that "succeeded" because nothing failed would be the
        // same false reassurance in a different shape.
        let steps = walk_through(&run_with(&[
            ("step_1_window", "succeeded"),
            ("step_1_element", "succeeded"),
        ]));
        assert_eq!(steps[0]["status"], "incomplete");
        assert!(
            steps[0].get("error").is_none(),
            "nothing failed, so no error"
        );
    }

    #[test]
    fn reading_a_workflow_back_says_what_it_will_touch() {
        // Step ids and types do not answer the question asked before running
        // something. An agent looking at `step_1_window` / `action` has only the
        // name and a count to decide on, and the three executed steps behind one
        // recorded action make even the count misleading.
        let raw = json!({
            "steps": [
                {"id": "step_1_window", "uses": "desktop.windows_uia.snapshot@1",
                 "with": {"window": {"process_name": "msedge.exe", "title": "Fixture"}}},
                {"id": "step_1_element", "uses": "desktop.windows_uia.find@1",
                 "with": {"locator": {"role": "edit", "automation_id": "city"}}},
                {"id": "step_1", "uses": "desktop.windows_uia.set_value@1",
                 "with": {"value": "Praha"}},
            ]
        });
        let actions = what_it_does(&raw);

        assert_eq!(actions.len(), 1, "one recorded action, not three steps");
        assert_eq!(actions[0]["action"], "set_value");
        assert_eq!(actions[0]["window"]["process_name"], "msedge.exe");
        assert_eq!(actions[0]["finds"]["automation_id"], "city");
        assert_eq!(actions[0]["writes"], "Praha");
    }

    #[test]
    fn a_credential_is_named_rather_than_shown() {
        // Someone asking what a workflow does is not asking for its password.
        let raw = json!({
            "steps": [
                {"id": "step_1_window", "uses": "desktop.windows_uia.snapshot@1",
                 "with": {"window": {"process_name": "app.exe"}}},
                {"id": "step_1_element", "uses": "desktop.windows_uia.find@1",
                 "with": {"locator": {"role": "edit"}}},
                {"id": "step_1", "uses": "desktop.windows_uia.set_value@1",
                 "with": {"value": "${{ inputs.step_1_secret }}"}},
            ]
        });
        let actions = what_it_does(&raw);
        assert_eq!(actions[0]["needsInput"], "${{ inputs.step_1_secret }}");
        assert!(
            actions[0].get("writes").is_none(),
            "a reference is not a value to write"
        );
    }

    #[test]
    fn an_agent_can_keep_what_it_worked_out() {
        // Without this an agent explores a window, acts, and loses all of it when
        // the session ends: the next run starts from describe_window again. The
        // targets it holds are no help -- a snapshot handle stops resolving once
        // the snapshot is gone.
        let tools = catalogue();
        let saving = tools
            .iter()
            .find(|tool| tool.name == "save_workflow")
            .expect("an agent needs a way to save what it did");
        assert!(saving.mutating, "it writes a file");

        let properties = &saving.schema["properties"];
        // It takes the steps, not a descriptor. Expanding one performed action
        // into snapshot/find/act is exactly what goes wrong when written by hand.
        assert!(properties.get("steps").is_some());
        assert!(
            properties.get("descriptor").is_none() && properties.get("workflow").is_none(),
            "assembling is this tool's job, not the caller's"
        );

        let step = &saving.schema["properties"]["steps"]["items"];
        let required: Vec<&str> = step["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for field in ["action", "locator", "window"] {
            assert!(required.contains(&field), "{field} is not optional");
        }
    }

    #[test]
    fn the_schema_warns_against_the_two_things_that_break_replay() {
        // Both were found the hard way on this desktop: a window id works in the
        // session that recorded it and never again, and a title carrying the open
        // document or a live status stops matching as soon as the application is
        // used.
        let tools = catalogue();
        let saving = tools
            .iter()
            .find(|tool| tool.name == "save_workflow")
            .expect("save_workflow");
        let window = &saving.schema["properties"]["steps"]["items"]["properties"]["window"];

        let described = window["description"].as_str().unwrap_or_default();
        assert!(
            described.contains("window_id") || described.contains("ids are assigned"),
            "say why an id will not do: {described:?}"
        );
        assert!(
            window["properties"].get("window_id").is_none(),
            "and do not accept one"
        );

        let title = window["properties"]["title"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            title.contains("does not change") || title.contains("varies"),
            "warn that titles move: {title:?}"
        );
    }

    #[test]
    fn a_credential_is_declared_rather_than_written_down() {
        let tools = catalogue();
        let saving = tools
            .iter()
            .find(|tool| tool.name == "save_workflow")
            .expect("save_workflow");
        let protected = saving.schema["properties"]["steps"]["items"]["properties"]["protected"]
            ["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            protected.contains("input") && protected.contains("credential"),
            "an agent has to know a password is not stored: {protected:?}"
        );
    }

    #[test]
    fn a_crowded_window_can_be_read_in_two_steps() {
        let tools = catalogue();

        let overview = tools
            .iter()
            .find(|tool| tool.name == "overview_window")
            .expect("an agent needs a way to see a window's shape before its contents");
        assert!(!overview.mutating, "mapping a window changes nothing");
        let properties = &overview.schema["properties"];
        assert!(
            properties.get("limit").is_none(),
            "an overview does not list elements, so a limit would mean nothing"
        );

        // The description has to carry the reason, because the schema is all an
        // agent reads. Measured: 9 of 20 windows here exceed the default limit.
        assert!(
            overview.description.contains("367") || overview.description.contains("twenty"),
            "say how crowded real windows get: {:?}",
            overview.description
        );

        let describe = tools
            .iter()
            .find(|tool| tool.name == "describe_window")
            .expect("describe_window");
        assert!(
            describe.description.contains("overview_window"),
            "describe_window must point at the overview, or an agent will keep \
hitting truncation without knowing there is another way: {:?}",
            describe.description
        );
        assert!(
            describe.schema["properties"].get("region").is_some(),
            "and it must accept the region name the overview hands out"
        );
    }

    #[test]
    fn a_region_name_means_the_same_thing_everywhere() {
        // Deliberately not a new selector language: the name the overview reports
        // is the ancestor name a locator carries in `within`. If the schemas
        // described these as different things an agent would learn two.
        let tools = catalogue();
        let describe = tools
            .iter()
            .find(|tool| tool.name == "describe_window")
            .expect("describe_window");
        let region = describe.schema["properties"]["region"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            region.contains("within"),
            "tell the agent the two agree: {region:?}"
        );
    }

    use super::*;

    #[test]
    fn asking_whether_something_is_gone_is_expressible() {
        // Without this an agent cannot ask "has the dialog closed?" at all: the
        // schema is closed, so the parameter would be rejected, and the only
        // available answer -- an error -- reads like a malfunction rather than
        // a no.
        let tool = find("find_element").expect("find_element must exist");
        let expect = &tool.schema["properties"]["expect"];

        assert!(
            !expect.is_null(),
            "find_element must let a caller say a miss is acceptable"
        );

        let modes: Vec<&str> = expect["enum"]
            .as_array()
            .expect("expect must be an enum so the choices are discoverable")
            .iter()
            .map(|value| value.as_str().expect("a string mode"))
            .collect();
        assert!(modes.contains(&"optional"), "modes were {modes:?}");

        // Ambiguity is not one of the choices. Relaxing "which one did you
        // mean" is how automation acts on the wrong element, and that must not
        // be reachable by asking a question about absence.
        assert!(!modes.contains(&"any"), "modes were {modes:?}");
    }

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

        for name in [
            "list_apps",
            "describe_window",
            "find_element",
            "probe_environment",
        ] {
            assert!(annotation(name, "readOnlyHint"), "{name} only reads");
        }
        for name in ["invoke", "type_text", "set_value", "pointer_click", "focus"] {
            assert!(
                !annotation(name, "readOnlyHint"),
                "{name} changes the desktop"
            );
            assert!(
                annotation(name, "destructiveHint"),
                "{name} must be flagged"
            );
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
                    target["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(field)),
                    "{name} target must require {field}"
                );
            }
        }
    }

    #[test]
    fn an_agent_can_express_which_of_several_namesakes_it_means() {
        // The driver resolves `within`, `near` and `nth` and they were verified
        // against a real page, but an agent only ever sees the schema -- and with
        // `additionalProperties: false` a client that validates would reject them
        // outright. Without these, five identical Edit buttons leave an agent no
        // way to say which row it means.
        let find = catalogue()
            .into_iter()
            .find(|tool| tool.name == "find_element")
            .expect("find_element");
        let locator = &find.schema["properties"]["locator"]["properties"];

        for field in ["within", "near", "nth"] {
            assert!(
                locator.get(field).is_some(),
                "locator schema has to offer {field}"
            );
        }
    }

    #[test]
    fn the_ways_of_narrowing_are_explained_not_merely_listed() {
        // A field an agent cannot tell when to use is a field it will misuse.
        // Measured: content anchors survive a restart while ordinals shift as
        // soon as a row is inserted, so the schema has to say which to reach for.
        let find = catalogue()
            .into_iter()
            .find(|tool| tool.name == "find_element")
            .expect("find_element");
        let locator = &find.schema["properties"]["locator"]["properties"];

        for field in ["within", "near", "nth"] {
            let described = locator[field]["description"]
                .as_str()
                .unwrap_or_default()
                .len();
            assert!(
                described > 40,
                "{field} needs to say when it applies, not just exist"
            );
        }
    }

    #[test]
    fn nested_selectors_cannot_smuggle_in_a_coordinate() {
        // `near.anchor` and `within` accept a nested locator, so they are
        // deliberately open-ended. That must not become a way around the rule
        // that an agent never names a point on the screen: the check above only
        // reads top-level properties.
        fn walk(schema: &Value, path: &str) {
            if let Some(properties) = schema["properties"].as_object() {
                for (key, value) in properties {
                    for forbidden in ["x", "y", "point", "coordinates", "position"] {
                        assert_ne!(key, forbidden, "{path} must not accept {forbidden}");
                    }
                    walk(value, &format!("{path}.{key}"));
                }
            }
        }

        for tool in catalogue() {
            walk(&tool.schema, tool.name);
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
    fn a_password_field_can_be_addressed_through_the_schema() {
        // The driver can match on `protected`, but an agent can only use what
        // the schema admits: `additionalProperties: false` means an unlisted
        // field is rejected rather than ignored. Password fields frequently have
        // no stable name, so without this they cannot be addressed at all.
        let tool = find("find_element").unwrap();
        let states = &tool.schema["properties"]["locator"]["properties"]["states"];
        assert_eq!(states["properties"]["protected"]["type"], json!("boolean"));
        assert_eq!(states["additionalProperties"], json!(false));
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

        // Asserting the word "narrow" appeared would not catch the failure that
        // mattered: the old hint said "narrow" while recommending automation_id
        // and role, the two fields colliding elements have already exhausted.
        let hint = payload["hint"].as_str().unwrap();
        for way in ["within", "near", "nth"] {
            assert!(
                hint.contains(way),
                "the hint has to point at {way}, which can actually separate them"
            );
        }
        assert!(
            hint.contains("candidates"),
            "and at the list of what it matched"
        );
    }

    #[test]
    fn probing_the_environment_needs_no_desktop() {
        // The tool must answer even where automation itself cannot run, so it is
        // asserted through `call` with a driver and through the no-driver path.
        let report = call(&stub_driver(), "probe_environment", &json!({})).unwrap();
        assert_eq!(report["kind"], "CapabilityProbe");

        let offline = call_without_driver("probe_environment", &json!({})).unwrap();
        assert_eq!(offline["kind"], "CapabilityProbe");
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
            Self {
                directory,
                _lock: lock,
            }
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

    /// A backend with one clickable button, so a recorded `action` step has
    /// somewhere real to land without touching the actual desktop.
    struct StubBackend;

    fn stub_window() -> aad_uia::WindowInfo {
        aad_uia::WindowInfo {
            window_id: "w1".into(),
            title: "Fixture".into(),
            process_id: 7,
            process_name: Some("fixture.exe".into()),
            class_name: Some("FixtureClass".into()),
            bounds: Some(aad_uia::Bounds {
                x: 0,
                y: 0,
                width: 400,
                height: 300,
            }),
            is_foreground: true,
            is_minimized: false,
        }
    }

    fn stub_node() -> aad_uia::Node {
        aad_uia::Node {
            node_id: "n1".into(),
            role: "Button".into(),
            name: Some("Save".into()),
            value: None,
            automation_id: Some("saveButton".into()),
            class_name: None,
            framework_id: None,
            bounds: Some(aad_uia::Bounds {
                x: 10,
                y: 10,
                width: 60,
                height: 20,
            }),
            states: aad_uia::States {
                enabled: Some(true),
                ..Default::default()
            },
            actions: vec!["invoke".into(), "focus".into()],
            depth: 1,
            parent_id: None,
            children: Vec::new(),
        }
    }

    impl aad_uia::Backend for StubBackend {
        fn list_windows(&self) -> Result<Vec<aad_uia::WindowInfo>, aad_uia::DriverError> {
            Ok(vec![stub_window()])
        }

        fn capture(
            &self,
            _window_id: &str,
            _limits: aad_uia::CaptureLimits,
        ) -> Result<aad_uia::CapturedTree, aad_uia::DriverError> {
            Ok(aad_uia::CapturedTree {
                window: stub_window(),
                root_id: Some("n1".into()),
                truncated: false,
                nodes: vec![stub_node()],
            })
        }

        fn verify(
            &self,
            _window_id: &str,
            _node: &aad_uia::Node,
        ) -> Result<bool, aad_uia::DriverError> {
            Ok(true)
        }

        fn focus(&self, _w: &str, _n: &aad_uia::Node) -> Result<(), aad_uia::DriverError> {
            Ok(())
        }
        fn invoke(&self, _w: &str, _n: &aad_uia::Node) -> Result<(), aad_uia::DriverError> {
            Ok(())
        }
        fn set_value(
            &self,
            _w: &str,
            _n: &aad_uia::Node,
            _v: &str,
        ) -> Result<(), aad_uia::DriverError> {
            Ok(())
        }
        fn type_text(
            &self,
            _w: &str,
            _n: &aad_uia::Node,
            _t: &str,
        ) -> Result<(), aad_uia::DriverError> {
            Ok(())
        }
        fn pointer_click(&self, _w: &str, _n: &aad_uia::Node) -> Result<(), aad_uia::DriverError> {
            Ok(())
        }
    }

    /// A driver over the stub backend, with a private snapshot store.
    fn stub_driver() -> Arc<UiaDriver> {
        Arc::new(UiaDriver::with_store(
            Arc::new(StubBackend),
            aad_uia::SnapshotStore::new(8, std::time::Duration::from_secs(60)),
        ))
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

        let described = call_without_driver("describe_workflow", &json!({"name": "greeter"}))
            .expect("describe");

        assert_eq!(described["name"], json!("greeter"));
        assert_eq!(described["step_count"], json!(1));
        assert!(
            described["inputs"]["who"].is_object(),
            "declared inputs are reported"
        );
        assert!(described["outputs"]["greeting"].is_object());
        assert!(described["runnable"].as_bool().unwrap());
        assert!(
            described["planDigest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:"),
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
            let error =
                call_without_driver("describe_workflow", &json!({"name": name})).unwrap_err();
            let payload: Value = serde_json::from_str(&error).unwrap();
            assert_eq!(
                payload["code"],
                json!("STORE.NAME_INVALID"),
                "{name:?} must be refused as a name"
            );
        }
    }

    #[test]
    fn running_a_saved_workflow_returns_its_outputs() {
        let store = ScratchStore::new("run");
        store.save("greeter", pure_workflow());

        let result = run_workflow(
            &stub_driver(),
            &json!({"name": "greeter", "inputs": {"who": "agent"}}),
        )
        .expect("run");

        assert_eq!(result["status"], json!("succeeded"));
        assert_eq!(result["outputs"]["greeting"], json!("agent"));
        assert_eq!(result["executed_steps"], json!(1));
    }

    #[test]
    fn a_recorded_workflow_reaches_the_desktop_provider() {
        // Recordings compile to `action` steps that dispatch through the desktop
        // provider. If the run is not given that provider, every real recording
        // fails on its first step -- and it fails looking like a broken
        // recording rather than a mis-wired caller, which is why this is
        // asserted with an action step rather than a pure one.
        let store = ScratchStore::new("action");
        let mut workflow = pure_workflow();
        workflow["steps"] = json!([{
            "id": "press",
            "type": "action",
            "uses": "desktop.windows_uia.find@1",
            "with": {
                "window": {"class_name": "FixtureClass"},
                "locator": {"automation_id": "saveButton"}
            }
        }]);
        workflow["variables"] = json!({});
        workflow["outputs"] = json!({});
        store.save("recorded", workflow);

        let result =
            run_workflow(&stub_driver(), &json!({"name": "recorded"})).expect("the run completes");

        assert_eq!(
            result["status"],
            json!("succeeded"),
            "an action step must resolve against the registered provider: {result}"
        );
        assert_eq!(result["executed_steps"], json!(1));
    }

    #[test]
    fn the_answers_name_their_fields_one_way() {
        // Eleven of the fourteen tools answered in snake_case and the workflow
        // three in camelCase, so the same number carried three names:
        // `executed_steps` from save_workflow, `stepCount` from
        // describe_workflow and `stepsExecuted` from run_workflow. An agent that
        // has read `node_id` and `snapshot_id` will guess `executed_steps`, and
        // guessing wrong reads as the field being absent rather than misspelt.
        //
        // `planDigest` and `apiVersion` are deliberately exempt: they are the
        // spec's own persisted field names, read back from checkpoints and
        // workflow documents, so renaming them here would disagree with the files
        // on disk.
        const SPEC_FIELDS: [&str; 4] = ["planDigest", "apiVersion", "stepId", "kind"];

        fn offenders(value: &Value, at: String, found: &mut Vec<String>) {
            match value {
                Value::Object(fields) => {
                    for (key, nested) in fields {
                        if key.chars().any(|c| c.is_uppercase())
                            && !SPEC_FIELDS.contains(&key.as_str())
                        {
                            found.push(format!("{at}.{key}"));
                        }
                        offenders(nested, format!("{at}.{key}"), found);
                    }
                }
                Value::Array(items) => {
                    for (at_index, nested) in items.iter().enumerate() {
                        offenders(nested, format!("{at}[{at_index}]"), found);
                    }
                }
                _ => {}
            }
        }

        let store = ScratchStore::new("naming");
        store.save("greeter", pure_workflow());

        let described = describe_workflow(&json!({"name": "greeter"})).expect("describe");
        let ran = run_workflow(
            &stub_driver(),
            &json!({"name": "greeter", "inputs": {"who": "a"}}),
        )
        .expect("run");

        let mut found = Vec::new();
        offenders(&described, "describe_workflow".into(), &mut found);
        offenders(&ran, "run_workflow".into(), &mut found);
        assert!(
            found.is_empty(),
            "these fields are not snake_case: {found:?}"
        );
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

        let error = run_workflow(&stub_driver(), &json!({"name": "dangerous"}))
            .expect_err("must be refused");
        let payload: Value = serde_json::from_str(&error).expect("structured error");

        assert_eq!(payload["code"], json!("MCP.SCRIPTS_REFUSED"));
        // Nothing ran, so the refusal must not imply a half-done run.
        assert_eq!(payload["effect"], json!("not_applied"));
        assert!(
            payload["hint"]
                .as_str()
                .unwrap()
                .contains("--allow-scripts"),
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
        match run_workflow(&stub_driver(), &json!({"name": "broken"})) {
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

        let error = run_workflow(
            &stub_driver(),
            &json!({"name": "greeter", "inputs": "who=agent"}),
        )
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
