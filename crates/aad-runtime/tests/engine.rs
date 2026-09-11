//! Behavioural tests for the workflow engine.
//!
//! These drive complete descriptors through compilation and execution, so they
//! assert on what a user actually observes: the run status, the outputs, the
//! journal, and how many times a provider was really called.

use aad_core::{compile_descriptor, AutomationError, WorkflowDescriptor};
use aad_plugin::manifest;
use aad_runtime::provider::Provider;
use aad_runtime::{run, ProviderRegistry, RunOptions, RunStatus};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Test providers
// ---------------------------------------------------------------------------

type Handler = Box<dyn Fn(&str, Value, usize) -> Result<Value, AutomationError> + Send + Sync>;

/// A provider whose behaviour is a closure, counting every invocation.
struct Fake {
    manifest: aad_plugin::CapabilityManifest,
    handler: Handler,
    calls: AtomicUsize,
    seen: Mutex<Vec<Value>>,
}

impl Fake {
    fn build(name: &str, actions: Value, handler: Handler) -> Arc<Self> {
        let document = manifest::document(name, actions.as_object().cloned().unwrap_or_default());
        Arc::new(Self {
            manifest: manifest::parse(&document).expect("fixture manifest is valid"),
            handler,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// Read-only actions that echo their arguments.
    fn echo(name: &str, actions: &[&str]) -> Arc<Self> {
        Self::build(
            name,
            read_only_actions(actions),
            Box::new(|_, args, _| Ok(json!({"echo": args}))),
        )
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn arguments(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
}

impl Provider for Fake {
    fn manifest(&self) -> &aad_plugin::CapabilityManifest {
        &self.manifest
    }

    fn invoke(
        &self,
        action: &str,
        args: Value,
        _timeout: Option<Duration>,
    ) -> Result<Value, AutomationError> {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.seen.lock().unwrap().push(args.clone());
        (self.handler)(action, args, attempt)
    }
}

fn read_only_actions(names: &[&str]) -> Value {
    let mut actions = Map::new();
    for name in names {
        actions.insert(
            (*name).to_string(),
            json!({"contract_major": 1, "effect": {"default_class": "read_only"}}),
        );
    }
    Value::Object(actions)
}

fn actions_with_effect(names: &[&str], class: &str) -> Value {
    let mut actions = Map::new();
    for name in names {
        actions.insert(
            (*name).to_string(),
            json!({"contract_major": 1, "effect": {"default_class": class}}),
        );
    }
    Value::Object(actions)
}

// ---------------------------------------------------------------------------
// Descriptor helpers
// ---------------------------------------------------------------------------

fn descriptor(body: Value) -> WorkflowDescriptor {
    let mut document = json!({
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "test.workflow"},
        "budgets": {"max_duration": "30s", "max_executed_steps": 100}
    });
    for (key, value) in body.as_object().expect("body must be an object") {
        document[key] = value.clone();
    }
    compile_descriptor(document, None).expect("the fixture descriptor must compile")
}

fn registry(providers: Vec<Arc<dyn Provider>>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    for provider in providers {
        registry.insert(provider);
    }
    registry
}

fn execute(descriptor: &WorkflowDescriptor, providers: ProviderRegistry) -> aad_runtime::RunResult {
    run(descriptor, RunOptions::default().with_providers(providers))
}

/// Execute with `script` steps permitted, as a trusting caller would.
fn execute_trusting_scripts(descriptor: &WorkflowDescriptor) -> aad_runtime::RunResult {
    run(descriptor, RunOptions::default().with_scripts_allowed(true))
}

fn event_types(result: &aad_runtime::RunResult) -> Vec<String> {
    result
        .events
        .iter()
        .map(|event| event.event_type.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Basic execution
// ---------------------------------------------------------------------------

#[test]
fn a_single_action_runs_and_reports_success() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "steps": [{"id": "act", "type": "action", "uses": "fixture.ping@1", "with": {"a": 1}}]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.calls(), 1);
    assert_eq!(provider.arguments()[0], json!({"a": 1}));
    assert_eq!(result.executed_steps, 1);
}

#[test]
fn outputs_are_resolved_from_step_results() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "steps": [{"id": "act", "type": "action", "uses": "fixture.ping@1", "with": {"a": 7}}],
        "outputs": {"value": {"value": "${{ steps.act.output.echo.a }}"}}
    }));

    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.outputs["value"], json!(7));
}

