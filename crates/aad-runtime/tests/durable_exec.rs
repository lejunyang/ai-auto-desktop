//! Durable execution tests.
//!
//! The claim under test is that a run survives a process restart, so several of
//! these do not merely simulate a crash: they spawn a real child process, kill
//! it mid-run, and reopen the journal from the parent. A simulated crash can
//! only prove the code does what I expected; killing a process proves the
//! *journal on disk* is genuinely sufficient to recover from.

use aad_core::compiler::compile_descriptor;
use aad_core::WorkflowDescriptor;
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::durable::{DesiredState, JournalStore, RunStatus};
use aad_runtime::durable_exec::{
    assert_durable_plan, DurableActionMode, DurableExecutor, DurableOptions, Phase, Stopped,
};
use aad_runtime::provider::{Provider, ProviderRegistry};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct ReadOnlyProvider {
    manifest: CapabilityManifest,
    calls: AtomicUsize,
    failure: Option<aad_core::AutomationError>,
}

impl ReadOnlyProvider {
    fn new() -> Arc<Self> {
        let raw = json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "CapabilityManifest",
            "metadata": {"name": "fixture", "version": "1.0.0"},
            "actions": {
                "read": {
                    "contract_major": 1,
                    "effect": {"default_class": "read_only"},
                    "risk": {"category": "observe", "level": "low"},
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"},
                    "errors": [{"code": "FIXTURE.FAIL", "retryable": false, "effect": "not_applied"}],
                    "sensitivity": {"input": "public", "output": "public", "error": "public"},
                    "durability": {"checkpoint_fields": {
                        "title": {"pointer": "/safe/title", "schema": {"type": "string"}}
                    }}
                }
            }
        });
        Arc::new(Self {
            manifest: manifest::parse(&raw).expect("valid provider"),
            calls: AtomicUsize::new(0),
            failure: None,
        })
    }

    fn failing(code: &str, message: &str) -> Arc<Self> {
        let mut provider = Arc::try_unwrap(Self::new()).ok().expect("new provider");
        provider.failure = Some(
            aad_core::AutomationError::new(code, message)
                .with_retryable(true)
                .with_effect("unknown"),
        );
        Arc::new(provider)
    }
}

impl Provider for ReadOnlyProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    fn invoke(
        &self,
        _action: &str,
        _args: Value,
        _timeout: Option<Duration>,
    ) -> Result<Value, aad_core::AutomationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        Ok(json!({"safe": {"title": "ok"}, "secret": "must-not-persist"}))
    }
}

/// A journal in its own directory, removed on drop.
struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aad-durable-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create directory");
        Self { path }
    }

    fn journal_path(&self) -> std::path::PathBuf {
        self.path.join("journal.sqlite3")
    }

    fn open(&self) -> JournalStore {
        JournalStore::open(self.journal_path()).expect("open journal")
    }

    /// Open with an explicit busy timeout, for tests that deliberately contend
    /// for the write lock and need to control who loses.
    fn open_with_timeout(&self, busy_timeout_ms: u32) -> JournalStore {
        JournalStore::open_with_timeout(self.journal_path(), busy_timeout_ms).expect("open journal")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn compile(raw: Value) -> WorkflowDescriptor {
    compile_descriptor(raw, None).expect("compile descriptor")
}

/// Wrap a workflow body in the envelope the compiler requires.
fn document(name: &str, body: Value) -> Value {
    let mut document = json!({
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": name},
        "budgets": {"max_duration": "60s", "max_executed_steps": 1000},
    });
    for (key, value) in body.as_object().expect("body must be an object") {
        document[key] = value.clone();
    }
    document
}

/// An integer variable a `set` step is allowed to assign.
fn counter(initial: i64) -> Value {
    json!({"schema": {"type": "integer"}, "mutable": true, "initial": initial})
}

/// A workflow of `set` steps: durable-eligible, and each step's effect is
/// visible in the checkpoint so progress can be asserted.
fn counting_workflow(steps: usize) -> Value {
    let body: Vec<Value> = (0..steps)
        .map(|index| {
            json!({
                "id": format!("step{index}"),
                "type": "set",
                "assign": {"vars.count": "${{ vars.count + 1 }}"},
            })
        })
        .collect();
    document(
        "durable.counting",
        json!({
            "variables": {"count": counter(0)},
            "outputs": {"total": {"value": "${{ vars.count }}"}},
            "steps": body,
        }),
    )
}

fn cleanup_workflow() -> WorkflowDescriptor {
    compile(document(
        "durable.cleanup",
        json!({
            "variables": {"count": counter(0)},
            "outputs": {"total": {"value": "${{ vars.count }}"}},
            "steps": [
                {"id": "work", "type": "set", "assign": {"vars.count": 1}}
            ],
            "finally": [
                {"id": "cleanup", "type": "set", "assign": {"vars.count": 7}}
            ]
        }),
    ))
}

fn seed_running_checkpoint(
    store: &JournalStore,
    descriptor: &WorkflowDescriptor,
    checkpoint: &Value,
) {
    let digest = aad_runtime::plan_digest(descriptor);
    store
        .create_run(
            "run-1",
            &descriptor.name,
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let lease = store.claim_owner("run-1", "dead", 1.0, 1.0).expect("claim");
    store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            1.1,
        )
        .expect("start");
    store
        .append_event_with_checkpoint(
            &lease,
            "test.checkpoint",
            &json!({}),
            checkpoint,
            None,
            None,
            1.2,
        )
        .expect("checkpoint");
}

fn read_only_workflow() -> WorkflowDescriptor {
    compile(document(
        "durable.read-only",
        json!({
            "outputs": {"title": {"value": "${{ steps.observe.output.title }}"}},
            "steps": [{
                "id": "observe",
                "type": "action",
                "uses": "fixture.read@1",
                "with": {"query": "public"},
                "effect": {"class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
                "sensitivity": {"input": "public", "output": "public", "error": "public"},
                "checkpoint": {"output": {"mode": "project", "fields": ["title"]}}
            }]
        }),
    ))
}

fn read_only_options(provider: Arc<ReadOnlyProvider>) -> DurableOptions {
    let mut providers = ProviderRegistry::new();
    providers.insert(provider);
    DurableOptions::default()
        .with_providers(providers)
        .with_action_mode(DurableActionMode::ReadOnly)
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!("{}:{}", json!(key), canonical_json(&map[key])))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => other.to_string(),
    }
}

fn digest_json(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(value));
    format!("sha256:{:x}", hasher.finalize())
}

#[test]
fn a_read_only_action_persists_only_its_projection() {
    let temp = TempDir::new("read-only-action");
    let provider = ReadOnlyProvider::new();
    let outcome = DurableExecutor::new(temp.open())
        .start(
            &read_only_workflow(),
            Some("run-1"),
            read_only_options(provider.clone()),
        )
        .expect("the durable observation completes");

    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(outcome.run.output, Some(json!({"title": "ok"})));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let persisted = serde_json::to_string(&outcome.run.to_json()).unwrap();
    assert!(!persisted.contains("must-not-persist"));
    let events = temp.open().list_events("run-1", 0, 100).unwrap();
    assert!(events
        .iter()
        .any(|event| event.event_type == "run.action_intent"));
    assert!(events
        .iter()
        .any(|event| event.event_type == "run.action_dispatch_authorized"));
    assert!(events
        .iter()
        .any(|event| event.event_type == "run.finalization_completed"));
}

