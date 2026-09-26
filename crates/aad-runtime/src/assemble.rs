//! Assembling a workflow from steps someone or something already performed.
//!
//! Recording produced steps, and the engine runs workflows, but nothing turned
//! one into the other on this side: `toDescriptor` lives in the GUI's
//! TypeScript, so the CLI printed recorded steps and left the caller to write the
//! descriptor by hand. Doing that by hand is how six format errors got past me
//! in one attempt -- the expansion is not obvious, and `validate` is the only
//! thing that catches it.
//!
//! The shape it produces is fixed by what the engine accepts, verified against
//! real descriptors on disk rather than recalled:
//!
//! - Each performed action snapshots the window, resolves every durable locator,
//!   then acts on references produced by that same run. Most actions expand to
//!   three executed steps; drag expands to four because both source and
//!   destination must be resolved from the fresh snapshot. A
//!   `snapshot:revision:node` handle stops resolving when its snapshot is gone.
//! - The window is named by `process_name`, never by `window_id`. Window ids do
//!   not survive a restart of the target.
//! - A protected value becomes a required, sensitive input rather than a literal.

use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

/// The descriptor dialect the engine accepts.
const API_VERSION: &str = "ai-auto-desktop.dev/v1alpha1";
const WORKFLOW_KIND: &str = "Workflow";

/// The parameters a recorded action is allowed to persist.
///
/// Runtime references (`target` and drag's `to`) are deliberately absent: they
/// are rebuilt from durable locators during every replay. Text is also kept out
/// of this map because protected fields externalise it as a sensitive input.
fn parameter_names(action: &str) -> Option<&'static [&'static str]> {
    match action {
        "focus" | "invoke" | "pointer_click" | "set_value" => Some(&[]),
        "type_text" => Some(&["mode"]),
        "key_chord" => Some(&["keys"]),
        "pointer_gesture" => Some(&["button", "count", "fraction", "scroll", "hover_ms"]),
        "drag" => Some(&["button", "fraction", "to_fraction"]),
        _ => None,
    }
}

fn invalid_parameter(ordinal: usize, action: &str, message: impl Into<String>) -> AssemblyProblem {
    AssemblyProblem::new(
        "WORKFLOW.PARAMETER_INVALID",
        format!(
            "step {ordinal} gives {action:?} invalid parameters: {}",
            message.into()
        ),
    )
}

fn validate_fraction(
    ordinal: usize,
    action: &str,
    name: &str,
    value: Option<&Value>,
) -> Result<(), AssemblyProblem> {
    let Some(value) = value else { return Ok(()) };
    let Some(object) = value.as_object() else {
        return Err(invalid_parameter(
            ordinal,
            action,
            format!("{name} must be an object"),
        ));
    };
    if object.len() != 2 || !object.contains_key("fx") || !object.contains_key("fy") {
        return Err(invalid_parameter(
            ordinal,
            action,
            format!("{name} must contain only fx and fy"),
        ));
    }
    for axis in ["fx", "fy"] {
        let Some(number) = object.get(axis).and_then(Value::as_f64) else {
            return Err(invalid_parameter(
                ordinal,
                action,
                format!("{name}.{axis} must be a number"),
            ));
        };
        if !number.is_finite() || !(0.0..=1.0).contains(&number) {
            return Err(invalid_parameter(
                ordinal,
                action,
                format!("{name}.{axis} must be between 0.0 and 1.0"),
            ));
        }
    }
    Ok(())
}