#[test]
fn an_unknown_action_fails_without_side_effects() {
    let workflow = descriptor(json!({
        "steps": [{"id": "act", "type": "action", "uses": "missing.thing@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Failed);
    let error = result.error.expect("an error is reported");
    assert_eq!(error.code, "ACTION.UNKNOWN");
    assert_eq!(error.effect, "not_applied");
}

#[test]
fn the_journal_records_the_full_lifecycle() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "steps": [{"id": "act", "type": "action", "uses": "fixture.ping@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![provider]));
    let types = event_types(&result);

    assert_eq!(types.first().map(String::as_str), Some("run.started"));
    assert_eq!(types.last().map(String::as_str), Some("run.finished"));
    for expected in [
        "step.started",
        "action.started",
        "action.finished",
        "step.finished",
    ] {
        assert!(
            types.contains(&expected.to_string()),
            "missing {expected} in {types:?}"
        );
    }
    // Sequence numbers must be dense and start at 1.
    let sequences: Vec<u64> = result.events.iter().map(|event| event.seq).collect();
    assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>());
}

#[test]
fn the_journal_says_which_element_an_action_landed_on() {
    // Without this a replay that clicked the wrong element is indistinguishable
    // from a correct one: both are a run of green steps and a `succeeded`. For a
    // recording played back after a restart, "which of the four same-named
    // buttons did it pick" is the only question worth asking.
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["find"]),
        Box::new(|_, _, _| {
            Ok(json!({
                "found": true,
                "match_count": 1,
                "node": {"node_id": "e8", "role": "button", "name": "Close"},
                "ref": "snap:1133:e8",
            }))
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{"id": "look", "type": "action", "uses": "fixture.find@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    let finished = result
        .events
        .iter()
        .find(|event| event.event_type == "action.finished")
        .expect("an action finished");
    let recorded = &finished.payload["result"];

    assert_eq!(recorded["node"]["node_id"], json!("e8"));
    assert_eq!(recorded["node"]["name"], json!("Close"));
    assert_eq!(recorded["found"], json!(true));
    assert_eq!(recorded["ref"], json!("snap:1133:e8"));
}

#[test]
fn a_large_result_is_counted_rather_than_copied_into_the_journal() {
    // A snapshot output measured on a real window is 6491 characters, of which
    // the node array is 5905. Copying that for every step would make the journal
    // unreadable for the sake of data that is already in the snapshot store --
    // but dropping it entirely is what left a replay unexplainable, so the length
    // stays.
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["snapshot"]),
        Box::new(|_, _, _| {
            let nodes: Vec<Value> = (0..500)
                .map(|index| json!({"node_id": format!("e{index}"), "role": "button"}))
                .collect();
            Ok(json!({
                "snapshot_id": "abc123",
                "revision": 7,
                "nodes": nodes,
                "window": {"window_id": "hwnd:1", "title": "App", "process_name": "app.exe"},
            }))
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{"id": "look", "type": "action", "uses": "fixture.snapshot@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    let finished = result
        .events
        .iter()
        .find(|event| event.event_type == "action.finished")
        .expect("an action finished");
    let recorded = &finished.payload["result"];

    assert_eq!(
        recorded["nodes_count"],
        json!(500),
        "the size is the useful part"
    );
    assert!(
        recorded.get("nodes").is_none(),
        "the nodes themselves must not be copied"
    );
    // What identifies the observation is still there.
    assert_eq!(recorded["snapshot_id"], json!("abc123"));
    assert_eq!(recorded["window"]["title"], json!("App"));

    let rendered = serde_json::to_string(recorded).expect("serialisable");
    assert!(
        rendered.len() < 400,
        "summary grew to {} characters: {rendered}",
        rendered.len()
    );
}

#[test]
fn a_summary_leaves_out_field_contents() {
    // A value can hold an entire document, and for a protected field it is
    // withheld on purpose. Neither belongs in a log kept for every run.
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["find"]),
        Box::new(|_, _, _| {
            Ok(json!({
                "node": {
                    "node_id": "e2",
                    "role": "edit",
                    "name": "Notes",
                    "value": "a very long body of text the user typed",
                },
            }))
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{"id": "look", "type": "action", "uses": "fixture.find@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    let finished = result
        .events
        .iter()
        .find(|event| event.event_type == "action.finished")
        .expect("an action finished");
    let node = &finished.payload["result"]["node"];

    assert_eq!(node["node_id"], json!("e2"), "identity is kept");
    assert!(node.get("value").is_none(), "contents are not: {node}");
}

// ---------------------------------------------------------------------------
// Inputs and variables
// ---------------------------------------------------------------------------

#[test]
fn a_missing_required_input_fails_before_any_action() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "inputs": {"name": {"schema": {"type": "string"}, "required": true}},
        "steps": [{"id": "act", "type": "action", "uses": "fixture.ping@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "INPUT.MISSING");
    assert_eq!(
        provider.calls(),
        0,
        "no action may run when inputs are invalid"
    );
}

#[test]
fn an_input_default_is_applied_when_the_value_is_omitted() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "inputs": {"greeting": {"schema": {"type": "string"}, "default": "hello"}},
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.ping@1",
            "with": {"text": "${{ inputs.greeting }}"}
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.arguments()[0], json!({"text": "hello"}));
}

#[test]
fn an_undeclared_input_is_rejected() {
    let workflow = descriptor(json!({
        "steps": [{"id": "done", "type": "return"}]
    }));
    let mut inputs = Map::new();
    inputs.insert("typo".into(), json!(1));

    let result = run(&workflow, RunOptions::default().with_inputs(inputs));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "INPUT.UNDECLARED");
}

#[test]
fn set_updates_a_mutable_variable_for_later_steps() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "variables": {"counter": {"schema": {"type": "integer"}, "mutable": true, "initial": 1}},
        "steps": [
            {"id": "bump", "type": "set", "assign": {"vars.counter": "${{ vars.counter + 41 }}"}},
            {
                "id": "act", "type": "action", "uses": "fixture.ping@1",
                "with": {"value": "${{ vars.counter }}"}
            }
        ]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.arguments()[0], json!({"value": 42}));
}

// ---------------------------------------------------------------------------
// Control flow
// ---------------------------------------------------------------------------

#[test]
fn an_if_step_takes_only_the_matching_branch() {
    let provider = Fake::echo("fixture", &["yes", "no"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "branch", "type": "if", "condition": "${{ True }}",
            "then": [{"id": "taken", "type": "action", "uses": "fixture.yes@1", "with": {}}],
            "else": [{"id": "skipped", "type": "action", "uses": "fixture.no@1", "with": {}}]
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.calls(), 1);
}

#[test]
fn a_step_level_if_skips_without_consuming_step_budget() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.ping@1", "with": {},
            "if": "${{ False }}"
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.calls(), 0);
    assert_eq!(result.executed_steps, 0);
    assert!(event_types(&result).contains(&"step.skipped".to_string()));
}

#[test]
fn a_switch_runs_the_first_matching_case_only() {
    let provider = Fake::echo("fixture", &["a", "b", "c"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "pick", "type": "switch",
            "cases": [
                {"when": "${{ False }}", "steps": [
                    {"id": "first", "type": "action", "uses": "fixture.a@1", "with": {}}
                ]},
                {"when": "${{ True }}", "steps": [
                    {"id": "second", "type": "action", "uses": "fixture.b@1", "with": {}}
                ]},
                {"when": "${{ True }}", "steps": [
                    {"id": "third", "type": "action", "uses": "fixture.c@1", "with": {}}
                ]}
            ]
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.calls(), 1);
}

#[test]
fn a_switch_falls_through_to_default() {
    let provider = Fake::echo("fixture", &["a", "fallback"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "pick", "type": "switch",
            "cases": [{"when": "${{ False }}", "steps": [
                {"id": "never", "type": "action", "uses": "fixture.a@1", "with": {}}
            ]}],
            "default": [{"id": "chosen", "type": "action", "uses": "fixture.fallback@1", "with": {}}]
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.arguments().len(), 1);
}

#[test]
fn foreach_binds_each_item_and_its_index() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "loop", "type": "foreach",
            "items": "${{ ['a', 'b', 'c'] }}",
            "as": "item", "index_as": "position", "max_items": 10,
            "steps": [{
                "id": "act", "type": "action", "uses": "fixture.ping@1",
                "with": {"item": "${{ item }}", "position": "${{ position }}"}
            }]
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(
        provider.arguments(),
        vec![
            json!({"item": "a", "position": 0}),
            json!({"item": "b", "position": 1}),
            json!({"item": "c", "position": 2}),
        ]
    );
}

#[test]
fn foreach_refuses_to_exceed_its_declared_bound() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "loop", "type": "foreach",
            "items": "${{ [1, 2, 3, 4] }}", "as": "item", "max_items": 2,
            "steps": [{"id": "act", "type": "action", "uses": "fixture.ping@1", "with": {}}]
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "LOOP.MAX_ITEMS_EXCEEDED");
    assert_eq!(provider.calls(), 0, "the bound is checked before iterating");
}

#[test]
fn while_iterates_until_its_condition_is_false() {
    let workflow = descriptor(json!({
        "variables": {"count": {"schema": {"type": "integer"}, "mutable": true, "initial": 0}},
        "steps": [{
            "id": "loop", "type": "while",
            "condition": "${{ vars.count < 3 }}", "max_iterations": 10, "timeout": "10s",
            "steps": [{"id": "bump", "type": "set", "assign": {"vars.count": "${{ vars.count + 1 }}"}}]
        }],
        "outputs": {"total": {"value": "${{ vars.count }}"}}
    }));

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.outputs["total"], json!(3));
}

#[test]
fn while_stops_loudly_when_it_exceeds_its_iteration_bound() {
    let workflow = descriptor(json!({
        "variables": {"count": {"schema": {"type": "integer"}, "mutable": true, "initial": 0}},
        "steps": [{
            "id": "loop", "type": "while",
            "condition": "${{ True }}", "max_iterations": 3, "timeout": "10s",
            "steps": [{"id": "bump", "type": "set", "assign": {"vars.count": "${{ vars.count + 1 }}"}}]
        }]
    }));

    let result = execute(&workflow, registry(vec![]));

    // Silently truncating would look like success; it must not.
    assert_ne!(result.status, RunStatus::Succeeded);
    assert_eq!(
        result.error.as_ref().map(|error| error.code.as_str()),
        Some("LOOP.MAX_ITERATIONS_EXCEEDED")
    );
}

#[test]
fn a_return_inside_a_loop_ends_the_whole_workflow() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "steps": [
            {
                "id": "loop", "type": "foreach",
                "items": "${{ [1, 2, 3] }}", "as": "item", "max_items": 10,
                "steps": [{"id": "stop", "type": "return", "value": "${{ item }}"}]
            },
            {"id": "never", "type": "action", "uses": "fixture.ping@1", "with": {}}
        ]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(
        provider.calls(),
        0,
        "a return exits the loop and the workflow"
    );
}

#[test]
fn a_return_step_ends_the_workflow_early() {
    let provider = Fake::echo("fixture", &["ping"]);
    let workflow = descriptor(json!({
        "steps": [
            {"id": "stop", "type": "return", "value": 1},
            {"id": "never", "type": "action", "uses": "fixture.ping@1", "with": {}}
        ]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.calls(), 0);
}

#[test]
fn an_object_return_is_the_run_output() {
    let workflow = descriptor(json!({
        "outputs": {"ignored": {"value": false}},
        "steps": [{"id": "done", "type": "return", "value": {"decision": "respond"}}]
    }));

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(
        result.outputs,
        Map::from_iter([("ignored".into(), json!(false))])
    );
    assert_eq!(result.return_value, Some(json!({"decision": "respond"})));
    assert_eq!(result.summary()["output"], json!({"decision": "respond"}));
    assert_eq!(result.summary()["outputs"], json!({"ignored": false}));
}

#[test]
fn a_fail_step_produces_its_declared_error() {
    let workflow = descriptor(json!({
        "steps": [{
            "id": "stop", "type": "fail",
            "error": {"code": "CUSTOM.STOP", "message": "halted on purpose"}
        }]
    }));

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Failed);
    let error = result.error.unwrap();
    assert_eq!(error.code, "CUSTOM.STOP");
    assert_eq!(error.message, "halted on purpose");
}

// ---------------------------------------------------------------------------
// Dependency ordering
// ---------------------------------------------------------------------------

#[test]
fn steps_execute_in_dependency_order_not_declaration_order() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let recorder = order.clone();
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["mark"]),
        Box::new(move |_, args, _| {
            recorder
                .lock()
                .unwrap()
                .push(args["label"].as_str().unwrap_or_default().to_string());
            Ok(json!({}))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [
            {
                "id": "last", "type": "action", "uses": "fixture.mark@1",
                "with": {"label": "last"}, "depends_on": ["middle"]
            },
            {
                "id": "first", "type": "action", "uses": "fixture.mark@1",
                "with": {"label": "first"}, "depends_on": []
            },
            {
                "id": "middle", "type": "action", "uses": "fixture.mark@1",
                "with": {"label": "middle"}, "depends_on": ["first"]
            }
        ]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(
        *order.lock().unwrap(),
        vec![
            "first".to_string(),
            "middle".to_string(),
            "last".to_string()
        ]
    );
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

#[test]
fn a_retryable_failure_is_retried_up_to_the_limit_and_can_succeed() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["flaky"]),
        Box::new(|_, _, attempt| {
            if attempt < 3 {
                Err(AutomationError::new("FIXTURE.FLAKY", "transient failure")
                    .with_retryable(true)
                    .with_effect("not_applied"))
            } else {
                Ok(json!({"attempt": attempt}))
            }
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.flaky@1", "with": {},
            "retry": {"max_attempts": 5, "backoff": {"strategy": "fixed", "initial_delay": "1ms"}}
        }],
        "outputs": {"attempt": {"value": "${{ steps.act.output.attempt }}"}}
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.calls(), 3);
    assert_eq!(result.outputs["attempt"], json!(3));
}

#[test]
fn a_non_retryable_failure_is_attempted_exactly_once() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["hard"]),
        Box::new(|_, _, _| {
            Err(AutomationError::new("FIXTURE.HARD", "permanent")
                .with_retryable(false)
                .with_effect("not_applied"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.hard@1", "with": {},
            "retry": {"max_attempts": 5, "backoff": {"strategy": "fixed", "initial_delay": "1ms"}}
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(provider.calls(), 1);
}

#[test]
fn retry_stops_after_exhausting_its_attempts() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["always"]),
        Box::new(|_, _, _| {
            Err(AutomationError::new("FIXTURE.ALWAYS", "still failing")
                .with_retryable(true)
                .with_effect("not_applied"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.always@1", "with": {},
            "retry": {"max_attempts": 3, "backoff": {"strategy": "fixed", "initial_delay": "1ms"}}
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(provider.calls(), 3);
}

#[test]
fn a_non_idempotent_action_with_an_unknown_effect_is_never_auto_retried() {
    let provider = Fake::build(
        "fixture",
        actions_with_effect(&["charge"], "non_idempotent"),
        Box::new(|_, _, _| {
            // The classic ambiguous case: the request went out, the reply did not.
            Err(AutomationError::new("PLUGIN.HOST_TIMEOUT", "no reply")
                .with_retryable(true)
                .with_effect("unknown"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "pay", "type": "action", "uses": "fixture.charge@1", "with": {},
            "effect": {"class": "non_idempotent"},
            "retry": {"max_attempts": 5, "backoff": {"strategy": "fixed", "initial_delay": "1ms"}}
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(
        provider.calls(),
        1,
        "an unprovable non-idempotent action must not be repeated"
    );
    assert_eq!(result.status, RunStatus::UnknownEffect);
}

#[test]
fn an_idempotent_action_with_an_unknown_effect_may_be_retried() {
    let provider = Fake::build(
        "fixture",
        actions_with_effect(&["put"], "idempotent"),
        Box::new(|_, _, attempt| {
            if attempt < 2 {
                Err(AutomationError::new("PLUGIN.HOST_TIMEOUT", "no reply")
                    .with_retryable(true)
                    .with_effect("unknown"))
            } else {
                Ok(json!({"ok": true}))
            }
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "put", "type": "action", "uses": "fixture.put@1", "with": {},
            "effect": {"class": "idempotent"},
            "retry": {"max_attempts": 3, "backoff": {"strategy": "fixed", "initial_delay": "1ms"}}
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.calls(), 2);
}

#[test]
fn retry_only_applies_to_matching_error_codes() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["mixed"]),
        Box::new(|_, _, _| {
            Err(AutomationError::new("OTHER.CODE", "not matched")
                .with_retryable(true)
                .with_effect("not_applied"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.mixed@1", "with": {},
            "retry": {
                "max_attempts": 4,
                "backoff": {"strategy": "fixed", "initial_delay": "1ms"},
                "on": {"codes": ["FIXTURE.*"]}
            }
        }]
    }));

    execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(provider.calls(), 1);
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[test]
fn an_on_error_handler_can_absorb_a_failure_and_continue() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["broken", "after"]),
        Box::new(|action, _, _| {
            if action.contains("broken") {
                Err(AutomationError::new("FIXTURE.BROKEN", "failed").with_effect("not_applied"))
            } else {
                Ok(json!({"ran": true}))
            }
        }),
    );

    let workflow = descriptor(json!({
        "steps": [
            {
                "id": "act", "type": "action", "uses": "fixture.broken@1", "with": {},
                "on_error": {
                    "match": {"codes": ["FIXTURE.*"]},
                    "steps": [],
                    "outcome": {"mode": "continue", "output": {"recovered": true}}
                }
            },
            {"id": "after", "type": "action", "uses": "fixture.after@1", "with": {}}
        ],
        "outputs": {"recovered": {"value": "${{ steps.act.output.recovered }}"}}
    }));

    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.outputs["recovered"], json!(true));
}

#[test]
fn a_handler_can_bind_the_error_and_inspect_its_code() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["broken"]),
        Box::new(|_, _, _| {
            Err(AutomationError::new("FIXTURE.BROKEN", "the reason").with_effect("not_applied"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.broken@1", "with": {},
            "on_error": {
                "as": "failure",
                "steps": [],
                "outcome": {
                    "mode": "continue",
                    "output": {"code": "${{ failure.code }}", "message": "${{ failure.message }}"}
                }
            }
        }],
        "outputs": {"code": {"value": "${{ steps.act.output.code }}"}}
    }));

    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.outputs["code"], json!("FIXTURE.BROKEN"));
}

#[test]
fn a_handler_that_does_not_match_leaves_the_error_intact() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["broken"]),
        Box::new(|_, _, _| {
            Err(AutomationError::new("OTHER.CODE", "unmatched").with_effect("not_applied"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.broken@1", "with": {},
            "on_error": {
                "match": {"codes": ["FIXTURE.*"]},
                "steps": [],
                "outcome": {"mode": "continue"}
            }
        }]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "OTHER.CODE");
}

#[test]
fn a_rethrow_handler_runs_its_steps_and_still_fails() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["broken", "log"]),
        Box::new(|action, _, _| {
            if action.contains("broken") {
                Err(AutomationError::new("FIXTURE.BROKEN", "failed").with_effect("not_applied"))
            } else {
                Ok(json!({}))
            }
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.broken@1", "with": {},
            "on_error": {
                "steps": [{"id": "log", "type": "action", "uses": "fixture.log@1", "with": {}}],
                "outcome": {"mode": "rethrow"}
            }
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "FIXTURE.BROKEN");
    assert_eq!(provider.calls(), 2, "the handler's own steps still run");
}

#[test]
fn the_error_location_records_the_failing_step() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["broken"]),
        Box::new(|_, _, _| {
            Err(AutomationError::new("FIXTURE.BROKEN", "failed").with_effect("not_applied"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{"id": "the_step", "type": "action", "uses": "fixture.broken@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![provider]));
    let error = result.error.unwrap();

    assert_eq!(error.location.step_id.as_deref(), Some("the_step"));
    assert_eq!(error.location.workflow.as_deref(), Some("test.workflow"));
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

#[test]
fn a_finally_block_runs_after_success() {
    let provider = Fake::echo("fixture", &["work", "cleanup"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "outer", "type": "block",
            "steps": [{"id": "work", "type": "action", "uses": "fixture.work@1", "with": {}}],
            "finally": [{"id": "cleanup", "type": "action", "uses": "fixture.cleanup@1", "with": {}}]
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(provider.calls(), 2);
}

#[test]
fn a_finally_block_still_runs_after_failure() {
    let cleaned = Arc::new(AtomicUsize::new(0));
    let counter = cleaned.clone();
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["work", "cleanup"]),
        Box::new(move |action, _, _| {
            if action.contains("cleanup") {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(json!({}))
            } else {
                Err(AutomationError::new("FIXTURE.BROKEN", "failed").with_effect("not_applied"))
            }
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "outer", "type": "block",
            "steps": [{"id": "work", "type": "action", "uses": "fixture.work@1", "with": {}}],
            "finally": [{"id": "cleanup", "type": "action", "uses": "fixture.cleanup@1", "with": {}}]
        }]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(cleaned.load(Ordering::SeqCst), 1, "cleanup must always run");
}

#[test]
fn a_cleanup_failure_is_suppressed_onto_the_original_error() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["work", "cleanup"]),
        Box::new(|action, _, _| {
            let code = if action.contains("cleanup") {
                "FIXTURE.CLEANUP_FAILED"
            } else {
                "FIXTURE.ORIGINAL"
            };
            Err(AutomationError::new(code, "failed").with_effect("not_applied"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "outer", "type": "block",
            "steps": [{"id": "work", "type": "action", "uses": "fixture.work@1", "with": {}}],
            "finally": [{"id": "cleanup", "type": "action", "uses": "fixture.cleanup@1", "with": {}}]
        }]
    }));

    let result = execute(&workflow, registry(vec![provider]));
    let error = result.error.expect("an error is reported");

    // The real cause must survive; the cleanup failure rides along.
    assert_eq!(error.code, "FIXTURE.ORIGINAL");
    assert_eq!(error.suppressed.len(), 1);
    assert_eq!(error.suppressed[0].code, "FIXTURE.CLEANUP_FAILED");
}

#[test]
fn a_workflow_level_finally_runs_on_the_failure_path() {
    let cleaned = Arc::new(AtomicUsize::new(0));
    let counter = cleaned.clone();
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["work", "cleanup"]),
        Box::new(move |action, _, _| {
            if action.contains("cleanup") {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(json!({}))
            } else {
                Err(AutomationError::new("FIXTURE.BROKEN", "failed").with_effect("not_applied"))
            }
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{"id": "work", "type": "action", "uses": "fixture.work@1", "with": {}}],
        "finally": [{"id": "cleanup", "type": "action", "uses": "fixture.cleanup@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
}

#[test]
fn a_workflow_finally_failure_on_success_has_a_stable_wrapper() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["cleanup"]),
        Box::new(|_, _, _| {
            Err(
                AutomationError::new("FIXTURE.CLEANUP_FAILED", "cleanup failed")
                    .with_effect("not_applied"),
            )
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{"id": "work", "type": "return", "value": null}],
        "finally": [{"id": "cleanup", "type": "action", "uses": "fixture.cleanup@1", "with": {}}]
    }));

    let result = execute(&workflow, registry(vec![provider]));
    let error = result.error.expect("cleanup failure");

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(error.code, "WORKFLOW.FINALLY_FAILED");
    assert_eq!(error.phase.as_deref(), Some("cleanup"));
    assert_eq!(error.cause.unwrap().code, "FIXTURE.CLEANUP_FAILED");
}

// ---------------------------------------------------------------------------
// Budgets, effects and cancellation
// ---------------------------------------------------------------------------

#[test]
fn the_step_budget_stops_a_runaway_workflow() {
    let workflow = {
        let mut document = json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "test.budget"},
            "budgets": {"max_duration": "30s", "max_executed_steps": 3},
            "variables": {"count": {"schema": {"type": "integer"}, "mutable": true, "initial": 0}},
            "steps": [{
                "id": "loop", "type": "while",
                "condition": "${{ True }}", "max_iterations": 1000, "timeout": "10s",
                "steps": [{"id": "bump", "type": "set", "assign": {"vars.count": "${{ vars.count + 1 }}"}}]
            }]
        });
        document["metadata"]["name"] = json!("test.budget");
        compile_descriptor(document, None).expect("descriptor compiles")
    };

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().code, "WORKFLOW.STEP_BUDGET_EXCEEDED");
    assert!(result.executed_steps <= 4);
}