#[test]
fn read_only_actions_still_require_explicit_opt_in() {
    let temp = TempDir::new("read-only-denied");
    let provider = ReadOnlyProvider::new();
    let mut providers = ProviderRegistry::new();
    providers.insert(provider.clone());
    let error = DurableExecutor::new(temp.open())
        .start(
            &read_only_workflow(),
            Some("run-1"),
            DurableOptions::default().with_providers(providers),
        )
        .expect_err("the default mode must reject actions");
    assert_eq!(error.code, "DURABLE.UNSUPPORTED_PLAN");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_durable_plugin_timeout_is_redacted_and_reported_as_timed_out() {
    let temp = TempDir::new("read-only-timeout");
    let provider = ReadOnlyProvider::failing("PLUGIN.HOST_TIMEOUT", "secret transport detail");
    let outcome = DurableExecutor::new(temp.open())
        .start(
            &read_only_workflow(),
            Some("run-1"),
            read_only_options(provider.clone()),
        )
        .expect("the provider timeout is a terminal run outcome");

    assert_eq!(outcome.run.status, RunStatus::TimedOut);
    let error = outcome.run.error.expect("timeout has an error");
    assert_eq!(error["code"], "ACTION.TIMEOUT");
    assert!(!error.to_string().contains("secret transport detail"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn an_action_intent_can_be_recovered_and_replayed_once() {
    let temp = TempDir::new("read-only-recovery");
    let workflow = read_only_workflow();
    let provider = ReadOnlyProvider::new();
    let store = temp.open();
    let plan_digest = aad_runtime::plan_digest(&workflow);
    store
        .create_run(
            "run-1",
            &workflow.name,
            &json!({}),
            &workflow.raw,
            None,
            Some(&plan_digest),
            None,
        )
        .unwrap();
    let old = store.claim_owner("run-1", "dead", 1.0, 1.0).unwrap();
    store
        .set_status(
            &old,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            1.1,
        )
        .unwrap();

    let step = &workflow.steps[0];
    let contract = &provider.manifest.actions["read"];
    let projection = json!({
        "mode": "project",
        "fields": ["title"],
        "definitions": {
            "title": {"pointer": "/safe/title", "schema": {"type": "string"}}
        }
    });
    let provider_digest = digest_json(&provider.manifest.raw);
    let contract_digest = digest_json(&contract.raw);
    let projection_digest = digest_json(&projection);
    let binding_digest = digest_json(&json!({
        "uses": "fixture.read@1",
        "input": {"query": "public"},
        "providerDigest": provider_digest,
        "contractDigest": contract_digest,
        "projectionDigest": projection_digest,
    }));
    let deadline = aad_runtime::durable::now_seconds() + 30.0;
    let dispatch_deadline = (deadline * 1000.0) as u64;
    let checkpoint = json!({
        "checkpointVersion": 1,
        "runtimeVersion": aad_runtime::RUNTIME_VERSION,
        "planDigest": plan_digest,
        "phase": "action_intent",
        "deadline": deadline,
        "nextTopLevelIndex": 0,
        "executedSteps": 1,
        "unknownEffect": false,
        "variables": {},
        "steps": {},
        "returned": null,
        "actionIntent": {
            "version": 2,
            "operationId": "recover-operation",
            "stepId": step.id,
            "reservationOrdinal": 1,
            "attempt": 1,
            "dispatchDeadlineEpochMs": dispatch_deadline,
            "providerDigest": provider_digest,
            "contractDigest": contract_digest,
            "projectionDigest": projection_digest,
            "bindingDigest": binding_digest,
        }
    });
    store
        .append_event_with_checkpoint(
            &old,
            "run.action_intent",
            &json!({}),
            &checkpoint,
            Some(RunStatus::Running),
            None,
            1.2,
        )
        .unwrap();
    drop(store);

    let outcome = DurableExecutor::new(temp.open())
        .resume(&workflow, "run-1", read_only_options(provider.clone()))
        .expect("a read-only intent is safe to replay");

    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(outcome.run.output, Some(json!({"title": "ok"})));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------- plan gating

#[test]
fn a_workflow_of_pure_steps_is_durable() {
    assert_durable_plan(&compile(counting_workflow(3))).expect("should be eligible");
}

#[test]
fn action_and_script_steps_are_refused_and_named() {
    // Refused because an interrupted dispatch cannot be proven safe to repeat.
    // Naming the offending steps is what makes the refusal actionable.
    let error = assert_durable_plan(&compile(document(
        "durable.with-action",
        json!({
            "variables": {"a": counter(0)},
            "steps": [
                {"id": "ok", "type": "set", "assign": {"vars.a": 1}},
                {
                    "id": "click", "type": "action", "uses": "desktop.focus@1",
                    "with": {"target": "x"},
                },
            ],
        }),
    )))
    .expect_err("must refuse");

    assert_eq!(error.code, "DURABLE.UNSUPPORTED_PLAN");
    assert_eq!(error.effect, "not_applied");
    let named = error.details.get("unsupportedSteps").expect("named steps");
    assert_eq!(named, &json!(["click"]), "only the action is at fault");
}

#[test]
fn a_sensitive_descriptor_is_refused() {
    let error = assert_durable_plan(&compile(document(
        "durable.secret",
        json!({
            "inputs": {
                "password": {"schema": {"type": "string"}, "required": true, "sensitive": true},
            },
            "variables": {"a": counter(0)},
            "steps": [{"id": "s", "type": "set", "assign": {"vars.a": 1}}],
        }),
    )))
    .expect_err("must refuse");
    assert_eq!(error.code, "DURABLE.SENSITIVE_DESCRIPTOR");
}

#[test]
fn explicit_top_level_dependencies_are_refused() {
    // A single "next index" cannot express an arbitrary top-level ordering, so
    // resuming such a plan could execute the wrong step.
    let error = assert_durable_plan(&compile(document(
        "durable.explicit-deps",
        json!({
            "variables": {"a": counter(0)},
            "steps": [
                {"id": "first", "type": "set", "assign": {"vars.a": 1}},
                {
                    "id": "second", "type": "set", "assign": {"vars.a": 2},
                    "depends_on": ["first"],
                },
            ],
        }),
    )))
    .expect_err("must refuse");
    assert_eq!(error.code, "DURABLE.UNSUPPORTED_PLAN");
}

// ---------------------------------------------------------------- happy path

#[test]
fn a_durable_run_completes_and_records_every_boundary() {
    let temp = TempDir::new("complete");
    let executor = DurableExecutor::new(temp.open());
    let descriptor = compile(counting_workflow(3));

    let outcome = executor
        .start(&descriptor, Some("run-1"), DurableOptions::default())
        .expect("run");

    assert_eq!(outcome.stopped, Stopped::Finished);
    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.run.output.as_ref().expect("output")["total"],
        json!(3)
    );
    // A terminal run holds no lease, so nothing appears to own it.
    assert!(outcome.run.owner_id.is_none());

    let events: Vec<String> = executor
        .journal()
        .list_events("run-1", 0, 100)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    // Every step is bracketed, which is what makes recovery able to tell
    // "inside a step" from "between steps".
    assert_eq!(
        events
            .iter()
            .filter(|e| *e == "run.segment_entered")
            .count(),
        3
    );
    assert_eq!(
        events.iter().filter(|e| *e == "run.segment_exited").count(),
        3
    );
    assert_eq!(events.first().expect("first"), "run.created");
    assert_eq!(events.last().expect("last"), "run.finished");
    assert_eq!(
        &events[events.len() - 4..],
        &[
            "run.finalization_intent",
            "run.finalization_started",
            "run.finalization_completed",
            "run.finished",
        ]
    );
}

#[test]
fn a_failing_step_ends_the_run_as_failed_with_its_error() {
    let temp = TempDir::new("failing");
    let executor = DurableExecutor::new(temp.open());
    let descriptor = compile(document(
        "durable.failing",
        json!({
            "variables": {"a": counter(0)},
            "steps": [
                {"id": "ok", "type": "set", "assign": {"vars.a": 1}},
                {
                    "id": "boom", "type": "fail",
                    "error": {"code": "DEMO.BROKEN", "message": "nope"},
                },
            ],
        }),
    ));

    let outcome = executor
        .start(&descriptor, Some("run-1"), DurableOptions::default())
        .expect("run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    let error = outcome.run.error.as_ref().expect("error");
    assert_eq!(error["code"], json!("DEMO.BROKEN"));
    // A failure carries no output, which the journal enforces independently.
    assert!(outcome.run.output.is_none());
}

#[test]
fn a_run_id_cannot_be_started_twice() {
    let temp = TempDir::new("duplicate");
    let executor = DurableExecutor::new(temp.open());
    let descriptor = compile(counting_workflow(1));
    executor
        .start(&descriptor, Some("run-1"), DurableOptions::default())
        .expect("first");
    let error = executor
        .start(&descriptor, Some("run-1"), DurableOptions::default())
        .expect_err("must refuse");
    assert_eq!(error.code, "JOURNAL.CONFLICT");
}

// ---------------------------------------------------------------- pause/resume

#[test]
fn a_pause_requested_before_execution_stops_the_run_before_any_step() {
    let temp = TempDir::new("pause");
    let descriptor = compile(counting_workflow(4));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);

    // Creating the run separately from executing it is what makes this
    // observable: intent can be recorded before anything is dispatched.
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Pause, None)
        .expect("request pause");

    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .execute(&descriptor, "run-1", DurableOptions::default())
        .expect("honour the pause");

    assert_eq!(outcome.stopped, Stopped::Paused);
    assert_eq!(outcome.run.status, RunStatus::Paused);
    // Paused releases the lease so whoever resumes can claim it at once.
    assert!(outcome.run.owner_id.is_none());

    let events: Vec<String> = store
        .list_events("run-1", 0, 100)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert!(
        !events.iter().any(|event| event == "run.segment_entered"),
        "no step should run when paused up front: {events:?}"
    );
    assert_eq!(events.last().expect("last"), "run.paused");
}

#[test]
fn a_paused_run_resumes_and_completes_from_its_boundary() {
    let temp = TempDir::new("pause-resume");
    let descriptor = compile(counting_workflow(4));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Pause, None)
        .expect("pause");

    let executor = DurableExecutor::new(temp.open());
    let paused = executor
        .execute(&descriptor, "run-1", DurableOptions::default())
        .expect("pause");
    assert_eq!(paused.run.status, RunStatus::Paused);

    // Resume without clearing the pause intent by hand. Asking to resume *is*
    // asking to run, so the executor must clear it: a resume that left the
    // request standing would read it at the first boundary and stop again,
    // leaving the run permanently stuck. (This test previously reset the intent
    // itself, which hid exactly that bug.)
    let finished = executor
        .resume(&descriptor, "run-1", DurableOptions::default())
        .expect("resume");

    assert_eq!(finished.run.status, RunStatus::Succeeded);
    assert_eq!(
        finished.run.desired_state,
        DesiredState::Run,
        "the standing pause request must be cleared, not left to re-trigger"
    );
    // All four steps ran exactly once: pausing before any of them lost nothing.
    assert_eq!(
        finished.run.output.as_ref().expect("output")["total"],
        json!(4)
    );
    let events: Vec<String> = store
        .list_events("run-1", 0, 200)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|e| *e == "run.segment_entered")
            .count(),
        4
    );
    assert!(events.iter().any(|e| e == "run.resumed"));
    // Clearing the intent is recorded, so an operator can see why their pause
    // stopped applying.
    assert!(
        events.iter().any(|e| e == "run.resume_requested"),
        "clearing the pause request must be auditable: {events:?}"
    );
}