/// Validate the durable portion of an action before writing it down.
///
/// The workflow compiler validates descriptor shape, not provider arguments. If
/// this layer merely copied arbitrary JSON, `save_workflow` could report success
/// for a workflow that the driver deterministically refuses on replay.
fn validate_parameters(
    ordinal: usize,
    action: &str,
    parameters: &Map<String, Value>,
) -> Result<(), AssemblyProblem> {
    match action {
        "focus" | "invoke" | "pointer_click" | "set_value" => {}
        "type_text" => {
            if let Some(mode) = parameters.get("mode") {
                if !matches!(mode.as_str(), Some("unicode" | "keys")) {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        "mode must be unicode or keys",
                    ));
                }
            }
        }
        "key_chord" => {
            let Some(keys) = parameters.get("keys").and_then(Value::as_array) else {
                return Err(invalid_parameter(ordinal, action, "keys is required"));
            };
            if keys.is_empty() || keys.len() > 6 {
                return Err(invalid_parameter(
                    ordinal,
                    action,
                    "keys must contain between 1 and 6 entries",
                ));
            }
            let mut main_keys = 0usize;
            for key in keys {
                let Some(key) = key.as_str() else {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        "every key must be a string",
                    ));
                };
                let normalized = key.trim().to_ascii_lowercase();
                let modifier = matches!(
                    normalized.as_str(),
                    "ctrl"
                        | "control"
                        | "shift"
                        | "alt"
                        | "menu"
                        | "win"
                        | "super"
                        | "meta"
                        | "cmd"
                );
                let alphanumeric = normalized.len() == 1
                    && normalized
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric);
                let named = matches!(
                    normalized.as_str(),
                    "enter"
                        | "return"
                        | "tab"
                        | "esc"
                        | "escape"
                        | "space"
                        | "spacebar"
                        | "backspace"
                        | "back"
                        | "delete"
                        | "del"
                        | "insert"
                        | "ins"
                        | "home"
                        | "end"
                        | "pageup"
                        | "pgup"
                        | "pagedown"
                        | "pgdn"
                        | "up"
                        | "down"
                        | "left"
                        | "right"
                        | "f1"
                        | "f2"
                        | "f3"
                        | "f4"
                        | "f5"
                        | "f6"
                        | "f7"
                        | "f8"
                        | "f9"
                        | "f10"
                        | "f11"
                        | "f12"
                );
                if !modifier && !alphanumeric && !named {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        format!("unknown key {key:?}"),
                    ));
                }
                if !modifier {
                    main_keys += 1;
                }
            }
            if main_keys != 1 {
                return Err(invalid_parameter(
                    ordinal,
                    action,
                    "a chord needs exactly one non-modifier key",
                ));
            }
        }
        "pointer_gesture" => {
            if let Some(button) = parameters.get("button") {
                if !matches!(button.as_str(), Some("left" | "right" | "middle")) {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        "button must be left, right or middle",
                    ));
                }
            }
            if let Some(count) = parameters.get("count") {
                if !matches!(count.as_u64(), Some(1 | 2)) {
                    return Err(invalid_parameter(ordinal, action, "count must be 1 or 2"));
                }
            }
            validate_fraction(ordinal, action, "fraction", parameters.get("fraction"))?;
            if let Some(scroll) = parameters.get("scroll") {
                let Some(scroll) = scroll.as_object() else {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        "scroll must be an object",
                    ));
                };
                if scroll
                    .keys()
                    .any(|key| key != "direction" && key != "notches")
                {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        "scroll accepts only direction and notches",
                    ));
                }
                if !matches!(
                    scroll.get("direction").and_then(Value::as_str),
                    Some("up" | "down" | "left" | "right")
                ) {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        "scroll.direction must be up, down, left or right",
                    ));
                }
                if let Some(notches) = scroll.get("notches") {
                    if !matches!(notches.as_u64(), Some(1..=30)) {
                        return Err(invalid_parameter(
                            ordinal,
                            action,
                            "scroll.notches must be between 1 and 30",
                        ));
                    }
                }
            }
            if let Some(hover_ms) = parameters.get("hover_ms") {
                if !matches!(hover_ms.as_u64(), Some(1..=5_000)) {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        "hover_ms must be between 1 and 5000",
                    ));
                }
            }
            let scroll = parameters.contains_key("scroll");
            let hover = parameters.contains_key("hover_ms");
            let click = parameters.contains_key("button") || parameters.contains_key("count");
            if (scroll && hover) || ((scroll || hover) && click) {
                return Err(invalid_parameter(
                    ordinal,
                    action,
                    "scroll, hover and click parameters are mutually exclusive",
                ));
            }
        }
        "drag" => {
            if let Some(button) = parameters.get("button") {
                if !matches!(button.as_str(), Some("left" | "right" | "middle")) {
                    return Err(invalid_parameter(
                        ordinal,
                        action,
                        "button must be left, right or middle",
                    ));
                }
            }
            validate_fraction(ordinal, action, "fraction", parameters.get("fraction"))?;
            validate_fraction(
                ordinal,
                action,
                "to_fraction",
                parameters.get("to_fraction"),
            )?;
        }
        _ => unreachable!("action was checked before parameter validation"),
    }
    Ok(())
}