#[test]
fn the_wall_clock_budget_ends_a_slow_run() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["slow"]),
        Box::new(|_, _, _| {
            std::thread::sleep(Duration::from_millis(120));
            Ok(json!({}))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "loop", "type": "while",
            "condition": "${{ True }}", "max_iterations": 1000, "timeout": "30s",
            "steps": [{"id": "act", "type": "action", "uses": "fixture.slow@1", "with": {}}]
        }]
    }));

    let started = std::time::Instant::now();
    let result = run(
        &workflow,
        RunOptions {
            providers: registry(vec![provider]),
            max_duration: Some(Duration::from_millis(300)),
            ..Default::default()
        },
    );

    assert_eq!(result.status, RunStatus::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the budget must actually stop the run"
    );
}

#[test]
fn cancellation_stops_the_run_and_is_reported_as_cancelled() {
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancel.clone();
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["tick"]),
        Box::new(move |_, _, attempt| {
            if attempt >= 2 {
                flag.store(true, Ordering::SeqCst);
            }
            Ok(json!({}))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "loop", "type": "while",
            "condition": "${{ True }}", "max_iterations": 1000, "timeout": "30s",
            "steps": [{"id": "act", "type": "action", "uses": "fixture.tick@1", "with": {}}]
        }]
    }));

    let result = run(
        &workflow,
        RunOptions {
            providers: registry(vec![provider.clone()]),
            cancel,
            ..Default::default()
        },
    );

    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(
        provider.calls() < 10,
        "cancellation must take effect promptly"
    );
}