#[test]
fn a_resume_does_not_clear_a_pause_on_a_run_it_refuses_to_continue() {
    // A run that cannot be continued must be refused without quietly rewriting
    // the operator's recorded intent on the way out.
    let temp = TempDir::new("refused-keeps-intent");
    let descriptor = compile(counting_workflow(3));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let now = aad_runtime::durable::now_seconds();
    let lease = store.claim_owner("run-1", "dead", 0.4, now).expect("claim");
    store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now,
        )
        .expect("start");
    // Interrupted inside a step: unresumable.
    store
        .append_event_with_checkpoint(
            &lease,
            "run.segment_entered",
            &json!({"stepId": "step1"}),
            &json!({
                "checkpointVersion": 1,
                "planDigest": digest,
                "phase": "in_top_level_step",
                "deadline": now + 300.0,
                "nextTopLevelIndex": 2,
                "variables": {"count": 1},
            }),
            None,
            None,
            now,
        )
        .expect("write");
    store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Pause, None)
        .expect("pause");
    std::thread::sleep(std::time::Duration::from_millis(600));

    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .resume(&descriptor, "run-1", DurableOptions::default())
        .expect("reconcile");

    assert_eq!(outcome.run.status, RunStatus::UnknownEffect);
    assert_eq!(
        outcome.run.desired_state,
        DesiredState::Pause,
        "a refused resume must leave the operator's intent as they set it"
    );
}

#[test]
fn a_pause_requested_partway_stops_at_the_next_boundary_and_keeps_progress() {
    let temp = TempDir::new("pause-partway");
    let descriptor = compile(counting_workflow(6));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");

    // Ask for the pause from another connection while the run is in flight. The
    // executor re-reads intent at every boundary, so it stops at the next one
    // rather than mid-step.
    let control = temp.open();
    let requested = std::thread::spawn(move || {
        // Give the run a moment to get past its first boundary.
        std::thread::sleep(std::time::Duration::from_millis(15));
        control.compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Pause, None)
    });

    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .execute(&descriptor, "run-1", DurableOptions::default())
        .expect("run until paused or finished");
    let control_result = requested.join().expect("control thread");

    // These steps are fast, so there are three real interleavings, not two: the
    // request can land before a boundary, after the run has already committed a
    // terminal status, or -- the awkward one -- while the run is still `running`
    // but past its final boundary, where it is accepted and then legitimately
    // has nothing left to stop. All three are correct outcomes. What must hold
    // in every one of them is that asking to pause never loses or repeats work.
    let entered_segments = || -> usize {
        store
            .list_events("run-1", 0, 500)
            .expect("events")
            .into_iter()
            .filter(|event| event.event_type == "run.segment_entered")
            .count()
    };

    match outcome.run.status {
        RunStatus::Paused => {
            assert_eq!(outcome.stopped, Stopped::Paused);
            assert!(control_result.is_ok(), "the pause is what stopped it");
            let checkpoint = outcome.run.checkpoint.as_ref().expect("checkpoint");
            let done = checkpoint["variables"]["count"].as_u64().expect("count");
            assert_eq!(
                checkpoint["nextTopLevelIndex"].as_u64().expect("index"),
                done,
                "the resume point must match the work actually completed"
            );
            assert_eq!(checkpoint["phase"], json!("between_top_level_steps"));

            // And it really can be resumed to completion from there, without
            // anyone clearing the pause intent by hand: that is `resume`'s job.
            let finished = executor
                .resume(&descriptor, "run-1", DurableOptions::default())
                .expect("resume");
            assert_eq!(finished.run.status, RunStatus::Succeeded);
            assert_eq!(
                finished.run.output.as_ref().expect("output")["total"],
                json!(6),
                "no step was lost or repeated across the pause"
            );
            assert_eq!(entered_segments(), 6, "each step ran exactly once");
        }
        RunStatus::Succeeded => {
            // Whichever way the race went, all six steps had already run, so
            // the run must report all six and have entered each exactly once. A
            // late pause is recorded, never applied retroactively.
            assert_eq!(
                outcome.run.output.as_ref().expect("output")["total"],
                json!(6),
                "a late pause must not cost or duplicate work"
            );
            assert_eq!(entered_segments(), 6, "each step ran exactly once");
            if control_result.is_err() {
                // It arrived after the terminal commit, so it had to be
                // refused: a terminal run accepts no further control.
                assert_eq!(outcome.run.desired_state, DesiredState::Run);
            } else {
                // It arrived while the run was still `running`, so accepting it
                // was correct -- but the body was already past its last
                // boundary. The request stands on the record even though there
                // was nothing left for it to stop.
                assert_eq!(
                    outcome.run.desired_state,
                    DesiredState::Pause,
                    "an accepted-but-too-late request must not be erased"
                );
            }
        }
        other => panic!("unexpected status: {}", other.as_str()),
    }
}

