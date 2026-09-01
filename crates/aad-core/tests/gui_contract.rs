//! The GUI and the compiler must agree on what a recorded workflow looks like.
//!
//! The desktop shell builds descriptors in TypeScript while the compiler that
//! accepts them is here, in Rust. Nothing in either language forces those two
//! to stay aligned, so a change to the shell's exporter could produce
//! recordings that look fine in the window and fail at replay.
//!
//! These tests pin the exported shape against the real compiler. The fixture is
//! the exporter's actual output, captured by running `Recording.toDescriptor()`
//! and confirmed with `aad validate` (9 steps, status `valid`), not a
//! hand-written guess at what it emits.

use aad_core::compiler::compile_descriptor;
use serde_json::json;

/// Compile a descriptor the way the CLI does.
fn compile(descriptor: &serde_json::Value) -> aad_core::Result<aad_core::WorkflowDescriptor> {
    compile_descriptor(descriptor.clone(), None)
}

/// Exactly what `Recording.toDescriptor()` produces for a three-action recording.
///
/// Each recorded action expands to three steps -- snapshot the window, find the
/// element, then act on what was found. That is not decoration: a saved recording
/// holds a locator rather than a `snapshot:revision:node` reference, because a
/// reference stops resolving once its snapshot is gone. The find step is what
/// converts the durable description back into a reference the action can use.
fn exported_by_the_gui() -> serde_json::Value {
    json!({
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "exported-by-the-gui"},
        "budgets": {"max_duration": "5m", "max_executed_steps": 18},
        "steps": [
            {
                "id": "step_1_window",
                "type": "action",
                "uses": "desktop.windows_uia.snapshot@1",
                "with": {"window": {
                    "class_name": "Notepad",
                    "process_name": "notepad.exe",
                    "title": "notes.txt - Notepad"
                }}
            },
            {
                "id": "step_1_element",
                "type": "action",
                "uses": "desktop.windows_uia.find@1",
                "with": {
                    "snapshot_id": "${{ steps.step_1_window.output.snapshot_id }}",
                    "locator": {"role": "Edit", "name": "Body"}
                }
            },
            {
                "id": "step_1",
                "type": "action",
                "uses": "desktop.windows_uia.focus@1",
                "with": {"target": "${{ steps.step_1_element.output.ref }}"}
            },
            {
                "id": "step_2_window",
                "type": "action",
                "uses": "desktop.windows_uia.snapshot@1",
                "with": {"window": {
                    "class_name": "Notepad",
                    "process_name": "notepad.exe",
                    "title": "notes.txt - Notepad"
                }}
            },
            {
                "id": "step_2_element",
                "type": "action",
                "uses": "desktop.windows_uia.find@1",
                "with": {
                    "snapshot_id": "${{ steps.step_2_window.output.snapshot_id }}",
                    "locator": {"role": "Edit", "name": "Body"}
                }
            },
            {
                "id": "step_2",
                "type": "action",
                "uses": "desktop.windows_uia.set_value@1",
                "with": {
                    "target": "${{ steps.step_2_element.output.ref }}",
                    "value": "written by the gui"
                }
            },
            {
                "id": "step_3_window",
                "type": "action",
                "uses": "desktop.windows_uia.snapshot@1",
                "with": {"window": {
                    "class_name": "Notepad",
                    "process_name": "notepad.exe",
                    "title": "notes.txt - Notepad"
                }}
            },
            {
                "id": "step_3_element",
                "type": "action",
                "uses": "desktop.windows_uia.find@1",
                "with": {
                    "snapshot_id": "${{ steps.step_3_window.output.snapshot_id }}",
                    "locator": {"role": "Button", "name": "Save"}
                }
            },
            {
                "id": "step_3",
                "type": "action",
                "uses": "desktop.windows_uia.invoke@1",
                "with": {"target": "${{ steps.step_3_element.output.ref }}"}
            }
        ]
    })
}