#[test]
fn a_read_only_action_never_reports_an_unknown_effect() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["look"]),
        Box::new(|_, _, _| {
            // Even an ambiguous transport failure cannot have changed anything.
            Err(AutomationError::new("PLUGIN.HOST_TIMEOUT", "no reply").with_effect("unknown"))
        }),
    );

    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.look@1", "with": {},
            "effect": {"class": "read_only"}
        }]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.error.unwrap().effect, "not_applied");
}

#[test]
fn a_precondition_failure_prevents_the_action_from_running() {
    let provider = Fake::echo("fixture", &["act"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "precondition": {"condition": "${{ False }}", "message": "not ready"}
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Failed);
    let error = result.error.unwrap();
    assert_eq!(error.code, "ACTION.PRECONDITION_FAILED");
    assert_eq!(error.effect, "not_applied");
    assert_eq!(provider.calls(), 0);
}

#[test]
fn a_postcondition_failure_reports_an_unknown_effect() {
    let provider = Fake::echo("fixture", &["act"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {"condition": "${{ False }}", "message": "did not settle"}
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    // The action ran; only the verification failed, so the effect is unknown.
    assert_eq!(provider.calls(), 1);
    assert_eq!(result.status, RunStatus::UnknownEffect);
    assert_eq!(result.error.unwrap().code, "ACTION.POSTCONDITION_FAILED");
}

#[test]
fn a_postcondition_observation_is_dispatched_and_readable() {
    // The point of `observe`: the condition judges freshly read state, not the
    // output the action already recorded. Measured against the real binary
    // before this existed -- an `observe` naming a provider nobody offers still
    // reported `succeeded`, which means it was never dispatched at all.
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["act", "look"]),
        Box::new(|action, _, _| {
            if action == "fixture.look@1" {
                Ok(json!({"settled": true}))
            } else {
                Ok(json!({"dispatched": true}))
            }
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {
                "condition": "${{ observation.settled }}",
                "observe": {"uses": "fixture.look@1", "with": {}}
            }
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded, "{:?}", result.error);
    // Two calls: the action, then the observation.
    assert_eq!(provider.calls(), 2);
}

#[test]
fn a_postcondition_polls_until_the_ui_catches_up() {
    // Desktop UI settles asynchronously: a dialog appears a moment after the
    // click. Without polling, every such assertion fails on a race and the
    // feature is unusable for the thing it exists for.
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["act", "look"]),
        Box::new(|action, _, attempt| {
            if action == "fixture.look@1" {
                // Not ready on the first look, ready on the second.
                Ok(json!({"settled": attempt >= 3}))
            } else {
                Ok(json!({"dispatched": true}))
            }
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {
                "condition": "${{ observation.settled }}",
                "observe": {"uses": "fixture.look@1", "with": {}},
                "timeout": "5s",
                "poll_interval": "10ms"
            }
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded, "{:?}", result.error);
    assert!(
        provider.calls() >= 3,
        "the observation must be retried, saw {} call(s)",
        provider.calls()
    );
}

#[test]
fn a_postcondition_that_never_holds_gives_up_at_its_timeout() {
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["act", "look"]),
        Box::new(|_, _, _| Ok(json!({"settled": false}))),
    );
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {
                "condition": "${{ observation.settled }}",
                "observe": {"uses": "fixture.look@1", "with": {}},
                "timeout": "300ms",
                "poll_interval": "10ms",
                "message": "the dialog never appeared"
            }
        }]
    }));

    let started = std::time::Instant::now();
    let result = execute(&workflow, registry(vec![provider]));
    let elapsed = started.elapsed();

    assert_eq!(result.status, RunStatus::UnknownEffect);
    let error = result.error.expect("a failing postcondition must report");
    assert_eq!(error.code, "ACTION.POSTCONDITION_FAILED");
    assert_eq!(error.message, "the dialog never appeared");
    // What was actually seen, so the failure can be diagnosed without a rerun.
    assert_eq!(error.details["last_observation"], json!({"settled": false}));
    assert!(
        elapsed >= std::time::Duration::from_millis(250),
        "must wait out its timeout, gave up after {elapsed:?}"
    );
}