/// Flipping the operator's intent while a run advances must never surface as
/// a failure.
///
/// The interesting window is a few microseconds wide: it opens when the
/// runner reads intent at a boundary and closes when it authorises the next
/// dispatch. A request landing inside it makes the runner lose its dispatch
/// CAS. That is the fencing mechanism working as designed, so it has to stop
/// or finish the run -- never report `JOURNAL.CONFLICT`, which would blame
/// the operator for a request the journal actually granted.
///
/// One request cannot be aimed at a window that narrow, so this hammers it
/// across many boundaries until it lands there.
#[test]
fn flipping_intent_while_a_run_advances_never_surfaces_a_conflict() {
    // A run the storm never manages to interrupt exercises no boundary and
    // proves nothing -- measured, that is roughly half of them. Repeat on
    // fresh runs until one is genuinely interrupted, so the assertions inside
    // always get something to judge.
    for round in 0..12 {
        if storm_round(round) {
            return;
        }
    }
    panic!("the storm never interrupted a run, so nothing about intent was tested");
}

/// One round. Returns whether the storm actually paused the run.
fn storm_round(round: usize) -> bool {
    let temp = TempDir::new(&format!("intent-storm-{round}"));
    let descriptor = compile(counting_workflow(40));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");

    // Open every connection this test needs *before* the storm starts. Opening
    // a journal validates its pragmas, which needs the write lock; doing that
    // under a write storm fails on lock contention and says nothing about
    // intent handling.
    // The runner waits far longer for the write lock than the storm does. A
    // storm that starves the runner *inside* a segment leaves the run
    // genuinely unrecoverable -- a dispatched step whose outcome is unknown is
    // correctly refused rather than silently repeated -- and no retry can undo
    // that. Losing those races must fall on the storm, which is scenery, not on
    // the runner, which is the subject. The flips still land in the same
    // microsecond windows either way.
    let executor = DurableExecutor::new(temp.open_with_timeout(30_000));
    let control = temp.open_with_timeout(50);

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flipping = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            // Both directions, as fast as the journal will take them. Every
            // outcome is legitimate here -- the run may be terminal, or the
            // intent may already be what we are asking for -- so failures are
            // deliberately ignored; the runner's behaviour is what is on test.
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = control.compare_and_set_desired_state(
                    "run-1",
                    DesiredState::Run,
                    DesiredState::Pause,
                    None,
                );
                let _ = control.compare_and_set_desired_state(
                    "run-1",
                    DesiredState::Pause,
                    DesiredState::Run,
                    None,
                );
                // Yield rather than sleep: the window being aimed at is only a
                // few microseconds wide, and sleeping steps right over it.
                std::thread::yield_now();
            }
        })
    };

    // Drive to a terminal state, resuming through every pause the storm causes.
    //
    // The storm is deliberately more aggressive than any real operator, so it
    // can also exhaust SQLite's own write locks. That is this harness competing
    // with itself, not the behaviour under test, so it is retried. What is never
    // tolerated is `JOURNAL.CONFLICT`: that would mean a request the journal
    // granted came back to the caller as an error.
    let attempt = |first: bool| {
        // A storage failure is not necessarily a failure to *start*: the
        // attempt may have moved the run out of `pending` before losing the
        // write lock. Retrying `execute` then reports the run is already
        // running -- a harness bug that looks like a product failure. So once
        // the run has started, retries resume instead.
        let mut starting = first;
        loop {
            let result = if starting {
                executor.execute(&descriptor, "run-1", DurableOptions::default())
            } else {
                executor.resume(&descriptor, "run-1", DurableOptions::default())
            };
            match result {
                Ok(outcome) => return outcome,
                Err(error) => {
                    assert_ne!(
                        error.code, "JOURNAL.CONFLICT",
                        "a granted pause must stop the run, not fail it: {error:?}"
                    );
                    if starting && error.code == "DURABLE.INVALID_STATE" {
                        starting = false;
                        continue;
                    }
                    assert_eq!(
                        error.code, "JOURNAL.STORAGE_FAILED",
                        "unexpected failure: {error:?}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    };

    let mut outcome = attempt(true);
    let mut legs = 1;
    while outcome.stopped == Stopped::Paused {
        assert!(legs < 200, "resuming should make progress, not spin");
        legs += 1;
        outcome = attempt(false);
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    flipping.join().expect("control thread");

    // However many times it stopped and restarted, the work must be exactly
    // right: all forty steps, each dispatched once.
    if outcome.run.status != RunStatus::Succeeded {
        let events = store.list_events("run-1", 0, 5000).expect("events");
        let tail: Vec<String> = events
            .iter()
            .rev()
            .take(12)
            .map(|event| format!("{} {}", event.event_type, event.payload))
            .collect();
        panic!(
            "run ended {:?}\nerror: {:?}\nlegs: {legs}\nlast events (newest first):\n  {}",
            outcome.run.status,
            outcome.run.error,
            tail.join("\n  ")
        );
    }
    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.run.output.as_ref().expect("output")["total"],
        json!(40)
    );
    let entered: Vec<String> = store
        .list_events("run-1", 0, 5000)
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type == "run.segment_entered")
        .map(|event| {
            event.payload["stepId"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let mut unique = entered.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(entered.len(), 40, "every step ran");
    assert_eq!(unique.len(), 40, "and none ran twice: {entered:?}");

    legs > 1
}

#[test]
fn a_cancel_requested_partway_ends_the_run_as_cancelled() {
    let temp = TempDir::new("cancel-partway");
    let descriptor = compile(counting_workflow(6));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Cancel, None)
        .expect("cancel");

    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .execute(&descriptor, "run-1", DurableOptions::default())
        .expect("honour the cancel");

    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    // Cancelled from a known-quiet point, so this is clean rather than
    // UNKNOWN_EFFECT.
    assert_eq!(
        outcome.run.error.as_ref().expect("error")["effect"],
        json!("not_applied")
    );
}

#[test]
fn a_cancel_before_execution_still_runs_workflow_cleanup() {
    let temp = TempDir::new("cancel-before-cleanup");
    let descriptor = cleanup_workflow();
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            &descriptor.name,
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Cancel, None)
        .expect("cancel");

    let outcome = DurableExecutor::new(temp.open())
        .execute(&descriptor, "run-1", DurableOptions::default())
        .expect("honour cancel through durable finalization");

    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    assert_eq!(
        outcome.run.checkpoint.as_ref().unwrap()["variables"]["count"],
        7
    );
    let events = temp.open().list_events("run-1", 0, 100).unwrap();
    assert!(events
        .iter()
        .any(|event| event.event_type == "run.finalization_completed"));
}

#[test]
fn executing_a_run_that_is_not_pending_is_refused() {
    let temp = TempDir::new("not-pending");
    let executor = DurableExecutor::new(temp.open());
    let descriptor = compile(counting_workflow(2));
    executor
        .start(&descriptor, Some("run-1"), DurableOptions::default())
        .expect("run");

    let error = executor
        .execute(&descriptor, "run-1", DurableOptions::default())
        .expect_err("must refuse");
    assert_eq!(error.code, "DURABLE.INVALID_STATE");
}

#[test]
fn resuming_a_terminal_run_is_refused() {
    let temp = TempDir::new("resume-terminal");
    let executor = DurableExecutor::new(temp.open());
    let descriptor = compile(counting_workflow(2));
    executor
        .start(&descriptor, Some("run-1"), DurableOptions::default())
        .expect("run");

    let error = executor
        .resume(&descriptor, "run-1", DurableOptions::default())
        .expect_err("must refuse");
    assert_eq!(error.code, "DURABLE.ALREADY_TERMINAL");
    assert_eq!(error.details["status"], json!("succeeded"));
}

#[test]
fn a_cancel_requested_while_down_is_honoured_without_executing_anything() {
    let temp = TempDir::new("cancel-down");
    let descriptor = compile(counting_workflow(3));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Cancel, None)
        .expect("cancel");

    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .resume(&descriptor, "run-1", DurableOptions::default())
        .expect("honour the cancel");

    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    // Cancelled cleanly: nothing was in flight, so this is not UNKNOWN_EFFECT.
    assert_eq!(
        outcome.run.error.as_ref().expect("error")["code"],
        json!("WORKFLOW.CANCELLED")
    );
    let events: Vec<String> = store
        .list_events("run-1", 0, 100)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert!(
        !events.iter().any(|event| event == "run.segment_entered"),
        "no step should have been dispatched: {events:?}"
    );
}

// ---------------------------------------------------------------- plan drift

#[test]
fn resuming_with_a_different_descriptor_is_refused() {
    let temp = TempDir::new("drift");
    let descriptor = compile(counting_workflow(3));
    let store = temp.open();
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&aad_runtime::plan_digest(&descriptor)),
            None,
        )
        .expect("create");

    // A changed plan against an old checkpoint would execute a step the
    // recorded state never described.
    let changed = compile(counting_workflow(5));
    let executor = DurableExecutor::new(temp.open());
    let error = executor
        .resume(&changed, "run-1", DurableOptions::default())
        .expect_err("must refuse");
    assert_eq!(error.code, "DURABLE.PLAN_MISMATCH");
}

// ---------------------------------------------------------------- checkpoints

#[test]
fn the_checkpoint_records_the_phase_and_survives_reopening() {
    let temp = TempDir::new("checkpoint");
    let executor = DurableExecutor::new(temp.open());
    let descriptor = compile(counting_workflow(2));
    executor
        .start(&descriptor, Some("run-1"), DurableOptions::default())
        .expect("run");

    // Read through a completely separate connection, as a new process would.
    let reopened = JournalStore::open(temp.journal_path()).expect("reopen");
    let run = reopened.get_run("run-1").expect("run");
    let checkpoint = run.checkpoint.as_ref().expect("checkpoint persisted");

    assert_eq!(checkpoint["checkpointVersion"], json!(2));
    assert_eq!(
        checkpoint["planDigest"],
        json!(aad_runtime::plan_digest(&descriptor))
    );
    // The last checkpoint before terminal commit contains the completed
    // finalization result, so recovery can commit without replaying cleanup.
    assert_eq!(checkpoint["phase"], json!(Phase::Finalizing.as_str()));
    assert_eq!(checkpoint["finalization"]["stage"], json!("result"));
    // The absolute deadline is stored so a resume cannot be handed a fresh
    // budget.
    assert!(checkpoint["deadline"].as_f64().expect("deadline") > 0.0);
    assert_eq!(checkpoint["variables"]["count"], json!(2));
}

#[test]
fn a_checkpoint_from_an_unsupported_version_is_refused() {
    let temp = TempDir::new("version");
    let descriptor = compile(counting_workflow(2));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let lease = store
        .claim_owner("run-1", "runner", 60.0, aad_runtime::durable::now_seconds())
        .expect("claim");
    store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            aad_runtime::durable::now_seconds(),
        )
        .expect("start");
    // A checkpoint this build cannot interpret must be refused, not guessed at.
    store
        .append_event_with_checkpoint(
            &lease,
            "run.checkpointed",
            &json!({}),
            &json!({
                "checkpointVersion": 99,
                "planDigest": digest,
                "phase": "between_top_level_steps",
                "deadline": aad_runtime::durable::now_seconds() + 300.0,
            }),
            None,
            None,
            aad_runtime::durable::now_seconds(),
        )
        .expect("write checkpoint");
    store
        .release_owner(&lease, aad_runtime::durable::now_seconds())
        .expect("release");

    let executor = DurableExecutor::new(temp.open());
    let error = executor
        .resume(&descriptor, "run-1", DurableOptions::default())
        .expect_err("must refuse");
    assert_eq!(error.code, "DURABLE.CHECKPOINT_UNSUPPORTED");
}

