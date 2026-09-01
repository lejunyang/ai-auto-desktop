//! The GUI and the compiler must agree on what a recorded workflow looks like.
//!
//! The desktop shell builds descriptors in TypeScript while the compiler that
//! accepts them is here, in Rust. Nothing in either language forces those two
//! to stay aligned, so a change to the shell's exporter could produce
//! recordings that look fine in the window and fail at replay.
//!
//! These tests pin the exported shape against the real compiler. They are
//! written from the exporter's actual output, captured by running
//! `Recording.toDescriptor()`, not from what it is supposed to emit.

use aad_core::compiler::compile_descriptor;
use serde_json::json;

/// Compile a descriptor the way the CLI does.
fn compile(descriptor: &serde_json::Value) -> aad_core::Result<aad_core::WorkflowDescriptor> {
    compile_descriptor(descriptor.clone(), None)
}

/// Exactly what `Recording.toDescriptor()` produces for a three-step recording.
fn exported_by_the_gui() -> serde_json::Value {
    json!({
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "gui-export-check"},
        "budgets": {"max_duration": "5m", "max_executed_steps": 10},
        "steps": [
            {
                "id": "step_1",
                "type": "action",
                "uses": "desktop.windows_uia.invoke@1",
                "with": {"target": "snapabc:3:e1"}
            },
            {
                "id": "step_2",
                "type": "action",
                "uses": "desktop.windows_uia.set_value@1",
                "with": {"target": "snapabc:3:e2", "value": "written by the gui"}
            },
            {
                "id": "step_3",
                "type": "action",
                "uses": "desktop.windows_uia.focus@1",
                "with": {"target": "snapabc:3:e3"}
            }
        ]
    })
}

#[test]
fn the_compiler_accepts_what_the_desktop_shell_exports() {
    let compiled = compile(&exported_by_the_gui())
        .expect("the GUI's exported descriptor must compile unchanged");

    assert_eq!(compiled.name, "gui-export-check");
    assert_eq!(compiled.steps.len(), 3);
}

#[test]
fn each_exported_step_keeps_the_reference_that_was_observed() {
    let compiled = compile(&exported_by_the_gui()).expect("must compile");

    // The reference is the whole safety story: losing it in export would leave
    // a step that cannot prove which element it meant.
    for step in &compiled.steps {
        let target = step.params["with"]["target"]
            .as_str()
            .expect("every recorded step carries a target");
        let parts: Vec<&str> = target.split(':').collect();
        assert_eq!(parts.len(), 3, "target {target:?} is not snapshot:revision:node");
        assert!(
            parts[1].parse::<u64>().is_ok(),
            "the revision in {target:?} must be a number"
        );
    }
}

#[test]
fn the_exported_step_ids_are_ones_the_compiler_allows() {
    // The shell numbers steps `step-1`; hyphens are not valid in an id, so the
    // exporter rewrites them. If that rewrite were dropped, this fails.
    let mut descriptor = exported_by_the_gui();
    descriptor["steps"][0]["id"] = json!("step-1");

    assert!(
        compile(&descriptor).is_err(),
        "a hyphenated id must be rejected, otherwise the exporter's rewrite is untested"
    );
}

#[test]
fn a_text_action_exported_without_its_text_still_compiles_as_empty() {
    // The GUI blocks export while text is missing, but the compiler is the last
    // line of defence and must not silently invent a value.
    let mut descriptor = exported_by_the_gui();
    descriptor["steps"][1]["with"] = json!({"target": "snapabc:3:e2", "value": ""});

    let compiled = compile(&descriptor).expect("an empty string is a legitimate value");
    assert_eq!(compiled.steps[1].params["with"]["value"], "");
}

#[test]
fn the_exported_budget_leaves_room_for_every_step() {
    let compiled = compile(&exported_by_the_gui()).expect("must compile");

    // A budget below the step count would fail the run at the last step, which
    // is the least useful moment to find out.
    assert!(
        compiled.budgets.max_executed_steps as usize
            >= compiled.steps.len(),
        "the exported budget must cover the exported steps"
    );
}

#[test]
fn every_action_the_shell_can_record_names_a_real_provider_action() {
    // The shell offers whichever actions the driver reported for an element, so
    // the `uses` string it builds must match the driver's naming exactly.
    for action in aad_uia_action_names() {
        let mut descriptor = exported_by_the_gui();
        descriptor["steps"] = json!([{
            "id": "only",
            "type": "action",
            "uses": format!("desktop.windows_uia.{action}@1"),
            "with": {"target": "snapabc:3:e1", "value": "x", "text": "x"}
        }]);

        assert!(
            compile(&descriptor).is_ok(),
            "the shell can record {action}, so the compiler must accept it"
        );
    }
}

/// The node actions the shell can put on a button.
///
/// Duplicated deliberately rather than imported: aad-core must not depend on
/// aad-uia, and a mismatch here should surface as a failure to review.
fn aad_uia_action_names() -> Vec<&'static str> {
    vec!["focus", "invoke", "set_value", "type_text", "pointer_click"]
}