#[test]
fn a_postcondition_without_a_timeout_is_judged_once() {
    // Waiting when no wait was requested would turn a fast, clear failure into
    // a slow one. Polling has to be opted into.
    let provider = Fake::echo("fixture", &["act"]);
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {"condition": "${{ False }}"}
        }]
    }));

    let started = std::time::Instant::now();
    let result = execute(&workflow, registry(vec![provider]));

    assert_eq!(result.status, RunStatus::UnknownEffect);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(200),
        "must fail immediately without a timeout"
    );
}

#[test]
fn a_postcondition_waits_through_an_observation_that_is_not_ready_yet() {
    // The commonest assertion of all is "after clicking, a dialog appears".
    // While waiting, the observation legitimately fails: the thing is not there
    // yet. Measured against the real binary, this aborted after 0.20s of a 3s
    // window, which made the timeout unreachable and the feature useless for
    // the case it exists to serve.
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["act", "look"]),
        Box::new(|action, _, attempt| {
            if action != "fixture.look@1" {
                return Ok(json!({"dispatched": true}));
            }
            if attempt < 4 {
                // Exactly what the driver reports for an element that has not
                // appeared yet.
                Err(
                    AutomationError::new("DRIVER.NOT_FOUND", "no element matched")
                        .with_retryable(true)
                        .with_effect("not_applied"),
                )
            } else {
                Ok(json!({"match_count": 1}))
            }
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {
                "condition": "${{ observation.match_count > 0 }}",
                "observe": {"uses": "fixture.look@1", "with": {}},
                "timeout": "5s",
                "poll_interval": "10ms"
            }
        }]
    }));

    let result = execute(&workflow, registry(vec![provider.clone()]));

    assert_eq!(result.status, RunStatus::Succeeded, "{:?}", result.error);
    assert!(
        provider.calls() >= 4,
        "must keep looking, saw {}",
        provider.calls()
    );
}