#[test]
fn a_legacy_v1_boundary_checkpoint_remains_resumable() {
    let temp = TempDir::new("legacy-v1");
    let descriptor = compile(counting_workflow(2));
    let digest = aad_runtime::plan_digest(&descriptor);
    let store = temp.open();
    seed_running_checkpoint(
        &store,
        &descriptor,
        &json!({
            "checkpointVersion": 1,
            "runtimeVersion": aad_runtime::RUNTIME_VERSION,
            "planDigest": digest,
            "phase": "between_top_level_steps",
            "deadline": aad_runtime::durable::now_seconds() + 60.0,
            "nextTopLevelIndex": 1,
            "executedSteps": 1,
            "unknownEffect": false,
            "variables": {"count": 1},
            "steps": {},
            "returned": null,
        }),
    );
    drop(store);

    let outcome = DurableExecutor::new(temp.open())
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovery"),
        )
        .expect("v1 checkpoint remains readable");
    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(outcome.run.output, Some(json!({"total": 2})));
}

#[test]
fn a_finalization_intent_resumes_cleanup_exactly_once() {
    let temp = TempDir::new("finalization-intent");
    let descriptor = cleanup_workflow();
    let digest = aad_runtime::plan_digest(&descriptor);
    let store = temp.open();
    seed_running_checkpoint(
        &store,
        &descriptor,
        &json!({
            "checkpointVersion": 2,
            "runtimeVersion": aad_runtime::RUNTIME_VERSION,
            "planDigest": digest,
            "phase": "finalizing",
            "deadline": aad_runtime::durable::now_seconds() + 60.0,
            "nextTopLevelIndex": 1,
            "executedSteps": 1,
            "unknownEffect": false,
            "variables": {"count": 1},
            "steps": {},
            "returned": null,
            "finalization": {
                "version": 1,
                "stage": "intent",
                "outputSet": true,
                "output": {"total": 1},
                "error": null,
            },
        }),
    );
    drop(store);

    let journal = temp.open();
    let outcome = DurableExecutor::new(journal)
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovery"),
        )
        .expect("cleanup intent is replay-safe");
    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(outcome.run.output, Some(json!({"total": 1})));
    assert_eq!(
        outcome.run.checkpoint.as_ref().unwrap()["variables"]["count"],
        7
    );
    let events = temp.open().list_events("run-1", 0, 100).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "run.finalization_started")
            .count(),
        1
    );
}

#[test]
fn an_explicit_return_survives_finalization_recovery() {
    let temp = TempDir::new("finalization-return");
    let descriptor = cleanup_workflow();
    let digest = aad_runtime::plan_digest(&descriptor);
    seed_running_checkpoint(
        &temp.open(),
        &descriptor,
        &json!({
            "checkpointVersion": 2,
            "runtimeVersion": aad_runtime::RUNTIME_VERSION,
            "planDigest": digest,
            "phase": "finalizing",
            "deadline": aad_runtime::durable::now_seconds() + 60.0,
            "nextTopLevelIndex": 1,
            "executedSteps": 1,
            "unknownEffect": false,
            "variables": {"count": 1},
            "steps": {},
            "returned": {"decision": "respond"},
            "finalization": {
                "version": 2,
                "stage": "intent",
                "outputSet": true,
                "output": {"decision": "respond"},
                "returned": true,
                "error": null,
            },
        }),
    );

    let outcome = DurableExecutor::new(temp.open())
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovery"),
        )
        .expect("return value survives recovery and cleanup");
    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(outcome.run.output, Some(json!({"decision": "respond"})));
}

#[test]
fn a_finalization_started_checkpoint_never_replays_cleanup() {
    let temp = TempDir::new("finalization-started");
    let descriptor = cleanup_workflow();
    let digest = aad_runtime::plan_digest(&descriptor);
    let store = temp.open();
    seed_running_checkpoint(
        &store,
        &descriptor,
        &json!({
            "checkpointVersion": 2,
            "runtimeVersion": aad_runtime::RUNTIME_VERSION,
            "planDigest": digest,
            "phase": "finalizing",
            "deadline": aad_runtime::durable::now_seconds() + 60.0,
            "nextTopLevelIndex": 1,
            "executedSteps": 1,
            "unknownEffect": false,
            "variables": {"count": 1},
            "steps": {},
            "returned": null,
            "finalization": {
                "version": 1,
                "stage": "started",
                "outputSet": true,
                "output": {"total": 1},
                "error": null,
            },
        }),
    );
    drop(store);

    let outcome = DurableExecutor::new(temp.open())
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovery"),
        )
        .expect("started cleanup is reconciled conservatively");
    assert_eq!(outcome.run.status, RunStatus::UnknownEffect);
    assert_eq!(
        outcome.run.error.as_ref().unwrap()["details"]["phase"],
        "finalization_started"
    );
    let events = temp.open().list_events("run-1", 0, 100).unwrap();
    assert!(!events
        .iter()
        .any(|event| event.event_type == "run.finalization_completed"));
}