/// Actions that carry text, and the argument each one expects.
///
/// `set_value` replaces a field's contents and `type_text` sends keystrokes, so
/// they name their argument differently; every other action takes none.
fn text_field(action: &str) -> Option<&'static str> {
    match action {
        "set_value" => Some("value"),
        "type_text" => Some("text"),
        _ => None,
    }
}

/// One thing that was done, described so it can be done again.
#[derive(Debug, Clone)]
pub struct PerformedStep {
    /// The action name, without the provider prefix or version.
    pub action: String,
    /// How to find the element again, from `describe` or `find`.
    pub locator: Value,
    /// Which window it was in, as a selector rather than an id.
    pub window: Value,
    /// The legacy text field used by recordings created before structured
    /// parameters existed. Kept so old files reopen and compile unchanged.
    pub argument: Option<String>,
    /// Durable action parameters, excluding runtime target references and text.
    pub parameters: Map<String, Value>,
    /// The durable destination locator for a drag. No other action accepts it.
    pub destination: Option<Value>,
    /// Whether that text was a credential, which must not be written down.
    pub protected: bool,
}

/// What went wrong while assembling, phrased for whoever supplied the steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblyProblem {
    pub code: String,
    pub message: String,
}

impl AssemblyProblem {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// Build a workflow descriptor from steps that were already performed.
///
/// Refuses rather than emitting something the engine will reject later: a
/// descriptor that fails at run time has already cost the caller a run, and the
/// reason is harder to see from a run failure than from here.
pub fn assemble(name: &str, steps: &[PerformedStep]) -> Result<Value, AssemblyProblem> {
    if name.trim().is_empty() {
        return Err(AssemblyProblem::new(
            "WORKFLOW.NAME_REQUIRED",
            "a workflow needs a name to be run by",
        ));
    }
    if steps.is_empty() {
        return Err(AssemblyProblem::new(
            "WORKFLOW.NO_STEPS",
            "a workflow with no steps would report success without doing anything",
        ));
    }

    let mut expanded: Vec<Value> = Vec::new();
    let mut inputs = Map::new();

    for (index, step) in steps.iter().enumerate() {
        let ordinal = index + 1;
        let step_id = format!("step_{ordinal}");
        let snapshot_id = format!("{step_id}_window");
        let find_id = format!("{step_id}_element");
        let destination_find_id = format!("{step_id}_destination");

        if step.action.trim().is_empty() {
            return Err(AssemblyProblem::new(
                "WORKFLOW.ACTION_REQUIRED",
                format!("step {ordinal} does not say what to do"),
            ));
        }
        if step.locator.is_null() || !step.locator.is_object() {
            return Err(AssemblyProblem::new(
                "WORKFLOW.LOCATOR_REQUIRED",
                format!(
                    "step {ordinal} has no locator, so the element could not be found again. \
Locators come from `describe` or `find`; an element whose locator is null cannot be \
told apart from its siblings and cannot be recorded."
                ),
            ));
        }
        if !step.window.is_object() || step.window.as_object().is_some_and(Map::is_empty) {
            return Err(AssemblyProblem::new(
                "WORKFLOW.WINDOW_REQUIRED",
                format!(
                    "step {ordinal} has no window selector. Use `process_name` and a \
distinguishing part of the title -- a window id does not survive the target restarting."
                ),
            ));
        }
        if step
            .window
            .get("window_id")
            .is_some_and(|value| !value.is_null())
        {
            return Err(AssemblyProblem::new(
                "WORKFLOW.WINDOW_ID_NOT_PORTABLE",
                format!(
                    "step {ordinal} selects its window by id. Ids are assigned per session, \
so the workflow would fail the next time the target starts. Use `process_name` and part \
of the title instead."
                ),
            ));
        }

        let allowed = parameter_names(&step.action).ok_or_else(|| {
            AssemblyProblem::new(
                "WORKFLOW.ACTION_UNSUPPORTED",
                format!("step {ordinal} uses unsupported action {:?}", step.action),
            )
        })?;
        let allowed: BTreeSet<&str> = allowed.iter().copied().collect();
        let unknown: Vec<&str> = step
            .parameters
            .keys()
            .map(String::as_str)
            .filter(|name| !allowed.contains(name))
            .collect();
        if !unknown.is_empty() {
            return Err(AssemblyProblem::new(
                "WORKFLOW.PARAMETER_UNSUPPORTED",
                format!(
                    "step {ordinal} gives {:?} parameter(s) {:?}; accepted parameters are {:?}",
                    step.action, unknown, allowed
                ),
            ));
        }
        validate_parameters(ordinal, &step.action, &step.parameters)?;
        match (&*step.action, &step.destination) {
            ("drag", Some(destination))
                if destination.is_object()
                    && destination
                        .as_object()
                        .is_some_and(|value| !value.is_empty()) => {}
            ("drag", _) => {
                return Err(AssemblyProblem::new(
                    "WORKFLOW.DESTINATION_REQUIRED",
                    format!("step {ordinal} is a drag but has no durable destination locator"),
                ))
            }
            (_, Some(_)) => {
                return Err(AssemblyProblem::new(
                    "WORKFLOW.DESTINATION_UNUSED",
                    format!(
                        "step {ordinal} is {:?}, so a drag destination would be ignored",
                        step.action
                    ),
                ))
            }
            (_, None) => {}
        }

        expanded.push(json!({
            "id": snapshot_id,
            "type": "action",
            "uses": "desktop.windows_uia.snapshot@1",
            "with": {"window": step.window},
        }));
        expanded.push(json!({
            "id": find_id,
            "type": "action",
            "uses": "desktop.windows_uia.find@1",
            "with": {
                "snapshot_id": format!("${{{{ steps.{snapshot_id}.output.snapshot_id }}}}"),
                "locator": step.locator,
            },
        }));
        if let Some(destination) = &step.destination {
            expanded.push(json!({
                "id": destination_find_id,
                "type": "action",
                "uses": "desktop.windows_uia.find@1",
                "with": {
                    "snapshot_id": format!("${{{{ steps.{snapshot_id}.output.snapshot_id }}}}"),
                    "locator": destination,
                },
            }));
        }

        let mut arguments = step.parameters.clone();
        // The references this run produced, never ones saved at record time.
        arguments.insert(
            "target".into(),
            json!(format!("${{{{ steps.{find_id}.output.ref }}}}")),
        );
        if step.destination.is_some() {
            arguments.insert(
                "to".into(),
                json!(format!("${{{{ steps.{destination_find_id}.output.ref }}}}")),
            );
        }

        if let Some(field) = text_field(&step.action) {
            if step.protected {
                // Named after the step so two credentials in one workflow stay
                // separate, and required so a missing one fails before the first
                // action rather than halfway through a login.
                let input_name = format!("{step_id}_secret");
                inputs.insert(
                    input_name.clone(),
                    json!({
                        "schema": {"type": "string"},
                        "required": true,
                        "sensitive": true,
                    }),
                );
                arguments.insert(
                    field.into(),
                    json!(format!("${{{{ inputs.{input_name} }}}}")),
                );
            } else {
                arguments.insert(
                    field.into(),
                    json!(step.argument.clone().unwrap_or_default()),
                );
            }
        } else if step.argument.is_some() {
            return Err(AssemblyProblem::new(
                "WORKFLOW.ARGUMENT_UNUSED",
                format!(
                    "step {ordinal} is {:?}, which carries no text, but an argument was \
given. Silently dropping it would leave the workflow doing less than asked.",
                    step.action
                ),
            ));
        }

        expanded.push(json!({
            "id": step_id,
            "type": "action",
            "uses": format!("desktop.windows_uia.{}@1", step.action),
            "with": Value::Object(arguments),
        }));
    }

    let mut descriptor = json!({
        "apiVersion": API_VERSION,
        "kind": WORKFLOW_KIND,
        "metadata": {"name": name},
        "budgets": {
            "max_duration": "5m",
            // One performed action costs three executed steps, so the budget is
            // set from the expanded count. Setting it from the performed count
            // would make a valid workflow run out partway.
            "max_executed_steps": (expanded.len() * 2).max(10),
        },
        "steps": expanded,
    });
    // Only present when something was actually externalised, so an ordinary
    // workflow keeps the shape it had before.
    if !inputs.is_empty() {
        descriptor["inputs"] = Value::Object(inputs);
    }
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(action: &str) -> PerformedStep {
        PerformedStep {
            action: action.to_string(),
            locator: json!({"role": "button", "name": "Save"}),
            window: json!({"process_name": "app.exe", "title": "Editor"}),
            argument: None,
            parameters: Map::new(),
            destination: None,
            protected: false,
        }
    }

    #[test]
    fn one_performed_action_becomes_three_executed_steps() {
        // Not an implementation detail worth hiding: the reference an action
        // consumes has to be produced by this run. A `snapshot:revision:node`
        // handle saved at record time stops resolving as soon as that snapshot
        // is gone, so the workflow has to re-find the element every time.
        let descriptor = assemble("save", &[step("invoke")]).expect("assembled");
        let steps = descriptor["steps"].as_array().expect("steps");
        assert_eq!(steps.len(), 3);

        assert_eq!(steps[0]["uses"], "desktop.windows_uia.snapshot@1");
        assert_eq!(steps[1]["uses"], "desktop.windows_uia.find@1");
        assert_eq!(steps[2]["uses"], "desktop.windows_uia.invoke@1");

        // Each one consumes what the previous produced.
        assert_eq!(
            steps[1]["with"]["snapshot_id"],
            "${{ steps.step_1_window.output.snapshot_id }}"
        );
        assert_eq!(
            steps[2]["with"]["target"],
            "${{ steps.step_1_element.output.ref }}"
        );
    }

    #[test]
    fn the_budget_counts_expanded_steps_not_performed_ones() {
        // Three performed actions are nine executed steps. A budget of three
        // would stop a valid workflow a third of the way through.
        let performed = vec![step("invoke"), step("focus"), step("invoke")];
        let descriptor = assemble("many", &performed).expect("assembled");
        let allowed = descriptor["budgets"]["max_executed_steps"]
            .as_u64()
            .expect("budget");
        assert!(
            allowed >= 9,
            "nine steps have to fit, got a budget of {allowed}"
        );
    }

    #[test]
    fn a_credential_becomes_a_required_input_rather_than_a_literal() {
        let mut secret = step("set_value");
        secret.protected = true;
        secret.argument = Some("hunter2".into());
        let descriptor = assemble("login", &[secret]).expect("assembled");

        let rendered = serde_json::to_string(&descriptor).expect("render");
        assert!(
            !rendered.contains("hunter2"),
            "the password must not be written into the workflow: {rendered}"
        );

        let input = &descriptor["inputs"]["step_1_secret"];
        assert_eq!(input["sensitive"], true);
        assert_eq!(
            input["required"], true,
            "a missing credential should fail before the first action, not midway"
        );
        let steps = descriptor["steps"].as_array().expect("steps");
        assert_eq!(steps[2]["with"]["value"], "${{ inputs.step_1_secret }}");
    }

    #[test]
    fn an_ordinary_workflow_declares_no_inputs() {
        let descriptor = assemble("plain", &[step("invoke")]).expect("assembled");
        assert!(
            descriptor.get("inputs").is_none(),
            "an inputs block with nothing in it invites the reader to look for one"
        );
    }

    #[test]
    fn a_window_chosen_by_id_is_refused() {
        // Ids are assigned per session. A workflow that selects by id works
        // exactly once -- in the session that recorded it -- and then fails,
        // which is the failure mode hardest to diagnose later.
        let mut by_id = step("invoke");
        by_id.window = json!({"window_id": "hwnd:12345"});
        let problem = assemble("fragile", &[by_id]).expect_err("must refuse");
        assert_eq!(problem.code, "WORKFLOW.WINDOW_ID_NOT_PORTABLE");
        assert!(
            problem.message.contains("process_name"),
            "say what to use instead: {}",
            problem.message
        );
    }

    #[test]
    fn a_step_without_a_locator_is_refused_with_the_reason() {
        // `describe` reports a null locator for an element it cannot tell apart
        // from its siblings. Assembling that into a workflow produces one that
        // fails on `find`, and the caller then has to work out why.
        let mut nameless = step("invoke");
        nameless.locator = Value::Null;
        let problem = assemble("unfindable", &[nameless]).expect_err("must refuse");
        assert_eq!(problem.code, "WORKFLOW.LOCATOR_REQUIRED");
        assert!(problem.message.contains("describe"));
    }

    #[test]
    fn an_argument_given_to_an_action_that_ignores_it_is_refused() {
        // Dropping it silently would leave the workflow doing less than the
        // caller asked for, with nothing to show that it happened.
        let mut clicking = step("invoke");
        clicking.argument = Some("typed text".into());
        let problem = assemble("confused", &[clicking]).expect_err("must refuse");
        assert_eq!(problem.code, "WORKFLOW.ARGUMENT_UNUSED");
    }

    #[test]
    fn an_empty_workflow_is_refused() {
        // It would run, report success, and do nothing -- the one outcome an
        // agent cannot distinguish from having worked.
        let problem = assemble("nothing", &[]).expect_err("must refuse");
        assert_eq!(problem.code, "WORKFLOW.NO_STEPS");
    }

    #[test]
    fn set_value_and_type_text_name_their_argument_differently() {
        let mut writing = step("set_value");
        writing.argument = Some("hello".into());
        let written = assemble("w", &[writing]).expect("assembled");
        assert_eq!(written["steps"][2]["with"]["value"], "hello");

        let mut typing = step("type_text");
        typing.argument = Some("hello".into());
        let typed = assemble("t", &[typing]).expect("assembled");
        assert_eq!(typed["steps"][2]["with"]["text"], "hello");
    }

    #[test]
    fn structured_parameters_are_preserved_for_each_action() {
        let mut typing = step("type_text");
        typing.argument = Some("hello".into());
        typing.parameters.insert("mode".into(), json!("keys"));
        let typed = assemble("keys", &[typing]).expect("assembled");
        assert_eq!(typed["steps"][2]["with"]["mode"], "keys");

        let mut chord = step("key_chord");
        chord.parameters.insert("keys".into(), json!(["CTRL", "A"]));
        let chord = assemble("chord", &[chord]).expect("assembled");
        assert_eq!(chord["steps"][2]["with"]["keys"], json!(["CTRL", "A"]));

        let mut gesture = step("pointer_gesture");
        gesture.parameters.insert("button".into(), json!("right"));
        gesture
            .parameters
            .insert("fraction".into(), json!({"fx": 0.25, "fy": 0.75}));
        let gesture = assemble("gesture", &[gesture]).expect("assembled");
        assert_eq!(gesture["steps"][2]["with"]["button"], "right");
        assert_eq!(gesture["steps"][2]["with"]["fraction"]["fx"], 0.25);
    }

    #[test]
    fn drag_refinds_both_endpoints_from_one_snapshot() {
        let mut drag = step("drag");
        drag.destination = Some(json!({"role": "listitem", "name": "Done"}));
        drag.parameters
            .insert("fraction".into(), json!({"fx": 0.9, "fy": 0.5}));
        drag.parameters
            .insert("to_fraction".into(), json!({"fx": 0.1, "fy": 0.5}));

        let descriptor = assemble("move", &[drag]).expect("assembled");
        let steps = descriptor["steps"].as_array().expect("steps");
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[2]["id"], "step_1_destination");
        assert_eq!(
            steps[2]["with"]["snapshot_id"],
            "${{ steps.step_1_window.output.snapshot_id }}"
        );
        assert_eq!(steps[3]["uses"], "desktop.windows_uia.drag@1");
        assert_eq!(
            steps[3]["with"]["to"],
            "${{ steps.step_1_destination.output.ref }}"
        );
    }