#[test]
fn an_observation_that_cannot_recover_fails_the_postcondition_at_once() {
    // The counterpart to waiting: a failure that will not fix itself must not
    // be retried until the timeout, or every genuine misconfiguration turns
    // into a slow one. `retryable` is what separates the two.
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["act", "look"]),
        Box::new(|action, _, _| {
            if action != "fixture.look@1" {
                return Ok(json!({"dispatched": true}));
            }
            Err(
                AutomationError::new("DRIVER.INVALID_REQUEST", "locator is malformed")
                    .with_effect("not_applied"),
            )
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {
                "condition": "${{ observation.match_count > 0 }}",
                "observe": {"uses": "fixture.look@1", "with": {}},
                "timeout": "5s",
                "poll_interval": "10ms"
            }
        }]
    }));

    let started = std::time::Instant::now();
    let result = execute(&workflow, registry(vec![provider]));

    let error = result.error.expect("a broken observation must be reported");
    assert_eq!(error.code, "DRIVER.INVALID_REQUEST");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "a permanent failure must not be retried until the timeout"
    );
}

#[test]
fn an_assertion_that_never_observed_anything_reports_why() {
    // When every round failed there is no observation to show, and a bare
    // "condition not satisfied" against a null would hide the actual cause.
    let provider = Fake::build(
        "fixture",
        read_only_actions(&["act", "look"]),
        Box::new(|action, _, _| {
            if action != "fixture.look@1" {
                return Ok(json!({"dispatched": true}));
            }
            Err(
                AutomationError::new("DRIVER.WINDOW_NOT_FOUND", "no window matched")
                    .with_retryable(true)
                    .with_effect("not_applied"),
            )
        }),
    );
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {
                "condition": "${{ observation.match_count > 0 }}",
                "observe": {"uses": "fixture.look@1", "with": {}},
                "timeout": "100ms",
                "poll_interval": "10ms"
            }
        }]
    }));

    let result = execute(&workflow, registry(vec![provider]));

    let error = result.error.expect("the assertion must fail");
    assert_eq!(error.code, "ACTION.POSTCONDITION_FAILED");
    assert_eq!(
        error.details["last_observation_error"]["code"],
        json!("DRIVER.WINDOW_NOT_FOUND"),
        "the reason it kept failing is the actual diagnosis"
    );
}