#[test]
fn a_finalization_result_commits_without_replaying_cleanup() {
    let temp = TempDir::new("finalization-result");
    let descriptor = cleanup_workflow();
    let digest = aad_runtime::plan_digest(&descriptor);
    let store = temp.open();
    seed_running_checkpoint(
        &store,
        &descriptor,
        &json!({
            "checkpointVersion": 2,
            "runtimeVersion": aad_runtime::RUNTIME_VERSION,
            "planDigest": digest,
            "phase": "finalizing",
            "deadline": aad_runtime::durable::now_seconds() + 60.0,
            "nextTopLevelIndex": 1,
            "executedSteps": 2,
            "unknownEffect": false,
            "variables": {"count": 7},
            "steps": {},
            "returned": null,
            "finalization": {
                "version": 1,
                "stage": "result",
                "result": {
                    "status": "succeeded",
                    "output": {"total": 1},
                    "error": null,
                    "executedSteps": 2,
                },
            },
        }),
    );
    drop(store);

    let outcome = DurableExecutor::new(temp.open())
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovery"),
        )
        .expect("completed cleanup only needs terminal commit");
    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(outcome.run.output, Some(json!({"total": 1})));
    let events = temp.open().list_events("run-1", 0, 100).unwrap();
    assert!(!events
        .iter()
        .any(|event| event.event_type == "run.finalization_started"));
    assert_eq!(events.last().unwrap().event_type, "run.finished");
}

#[test]
fn a_finalization_intent_preserves_a_body_failure_through_cleanup() {
    let temp = TempDir::new("finalization-failure");
    let descriptor = cleanup_workflow();
    let digest = aad_runtime::plan_digest(&descriptor);
    let failure = aad_core::AutomationError::new("TEST.FAIL", "body failed")
        .with_effect("not_applied")
        .at_step("work", Some("$.steps[0]"), Some(1), Some(&descriptor.name));
    let store = temp.open();
    seed_running_checkpoint(
        &store,
        &descriptor,
        &json!({
            "checkpointVersion": 2,
            "runtimeVersion": aad_runtime::RUNTIME_VERSION,
            "planDigest": digest,
            "phase": "finalizing",
            "deadline": aad_runtime::durable::now_seconds() + 60.0,
            "nextTopLevelIndex": 1,
            "executedSteps": 1,
            "unknownEffect": false,
            "variables": {"count": 1},
            "steps": {},
            "returned": null,
            "finalization": {
                "version": 1,
                "stage": "intent",
                "outputSet": false,
                "output": null,
                "error": failure.to_json(),
            },
        }),
    );
    drop(store);

    let outcome = DurableExecutor::new(temp.open())
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovery"),
        )
        .expect("body failure remains a terminal result");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(outcome.run.error.as_ref().unwrap()["code"], "TEST.FAIL");
    assert_eq!(
        outcome.run.error.as_ref().unwrap()["location"]["step_id"],
        "work"
    );
    assert_eq!(
        outcome.run.checkpoint.as_ref().unwrap()["variables"]["count"],
        7
    );
}

#[test]
fn a_completed_success_honours_a_sticky_cancel_without_replaying_cleanup() {
    let temp = TempDir::new("finalization-result-cancel");
    let descriptor = cleanup_workflow();
    let digest = aad_runtime::plan_digest(&descriptor);
    let store = temp.open();
    seed_running_checkpoint(
        &store,
        &descriptor,
        &json!({
            "checkpointVersion": 2,
            "runtimeVersion": aad_runtime::RUNTIME_VERSION,
            "planDigest": digest,
            "phase": "finalizing",
            "deadline": aad_runtime::durable::now_seconds() + 60.0,
            "nextTopLevelIndex": 1,
            "executedSteps": 2,
            "unknownEffect": false,
            "variables": {"count": 7},
            "steps": {},
            "returned": null,
            "finalization": {
                "version": 1,
                "stage": "result",
                "result": {
                    "status": "succeeded",
                    "output": {"total": 1},
                    "error": null,
                    "executedSteps": 2,
                },
            },
        }),
    );
    store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Cancel, None)
        .unwrap();
    drop(store);

    let outcome = DurableExecutor::new(temp.open())
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovery"),
        )
        .expect("sticky cancel supersedes a successful result");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    assert!(outcome.run.output.is_none());
    let events = temp.open().list_events("run-1", 0, 100).unwrap();
    assert!(!events
        .iter()
        .any(|event| event.event_type == "run.finalization_started"));
}

#[test]
fn a_malformed_finalization_checkpoint_is_rejected_before_cleanup() {
    let temp = TempDir::new("malformed-finalization");
    let descriptor = cleanup_workflow();
    let digest = aad_runtime::plan_digest(&descriptor);
    let store = temp.open();
    seed_running_checkpoint(
        &store,
        &descriptor,
        &json!({
            "checkpointVersion": 2,
            "runtimeVersion": aad_runtime::RUNTIME_VERSION,
            "planDigest": digest,
            "phase": "finalizing",
            "deadline": aad_runtime::durable::now_seconds() + 60.0,
            "nextTopLevelIndex": 1,
            "executedSteps": 1,
            "unknownEffect": false,
            "variables": {"count": 1},
            "steps": {},
            "returned": null,
            "finalization": {
                "version": 1,
                "stage": "intent",
                "outputSet": true,
                "output": {"total": 1},
                "error": null,
                "unexpected": true,
            },
        }),
    );
    drop(store);

    let error = DurableExecutor::new(temp.open())
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovery"),
        )
        .expect_err("unknown checkpoint fields must fail closed");
    assert_eq!(error.code, "DURABLE.CHECKPOINT_INVALID");
    let current = temp.open().get_run("run-1").unwrap();
    assert_eq!(current.checkpoint.unwrap()["variables"]["count"], 1);
}

#[test]
fn a_current_checkpoint_requires_complete_typed_segment_state() {
    let descriptor = compile(counting_workflow(2));
    let digest = aad_runtime::plan_digest(&descriptor);
    let base = json!({
        "checkpointVersion": 2,
        "runtimeVersion": aad_runtime::RUNTIME_VERSION,
        "planDigest": digest,
        "phase": "between_top_level_steps",
        "deadline": aad_runtime::durable::now_seconds() + 60.0,
        "nextTopLevelIndex": 1,
        "executedSteps": 1,
        "unknownEffect": false,
        "variables": {"count": 1},
        "steps": {},
        "returned": null,
    });

    for (label, field, replacement) in [
        ("missing-executed-steps", "executedSteps", None),
        ("invalid-executed-steps", "executedSteps", Some(json!("1"))),
        ("missing-returned", "returned", None),
    ] {
        let temp = TempDir::new(label);
        let mut checkpoint = base.clone();
        let object = checkpoint.as_object_mut().unwrap();
        if let Some(value) = replacement {
            object.insert(field.into(), value);
        } else {
            object.remove(field);
        }
        seed_running_checkpoint(&temp.open(), &descriptor, &checkpoint);

        let error = DurableExecutor::new(temp.open())
            .resume(
                &descriptor,
                "run-1",
                DurableOptions::default().with_owner_id("recovery"),
            )
            .expect_err("an incomplete current checkpoint must fail closed");
        assert_eq!(error.code, "DURABLE.CHECKPOINT_INVALID", "{label}");
    }
}

#[test]
fn a_checkpoint_without_a_deadline_is_refused_rather_than_treated_as_unlimited() {
    let temp = TempDir::new("no-deadline");
    let descriptor = compile(counting_workflow(2));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let now = aad_runtime::durable::now_seconds();
    let lease = store
        .claim_owner("run-1", "runner", 60.0, now)
        .expect("claim");
    store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now,
        )
        .expect("start");
    store
        .append_event_with_checkpoint(
            &lease,
            "run.checkpointed",
            &json!({}),
            &json!({
                "checkpointVersion": 1,
                "planDigest": digest,
                "phase": "between_top_level_steps",
            }),
            None,
            None,
            now,
        )
        .expect("write");
    store.release_owner(&lease, now).expect("release");

    let executor = DurableExecutor::new(temp.open());
    let error = executor
        .resume(&descriptor, "run-1", DurableOptions::default())
        .expect_err("must refuse");
    assert_eq!(error.code, "DURABLE.CHECKPOINT_INVALID");
}

