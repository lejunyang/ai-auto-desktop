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
//! - Each performed action becomes **three** executed steps: snapshot the
//!   window, find the element by locator, then act on the reference that find
//!   produced. The reference has to come from the run, not from the recording:
//!   a `snapshot:revision:node` handle stops resolving the moment the snapshot
//!   is gone.
//! - The window is named by `process_name`, never by `window_id`. Window ids do
//!   not survive a restart of the target.
//! - A protected value becomes a required, sensitive input rather than a literal.

use serde_json::{json, Map, Value};

/// The descriptor dialect the engine accepts.
const API_VERSION: &str = "ai-auto-desktop.dev/v1alpha1";
const WORKFLOW_KIND: &str = "Workflow";

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
    /// The text that was typed or written, when the action carries any.
    pub argument: Option<String>,
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

        let mut arguments = Map::new();
        // The reference this run produced, not one saved at record time.
        arguments.insert(
            "target".into(),
            json!(format!("${{{{ steps.{find_id}.output.ref }}}}")),
        );

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
}