#[test]
fn a_postcondition_cannot_observe_with_a_writing_action() {
    // An assertion that changes what it is checking establishes nothing, and a
    // write smuggled in here would also skip the risk and confirmation checks
    // that a real action step goes through.
    let reader = Fake::echo("fixture", &["act"]);
    let writer = Fake::build(
        "writer",
        actions_with_effect(&["press"], "non_idempotent"),
        Box::new(|_, _, _| Ok(json!({"pressed": true}))),
    );
    let workflow = descriptor(json!({
        "steps": [{
            "id": "act", "type": "action", "uses": "fixture.act@1", "with": {},
            "postcondition": {
                "condition": "${{ True }}",
                "observe": {"uses": "writer.press@1", "with": {}}
            }
        }]
    }));

    let result = execute(&workflow, registry(vec![reader, writer.clone()]));

    let error = result.error.expect("a writing observation must be refused");
    assert_eq!(error.code, "POLICY.DENIED");
    assert_eq!(error.effect, "not_applied");
    assert_eq!(
        writer.calls(),
        0,
        "the refusal must happen before the write is dispatched"
    );
}

// ---------------------------------------------------------------------------
// Script steps
// ---------------------------------------------------------------------------

#[test]
fn a_script_step_runs_and_its_output_is_readable_by_later_steps() {
    if aad_runtime::script::availability()["state"] == "unavailable" {
        return;
    }
    let workflow = descriptor(json!({
        "steps": [{
            "id": "compute", "type": "script",
            "runtime": "python",
            "output_schema": {"type": "object"},
            "source": "import json,sys; data=json.load(sys.stdin); \
    print(json.dumps({'total': data['a'] + data['b']}))",
            "inputs": {"a": 20, "b": 22}
        }],
        "outputs": {"total": {"value": "${{ steps.compute.output.total }}"}}
    }));

    let result = execute_trusting_scripts(&workflow);

    assert_eq!(result.status, RunStatus::Succeeded, "{:?}", result.error);
    assert_eq!(result.outputs["total"], json!(42));
}

#[test]
fn a_failing_script_step_fails_the_workflow() {
    if aad_runtime::script::availability()["state"] == "unavailable" {
        return;
    }
    let workflow = descriptor(json!({
        "steps": [{
            "id": "boom", "type": "script",
            "runtime": "python",
            "output_schema": {"type": "object"},
            "source": "raise SystemExit(4)"
        }]
    }));

    let result = execute_trusting_scripts(&workflow);

    assert_ne!(result.status, RunStatus::Succeeded);
    assert_eq!(
        result.error.as_ref().map(|error| error.code.as_str()),
        Some("SCRIPT.EXIT_NONZERO")
    );
}

#[test]
fn a_script_step_is_refused_unless_the_caller_opted_in() {
    // A descriptor is data. Running one must not be enough to make this
    // process execute code the caller never agreed to run.
    let workflow = descriptor(json!({
        "steps": [{
            "id": "exfiltrate", "type": "script",
            "runtime": "python",
            "output_schema": {"type": "object"},
            "source": "import json; print(json.dumps({'ran': True}))"
        }],
        "outputs": {"ran": {"value": "${{ steps.exfiltrate.output.ran }}"}}
    }));

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Failed);
    let error = result.error.expect("the refusal is reported");
    assert_eq!(error.code, "SCRIPT.SANDBOX_DENIED");
    // Nothing ran, so the refusal must not be reported as ambiguous: an
    // `unknown` effect would send an operator hunting for side effects.
    assert_eq!(error.effect, "not_applied");
    assert!(
        error.details.contains_key("remedy"),
        "the refusal must say how to proceed deliberately"
    );
    assert!(result.outputs.is_empty());
}

#[test]
fn a_refused_script_never_reaches_the_interpreter() {
    // The refusal has to happen before interpreting the body. Use a program
    // that would return an unmistakable value when allowed; writing a marker
    // into the host's temp directory is not a valid control because the Linux
    // sandbox deliberately gives the script a private, empty /tmp.
    if aad_runtime::script::availability()["state"] == "unavailable" {
        return;
    }

    let body = json!({
        "steps": [{
            "id": "mark", "type": "script",
            "runtime": "python",
            "output_schema": {"type": "object"},
            "source": "import json; print(json.dumps({'ran': True}))"
        }],
        "outputs": {"ran": {"value": "${{ steps.mark.output.ran }}"}}
    });

    let refused = execute(&descriptor(body.clone()), registry(vec![]));

    assert_eq!(
        refused.error.as_ref().map(|error| error.code.as_str()),
        Some("SCRIPT.SANDBOX_DENIED")
    );
    assert!(
        refused.outputs.is_empty(),
        "a refused script produced output"
    );

    // And the same descriptor *does* leave the mark once the caller opts in,
    // so the assertion above is really observing the gate rather than a
    // script that could never have written the file anyway.
    let allowed = execute_trusting_scripts(&descriptor(body));
    assert_eq!(allowed.status, RunStatus::Succeeded, "{:?}", allowed.error);
    assert_eq!(allowed.outputs["ran"], json!(true));
}