#[test]
fn an_expired_deadline_ends_the_run_as_timed_out_on_resume() {
    let temp = TempDir::new("expired");
    let descriptor = compile(counting_workflow(3));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let now = aad_runtime::durable::now_seconds();
    let lease = store
        .claim_owner("run-1", "runner", 60.0, now)
        .expect("claim");
    store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now,
        )
        .expect("start");
    // Time spent paused still counts, so a resume after the deadline must not
    // get a fresh budget.
    store
        .append_event_with_checkpoint(
            &lease,
            "run.checkpointed",
            &json!({}),
            &json!({
                "checkpointVersion": 1,
                "planDigest": digest,
                "phase": "between_top_level_steps",
                "deadline": now - 10.0,
                "nextTopLevelIndex": 1,
            }),
            None,
            None,
            now,
        )
        .expect("write");
    store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Paused,
            None,
            None,
            None,
            None,
            now,
        )
        .expect("pause");

    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .resume(&descriptor, "run-1", DurableOptions::default())
        .expect("finalise");
    assert_eq!(outcome.run.status, RunStatus::TimedOut);
    // Distinct from a plain failure on the wire.
    assert_eq!(outcome.run.to_json()["status"], json!("timed_out"));
}

// ------------------------------------------------- interrupted-segment safety

#[test]
fn a_run_interrupted_inside_a_step_becomes_unknown_effect_with_no_replay() {
    let temp = TempDir::new("interrupted");
    let descriptor = compile(counting_workflow(3));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let now = aad_runtime::durable::now_seconds();
    // A short TTL so the abandoned lease lapses quickly, as it must before any
    // other runner may touch the run.
    let lease = store
        .claim_owner("run-1", "runner-dead", 0.4, now)
        .expect("claim");
    store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now,
        )
        .expect("start");
    // Exactly what the drive loop writes before dispatching a step. The runner
    // then "dies" without ever writing the exit boundary.
    store
        .append_event_with_checkpoint(
            &lease,
            "run.segment_entered",
            &json!({"stepId": "step1"}),
            &json!({
                "checkpointVersion": 1,
                "planDigest": digest,
                "phase": "in_top_level_step",
                "deadline": now + 300.0,
                "nextTopLevelIndex": 2,
                "variables": {"count": 1},
            }),
            None,
            None,
            now,
        )
        .expect("write");

    // The lease lapses because the process is gone, so a new runner can claim.
    std::thread::sleep(std::time::Duration::from_millis(600));
    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("runner-new"),
        )
        .expect("reconcile");

    assert_eq!(
        outcome.run.status,
        RunStatus::UnknownEffect,
        "an unproven effect must not be reported as a clean failure"
    );
    let error = outcome.run.error.as_ref().expect("error");
    assert_eq!(error["code"], json!("DURABLE.UNKNOWN_EFFECT"));
    assert_eq!(error["effect"], json!("unknown"));
    assert_eq!(error["details"]["phase"], json!("in_top_level_step"));
    // The remedy has to reach a human: only they can inspect the desktop.
    assert!(error["details"]["remedy"].is_string());

    // Crucially, nothing further was dispatched during reconciliation.
    let events: Vec<String> = store
        .list_events("run-1", 0, 100)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|e| *e == "run.segment_entered")
            .count(),
        1,
        "the interrupted step must not be re-entered: {events:?}"
    );
    assert!(!events.iter().any(|e| e == "run.segment_exited"));
}

#[test]
fn an_interruption_during_cleanup_is_also_unknown_effect() {
    let temp = TempDir::new("interrupted-finalizing");
    let descriptor = compile(counting_workflow(2));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let now = aad_runtime::durable::now_seconds();
    let lease = store.claim_owner("run-1", "dead", 0.5, now).expect("claim");
    store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now,
        )
        .expect("start");
    // Cleanup steps can have effects too, so being interrupted there is just as
    // unprovable as being interrupted in the body.
    store
        .append_event_with_checkpoint(
            &lease,
            "run.finalizing",
            &json!({}),
            &json!({
                "checkpointVersion": 1,
                "planDigest": digest,
                "phase": "finalizing",
                "deadline": now + 300.0,
                "nextTopLevelIndex": 2,
            }),
            None,
            None,
            now,
        )
        .expect("write");

    // Wait for the abandoned lease to lapse; until then the run stays fenced.
    std::thread::sleep(std::time::Duration::from_millis(700));
    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("new"),
        )
        .expect("reconcile");
    assert_eq!(outcome.run.status, RunStatus::UnknownEffect);
    assert_eq!(
        outcome.run.error.as_ref().expect("error")["details"]["phase"],
        json!("finalizing")
    );
}

// ---------------------------------------------------------------- lease fencing

#[test]
fn a_live_lease_stops_a_second_runner_from_resuming_the_same_run() {
    let temp = TempDir::new("fencing");
    let descriptor = compile(counting_workflow(3));
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    store
        .create_run(
            "run-1",
            "durable.counting",
            &json!({}),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let now = aad_runtime::durable::now_seconds();
    // A runner that is very much alive holds the run.
    let held = store
        .claim_owner("run-1", "runner-alive", 600.0, now)
        .expect("claim");
    store
        .set_status(
            &held,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now,
        )
        .expect("start");

    let executor = DurableExecutor::new(temp.open());
    let error = executor
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("interloper"),
        )
        .expect_err("must not steal a live run");
    assert_eq!(error.code, "JOURNAL.LEASE_CONFLICT");
    // Retryable: the lease may lapse later, so this is worth trying again.
    assert!(error.retryable);
}

// ---------------------------------------------------------------- real crash

/// A workflow whose steps are slow, so a kill reliably lands mid-run.
///
/// `while` with a bounded counter burns wall-clock time without needing a script
/// or action step, both of which durable mode refuses.
fn slow_counting_workflow(steps: usize, spin: u64) -> Value {
    let body: Vec<Value> = (0..steps)
        .map(|index| {
            json!({
                "id": format!("step{index}"),
                "type": "block",
                "steps": [
                    {
                        "id": format!("spin{index}"),
                        "type": "while",
                        "condition": format!("${{{{ vars.spin{index} < {spin} }}}}"),
                        "max_iterations": spin + 1,
                        // The compiler requires an explicit timeout on a loop,
                        // so an unbounded one cannot be written by accident.
                        "timeout": "60s",
                        "steps": [{
                            "id": format!("tick{index}"),
                            "type": "set",
                            "assign": {
                                format!("vars.spin{index}"):
                                    format!("${{{{ vars.spin{index} + 1 }}}}"),
                            },
                        }],
                    },
                    {
                        "id": format!("count{index}"),
                        "type": "set",
                        "assign": {"vars.count": "${{ vars.count + 1 }}"},
                    },
                ],
            })
        })
        .collect();
    let mut variables = serde_json::Map::new();
    variables.insert("count".into(), counter(0));
    for index in 0..steps {
        variables.insert(format!("spin{index}"), counter(0));
    }
    document(
        "durable.slow-counting",
        json!({
            // The spin loops execute a great many steps, so the default budget
            // would stop the run before the crash could be observed.
            "budgets": {"max_duration": "120s", "max_executed_steps": 500000},
            "variables": Value::Object(variables),
            "outputs": {"total": {"value": "${{ vars.count }}"}},
            "steps": body,
        }),
    )
}

/// Locate the helper binary cargo built alongside this test.
fn crash_runner_path() -> std::path::PathBuf {
    // The test binary lives in target/<profile>/deps, so the helper is two
    // levels up. Finding it this way avoids assuming debug vs release.
    let mut directory = std::env::current_exe().expect("test executable path");
    directory.pop();
    if directory.ends_with("deps") {
        directory.pop();
    }
    let candidate = directory.join(format!("crash_runner{}", std::env::consts::EXE_SUFFIX));
    assert!(
        candidate.exists(),
        "crash_runner helper not found at {}; cargo should build it with the crate",
        candidate.display()
    );
    candidate
}