    #[test]
    fn invalid_structured_parameters_are_refused_before_saving() {
        let mut chord = step("key_chord");
        chord.parameters.insert("keys".into(), json!([]));
        let problem = assemble("empty_chord", &[chord]).expect_err("empty chord must fail");
        assert_eq!(problem.code, "WORKFLOW.PARAMETER_INVALID");

        let mut chord = step("key_chord");
        chord
            .parameters
            .insert("keys".into(), json!(["ctrl", "nope"]));
        let problem = assemble("unknown_key", &[chord]).expect_err("unknown key must fail");
        assert_eq!(problem.code, "WORKFLOW.PARAMETER_INVALID");

        let mut chord = step("key_chord");
        chord.parameters.insert("keys".into(), json!(["a", "b"]));
        let problem = assemble("two_keys", &[chord]).expect_err("two main keys must fail");
        assert_eq!(problem.code, "WORKFLOW.PARAMETER_INVALID");

        let mut gesture = step("pointer_gesture");
        gesture
            .parameters
            .insert("scroll".into(), json!({"direction": "down"}));
        gesture.parameters.insert("button".into(), json!("left"));
        let problem = assemble("mixed_gesture", &[gesture]).expect_err("mixed gesture must fail");
        assert_eq!(problem.code, "WORKFLOW.PARAMETER_INVALID");

        let mut drag = step("drag");
        drag.destination = Some(json!({"role": "listitem", "name": "Done"}));
        drag.parameters
            .insert("to_fraction".into(), json!({"fx": 1.2, "fy": 0.5}));
        let problem = assemble("outside", &[drag]).expect_err("out of range must fail");
        assert_eq!(problem.code, "WORKFLOW.PARAMETER_INVALID");
    }

    #[test]
    fn unknown_parameters_and_runtime_references_are_refused() {
        let mut chord = step("key_chord");
        chord
            .parameters
            .insert("target".into(), json!("saved:1:e9"));
        let problem = assemble("unsafe", &[chord]).expect_err("must refuse");
        assert_eq!(problem.code, "WORKFLOW.PARAMETER_UNSUPPORTED");

        let mut click = step("invoke");
        click.destination = Some(json!({"role": "button", "name": "Other"}));
        let problem = assemble("unused", &[click]).expect_err("must refuse");
        assert_eq!(problem.code, "WORKFLOW.DESTINATION_UNUSED");
    }
}