#[test]
fn the_default_run_options_deny_scripts() {
    // Stated as its own fact: every caller that does not think about this
    // gets the safe behaviour, including future ones.
    assert!(!RunOptions::default().allow_scripts);
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn the_plan_digest_is_stable_for_the_same_descriptor() {
    let workflow = descriptor(json!({
        "steps": [{"id": "done", "type": "return", "value": 1}]
    }));
    let same = descriptor(json!({
        "steps": [{"id": "done", "type": "return", "value": 1}]
    }));
    let different = descriptor(json!({
        "steps": [{"id": "done", "type": "return", "value": 2}]
    }));

    assert_eq!(
        aad_runtime::plan_digest(&workflow),
        aad_runtime::plan_digest(&same)
    );
    assert_ne!(
        aad_runtime::plan_digest(&workflow),
        aad_runtime::plan_digest(&different)
    );
    assert!(aad_runtime::plan_digest(&workflow).starts_with("sha256:"));
}

#[test]
fn a_run_result_serializes_to_the_run_schema_shape() {
    let workflow = descriptor(json!({
        "steps": [{"id": "done", "type": "return", "value": 1}]
    }));

    let document = execute(&workflow, registry(vec![])).to_json();

    assert_eq!(document["apiVersion"], "ai-auto-desktop.dev/v1alpha1");
    assert_eq!(document["kind"], "Run");
    assert_eq!(document["status"], "succeeded");
    assert!(document["runId"].as_str().is_some());
    assert!(document["workflow"]["planDigest"].as_str().is_some());
    assert!(document["finishedAt"].as_str().is_some());
}

// ---------------------------------------------------------------------------
// requires.runtime
// ---------------------------------------------------------------------------

#[test]
fn a_runtime_requirement_this_build_does_not_satisfy_stops_the_run() {
    // Regression: the range was compiled into the descriptor and then never
    // consulted, so the engine executed steps for a runtime the workflow had
    // explicitly excluded. The Python engine refused the same descriptor, so the
    // compatibility contract held in one implementation and not in the shipped
    // one.
    let workflow = descriptor(json!({
        "requires": {"runtime": ">=99.0.0"},
        "steps": [{"id": "done", "type": "return", "value": "reached"}]
    }));

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Failed);
    let error = result
        .error
        .expect("an unsatisfied runtime range must error");
    assert_eq!(error.code, "DESCRIPTOR.VERSION_UNSUPPORTED");
    // Nothing may run: the point is to stop before the interface is touched.
    assert_eq!(result.executed_steps, 0);
    assert!(
        result.outputs.is_empty(),
        "a refused run must not produce outputs: {:?}",
        result.outputs
    );
}

#[test]
fn a_satisfied_runtime_requirement_runs_normally() {
    let workflow = descriptor(json!({
        "requires": {"runtime": ">=0.0.1"},
        "steps": [{"id": "done", "type": "return", "value": "reached"}]
    }));

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Succeeded);
}

#[test]
fn a_descriptor_without_a_runtime_requirement_is_unaffected() {
    let workflow = descriptor(json!({
        "steps": [{"id": "done", "type": "return", "value": "reached"}]
    }));

    let result = execute(&workflow, registry(vec![]));

    assert_eq!(result.status, RunStatus::Succeeded);
}

#[test]
fn workflow_platform_and_permission_requirements_fail_before_dispatch() {
    let provider = Fake::echo("fixture", &["ping"]);
    let other_platform = if cfg!(target_os = "windows") {
        "linux"
    } else {
        "windows"
    };
    let unsupported = descriptor(json!({
        "requires": {"platforms": [other_platform]},
        "steps": [{"id": "act", "type": "action", "uses": "fixture.ping@1", "with": {}}]
    }));
    let result = execute(&unsupported, registry(vec![provider.clone()]));
    assert_eq!(
        result.error.unwrap().code,
        "CAPABILITY.PLATFORM_UNSUPPORTED"
    );
    assert_eq!(provider.calls(), 0);

    let permission = descriptor(json!({
        "requires": {"permissions": ["desktop.observe"]},
        "steps": [{"id": "act", "type": "action", "uses": "fixture.ping@1", "with": {}}]
    }));
    let denied = execute(&permission, registry(vec![provider.clone()]));
    assert_eq!(denied.error.unwrap().code, "POLICY.DENIED");
    assert_eq!(provider.calls(), 0);

    let allowed = run(
        &permission,
        RunOptions::default()
            .with_providers(registry(vec![provider.clone()]))
            .with_granted_permissions(["desktop.observe"]),
    );
    assert_eq!(allowed.status, RunStatus::Succeeded, "{:?}", allowed.error);
    assert_eq!(provider.calls(), 1);
}

#[test]
fn action_input_and_output_schemas_are_enforced_around_dispatch() {
    let input_provider = Fake::build(
        "fixture",
        json!({
            "typed": {
                "contract_major": 1,
                "effect": {"default_class": "read_only"},
                "input_schema": {"type": "object", "required": ["value"], "properties": {"value": {"type": "string"}}},
                "output_schema": {"type": "object", "required": ["ok"], "properties": {"ok": {"type": "boolean"}}}
            }
        }),
        Box::new(|_, _, _| Ok(json!({"ok": true}))),
    );
    let invalid_input = descriptor(json!({
        "steps": [{"id": "act", "type": "action", "uses": "fixture.typed@1", "with": {"value": 7}}]
    }));
    let result = execute(&invalid_input, registry(vec![input_provider.clone()]));
    assert_eq!(result.error.unwrap().code, "ACTION.INPUT_INVALID");
    assert_eq!(input_provider.calls(), 0);

    let output_provider = Fake::build(
        "fixture",
        json!({
            "typed": {
                "contract_major": 1,
                "effect": {"default_class": "read_only"},
                "input_schema": {"type": "object"},
                "output_schema": {"type": "object", "required": ["ok"], "properties": {"ok": {"type": "boolean"}}}
            }
        }),
        Box::new(|_, _, _| Ok(json!({"ok": "not-a-boolean"}))),
    );
    let invalid_output = descriptor(json!({
        "steps": [{"id": "act", "type": "action", "uses": "fixture.typed@1", "with": {}}]
    }));
    let result = execute(&invalid_output, registry(vec![output_provider.clone()]));
    assert_eq!(result.error.unwrap().code, "ACTION.OUTPUT_INVALID");
    assert_eq!(output_provider.calls(), 1);
}

#[test]
fn every_tracked_example_is_accepted_by_the_runtime_it_declares() {
    // The examples declare a range against the shipped RUNTIME_VERSION, so a
    // version bump that forgets them must fail here rather than at the moment
    // someone runs one.
    for range in [">=0.0.1", "^0.0.1"] {
        assert!(
            aad_runtime::version::matches(aad_runtime::RUNTIME_VERSION, range),
            "the shipped runtime must satisfy {range}, which the tracked examples declare"
        );
    }
}