/// Kill a real process mid-run and recover from the journal it left behind.
///
/// This is the test that justifies the word "durable". The others write crash
/// state by hand, which only proves the recovery code does what I expect. Here a
/// real child process is killed with no chance to clean up, flush, or release its
/// lease, and recovery works from nothing but the SQLite file it left behind.
#[test]
fn a_killed_process_leaves_a_recoverable_journal() {
    let temp = TempDir::new("real-crash");
    let journal_path = temp.journal_path();
    // Slow enough that the child is certainly mid-run when killed, and long
    // enough that plenty of steps remain afterwards.
    let raw = slow_counting_workflow(40, 400);
    let descriptor = compile(raw.clone());
    let descriptor_path = temp.path.join("workflow.json");
    std::fs::write(
        &descriptor_path,
        serde_json::to_string(&raw).expect("serialise"),
    )
    .expect("write descriptor");

    let mut child = std::process::Command::new(crash_runner_path())
        .arg(&journal_path)
        .arg(&descriptor_path)
        .arg("run-1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the crash runner");

    // Wait until the child has genuinely committed progress, so the kill
    // interrupts real work rather than racing startup.
    let mut committed = 0u64;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let Ok(store) = JournalStore::open(&journal_path) else {
            continue;
        };
        if let Ok(run) = store.get_run("run-1") {
            if let Some(checkpoint) = &run.checkpoint {
                committed = checkpoint["variables"]["count"].as_u64().unwrap_or(0);
                if committed >= 3 {
                    break;
                }
            }
        }
    }
    assert!(
        committed >= 3,
        "the child did not commit progress before the kill (count={committed})"
    );

    // SIGKILL equivalent: no unwinding, no destructors, no lease release.
    child.kill().expect("kill the child");
    let status = child.wait().expect("reap the child");
    assert!(!status.success(), "the child must not have exited cleanly");

    // Read the state the dead process left behind.
    let store = JournalStore::open(&journal_path).expect("reopen after the kill");
    let abandoned = store.get_run("run-1").expect("the run survived the kill");
    assert_eq!(
        abandoned.status,
        RunStatus::Running,
        "a killed runner cannot mark its own run finished"
    );
    assert_eq!(
        abandoned.owner_id.as_deref(),
        Some("crash-runner"),
        "the dead process still appears to hold the lease"
    );
    let interrupted_at = abandoned
        .checkpoint
        .as_ref()
        .expect("a checkpoint was committed")["nextTopLevelIndex"]
        .as_u64()
        .expect("index");
    // Which of the two real crash states we landed in. Killing a process cannot
    // be aimed precisely, so rather than assume one, read what actually happened
    // and hold that case to its own strict expectation. Both are genuine
    // outcomes; neither is allowed to replay a step.
    let interrupted_phase = abandoned.checkpoint.as_ref().expect("checkpoint")["phase"]
        .as_str()
        .expect("phase")
        .to_string();
    let entered_before = store
        .list_events("run-1", 0, 5000)
        .expect("events")
        .iter()
        .filter(|event| event.event_type == "run.segment_entered")
        .count();

    // Before the lease lapses the run must stay fenced, whoever asks.
    let contended = DurableExecutor::new(JournalStore::open(&journal_path).expect("open"));
    let too_early = contended
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("impatient"),
        )
        .expect_err("a live lease must fence a second runner even after a crash");
    assert_eq!(
        too_early.code, "JOURNAL.LEASE_CONFLICT",
        "before the lease lapses the run must stay fenced"
    );
    std::thread::sleep(std::time::Duration::from_millis(1400));

    // A brand new executor with a brand new connection: everything it knows
    // comes from the file the killed process left.
    let executor = DurableExecutor::new(JournalStore::open(&journal_path).expect("reopen"));
    let outcome = executor
        .resume(
            &descriptor,
            "run-1",
            DurableOptions::default().with_owner_id("recovered"),
        )
        .expect("resume from disk after a real crash");

    // The kill landed in one of exactly two real states, and each has its own
    // strict requirement. What is never allowed is re-entering a step that had
    // already begun.
    let events: Vec<String> = store
        .list_events("run-1", 0, 5000)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    let entered_after = events
        .iter()
        .filter(|e| *e == "run.segment_entered")
        .count();

    if interrupted_phase == "in_top_level_step" {
        // Killed mid-step: whether the effect landed is unknowable, so the run
        // must say so rather than guess, and must dispatch nothing at all.
        assert_eq!(
            outcome.run.status,
            RunStatus::UnknownEffect,
            "an interrupted step must not be silently replayed or reported as failed"
        );
        let error = outcome.run.error.as_ref().expect("error");
        assert_eq!(error["code"], json!("DURABLE.UNKNOWN_EFFECT"));
        assert_eq!(error["details"]["phase"], json!("in_top_level_step"));
        assert_eq!(
            entered_after, entered_before,
            "recovery must not enter any step: {entered_before} before, {entered_after} after"
        );
    } else {
        // Killed cleanly between steps: this is resumable, and the run must
        // finish the remaining work without repeating what was already done.
        assert_eq!(
            interrupted_phase, "between_top_level_steps",
            "a crash leaves the run either mid-step or at a boundary"
        );
        assert_eq!(outcome.run.status, RunStatus::Succeeded);
        let total = outcome.run.output.as_ref().expect("output")["total"]
            .as_u64()
            .expect("total");
        assert_eq!(total, 40, "every step ran exactly once across the crash");
        assert_eq!(
            entered_after, 40,
            "each of the 40 steps must be entered exactly once, not re-entered"
        );
    }
    assert!(
        events.iter().any(|e| e == "run.reclaimed"),
        "taking over a dead runner's run must be recorded: {events:?}"
    );
    // `interrupted_at` is the boundary the dead process had reached; recovery
    // must never rewind behind it.
    assert!(
        entered_after >= interrupted_at as usize,
        "recovery must not lose committed progress"
    );
}

#[test]
fn inputs_are_restored_from_the_journal_on_resume() {
    let temp = TempDir::new("inputs");
    let raw = document(
        "durable.with-inputs",
        json!({
            "inputs": {"label": {"schema": {"type": "string"}, "required": true}},
            "variables": {
                "seen": {"schema": {"type": "string"}, "mutable": true, "initial": ""},
            },
            "outputs": {"echo": {"value": "${{ vars.seen }}"}},
            "steps": [
                {"id": "first", "type": "set", "assign": {"vars.seen": "${{ inputs.label }}"}},
                {"id": "second", "type": "set", "assign": {"vars.seen": "${{ vars.seen }}!"}},
            ],
        }),
    );
    let descriptor = compile(raw);
    let store = temp.open();
    let digest = aad_runtime::plan_digest(&descriptor);
    let mut inputs = Map::new();
    inputs.insert("label".into(), json!("hello"));
    store
        .create_run(
            "run-1",
            "durable.with-inputs",
            &Value::Object(inputs),
            &descriptor.raw,
            None,
            Some(&digest),
            None,
        )
        .expect("create");
    let now = aad_runtime::durable::now_seconds();
    let lease = store.claim_owner("run-1", "dead", 0.4, now).expect("claim");
    store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now,
        )
        .expect("start");
    store
        .append_event_with_checkpoint(
            &lease,
            "run.segment_exited",
            &json!({}),
            &json!({
                "checkpointVersion": 1,
                "planDigest": digest,
                "phase": "between_top_level_steps",
                "deadline": now + 300.0,
                "nextTopLevelIndex": 1,
                "executedSteps": 1,
                "variables": {"seen": "hello"},
            }),
            None,
            None,
            now,
        )
        .expect("checkpoint");
    drop(store);
    std::thread::sleep(std::time::Duration::from_millis(600));

    // A resumed run must see the same inputs the original did; re-deriving them
    // from the descriptor's defaults would silently change behaviour.
    let executor = DurableExecutor::new(temp.open());
    let outcome = executor
        .resume(&descriptor, "run-1", DurableOptions::default())
        .expect("resume");
    assert_eq!(outcome.run.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.run.output.as_ref().expect("output")["echo"],
        json!("hello!")
    );
}