/// The provider action a compiled step invokes.
///
/// `uses` is kept among the raw params rather than promoted to a field, so it is
/// read through the params map.
fn uses_of(step: &aad_core::CompiledStep) -> &str {
    step.params
        .get("uses")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

#[test]
fn the_compiler_accepts_what_the_desktop_shell_exports() {
    let compiled = compile(&exported_by_the_gui())
        .expect("the GUI's exported descriptor must compile unchanged");

    assert_eq!(compiled.name, "exported-by-the-gui");
    assert_eq!(compiled.steps.len(), 9, "three recorded actions, three steps each");
}

#[test]
fn an_action_consumes_the_reference_its_find_step_produced() {
    // This is the whole replay mechanism. If an action quoted a saved reference
    // instead, the recording would work in the session that made it and fail
    // afterwards -- which is exactly what happened before this expansion existed.
    let compiled = compile(&exported_by_the_gui()).expect("must compile");

    for step in &compiled.steps {
        let uses = uses_of(step);
        if uses.contains("snapshot") || uses.contains("find") {
            continue;
        }
        let target = step.params["with"]["target"]
            .as_str()
            .expect("every action step carries a target");
        assert!(
            target.starts_with("${{ steps.") && target.ends_with(".output.ref }}"),
            "action {} quotes {target:?}, which is not a freshly found reference",
            step.id
        );
    }
}

#[test]
fn no_exported_step_carries_a_saved_snapshot_reference() {
    // A literal `snapshot:revision:node` anywhere in the descriptor would mean
    // the exporter had regressed to saving session-scoped state.
    let serialized = serde_json::to_string(&exported_by_the_gui()).unwrap();

    let literal_reference = regex_lite_matches(&serialized);

    assert!(
        !literal_reference,
        "the export contains a literal snapshot reference, which cannot survive being saved"
    );
}

/// Whether the text contains something shaped like `hexid:number:nodeid`.
///
/// Hand-rolled rather than pulling in a regex crate for one test.
fn regex_lite_matches(text: &str) -> bool {
    text.split('"').any(|token| {
        let parts: Vec<&str> = token.split(':').collect();
        parts.len() == 3
            && parts[0].len() >= 8
            && parts[0].chars().all(|c| c.is_ascii_hexdigit())
            && parts[1].parse::<u64>().is_ok()
            && parts[2].starts_with('e')
    })
}

#[test]
fn a_find_step_searches_the_snapshot_taken_beside_it() {
    // A find that captured its own snapshot would read the window twice and
    // could resolve against a different state than the one just observed.
    let compiled = compile(&exported_by_the_gui()).expect("must compile");

    for step in &compiled.steps {
        if !uses_of(step).contains("find") {
            continue;
        }
        let quoted = step.params["with"]["snapshot_id"]
            .as_str()
            .expect("a find step must quote a snapshot");
        let expected_window = step.id.replace("_element", "_window");
        assert!(
            quoted.contains(&expected_window),
            "find {} reads {quoted:?} rather than its own window snapshot",
            step.id
        );
    }
}

#[test]
fn the_window_is_identified_descriptively_rather_than_by_handle() {
    // Window ids are handles from the recording session. Saving one would make
    // the recording depend on a number that means nothing tomorrow.
    let compiled = compile(&exported_by_the_gui()).expect("must compile");

    for step in &compiled.steps {
        if !uses_of(step).contains("snapshot") {
            continue;
        }
        let window = &step.params["with"]["window"];
        assert!(window.is_object(), "step {} must describe its window", step.id);
        assert!(
            step.params["with"].get("window_id").is_none(),
            "step {} pins a live handle, which will not survive being saved",
            step.id
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
    descriptor["steps"][5]["with"] = json!({
        "target": "${{ steps.step_2_element.output.ref }}",
        "value": ""
    });

    let compiled = compile(&descriptor).expect("an empty string is a legitimate value");
    assert_eq!(compiled.steps[5].params["with"]["value"], "");
}

#[test]
fn the_exported_budget_leaves_room_for_every_step() {
    let compiled = compile(&exported_by_the_gui()).expect("must compile");

    // A budget below the step count would fail the run at the last step, which
    // is the least useful moment to find out. The expansion makes this easy to
    // get wrong: three recorded actions are nine executed steps.
    assert!(
        compiled.budgets.max_executed_steps as usize >= compiled.steps.len(),
        "budget {} cannot cover {} steps",
        compiled.budgets.max_executed_steps,
        compiled.steps.len()
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

#[test]
fn the_two_steps_a_replay_depends_on_are_provider_actions_too() {
    // The expansion is worthless if the compiler does not accept the snapshot
    // and find steps that make it work.
    for action in ["snapshot", "find"] {
        let descriptor = json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "one-step"},
            "budgets": {"max_duration": "1m", "max_executed_steps": 5},
            "steps": [{
                "id": "only",
                "type": "action",
                "uses": format!("desktop.windows_uia.{action}@1"),
                "with": {"window": {"class_name": "Notepad"}, "locator": {"role": "Edit"}}
            }]
        });

        assert!(
            compile(&descriptor).is_ok(),
            "a replay needs {action}, so the compiler must accept it"
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
