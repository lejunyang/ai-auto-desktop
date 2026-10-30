//! Durable workflow runs over MCP.
//!
//! `run_workflow` with `durable: true` starts a run through the runtime's
//! [`DurableExecutor`] against the per-user run store
//! ([`durable::default_store_path`]), the same SQLite journal `aad start`,
//! `aad status` and `aad resume` use. The tools here mirror those commands so
//! an agent can follow a run it started, from this server or a later one.
//!
//! How a run is driven:
//!
//! * The executor runs on a background thread that owns its own journal
//!   connection and the run's owner lease. The call that started it waits up to
//!   `wait_ms` and then answers with the run's current state, so a long run
//!   comes back as `running` with a `run_id` instead of blocking the call.
//! * The lease is renewed at every step boundary, exactly as for `aad start`.
//!   While this server is alive no other runner can take the run over.
//! * When the client disconnects, the server asks each run it is driving to
//!   pause and waits briefly, so a run between steps stops cleanly as `paused`.
//! * If the process dies instead -- killed, or a step outlasts the grace period
//!   -- nothing more is written. The run stays `running` until its lease lapses
//!   (`DEFAULT_LEASE_TTL_SECONDS`); `workflow_run_resume` (or `aad resume`) then
//!   takes it over. Resumption follows the executor's rules and nothing else: a
//!   run stopped between steps continues, while a run interrupted inside a step
//!   is finalised as `unknown_effect` with no further dispatch. An interrupted
//!   step is never replayed, except the validated read-only `action_intent`
//!   case the executor proves safe itself.
//!
//! Durable execution keeps its deliberate limits here: scripts, write actions
//! and sensitive inputs or outputs are refused, and top-level read-only actions
//! need `durable_actions: "read_only"`. A refusal is returned as such; the
//! caller decides whether to run in memory instead (`fallback_in_memory`).

use crate::tools::{self, brief, failure, Tool};
use aad_core::AutomationError;
use aad_runtime::durable::{self, DesiredState, JournalError, JournalStore, RunRecord, RunStatus};
use aad_runtime::durable_exec::{
    self, DurableActionMode, DurableExecutor, DurableOptions, DurableOutcome,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The tools this module answers, besides `run_workflow`'s durable mode.
pub const NAMES: &[&str] = &[
    "workflow_run_status",
    "workflow_run_list",
    "workflow_run_events",
    "workflow_run_resume",
    "workflow_run_pause",
    "workflow_run_cancel",
];

/// The ones that change a run, and so need the server's input grant.
pub const MUTATING: &[&str] = &[
    "workflow_run_resume",
    "workflow_run_pause",
    "workflow_run_cancel",
];

const DEFAULT_WAIT_MS: i64 = 20_000;
const MAX_WAIT_MS: i64 = 120_000;
/// How long a freshly spawned runner may take to create its run row before the
/// call answers `starting`. Creation is a validation pass and one insert.
const CREATION_GRACE: Duration = Duration::from_secs(5);
/// How long a disconnect waits for runs to reach a step boundary and pause.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Events read to work out which step a run is on.
const RECENT_EVENTS: i64 = 32;

fn run_id_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "pattern": "^[A-Za-z0-9._-]{1,128}$",
        "description": description,
    })
}

fn wait_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": MAX_WAIT_MS,
        "description": "How long to wait for the run to finish or pause before \
    answering with its current state. Defaults to 20000.",
    })
}

fn durable_actions_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["deny", "read_only"],
        "description": "deny (default) refuses every action step; read_only also \
    accepts strictly validated top-level read-only actions. Write actions are always refused.",
    })
}

pub fn catalogue() -> Vec<Tool> {
    vec![
        Tool {
            name: "workflow_run_status",
            description: "Report a durable run started with run_workflow durable: true \
(or `aad start`): its status, the step it is on, its outputs or error, and what to do \
next. A run interrupted inside a step reports status unknown_effect with the phase and \
remedy: whether that step took effect cannot be known, so inspect the application \
before repeating anything.",
            schema: json!({
                "type": "object",
                "properties": {"run_id": run_id_schema("The run to report.")},
                "required": ["run_id"],
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "workflow_run_list",
            description: "List durable runs in the per-user run store, newest first, \
including runs started by `aad start` or by an earlier server.",
            schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "running", "paused", "succeeded", "failed",
                                 "timed_out", "cancelled", "unknown_effect"],
                        "description": "Only runs with this status."
                    },
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100,
                              "description": "Runs per page. Defaults to 20."},
                    "offset": {"type": "integer", "minimum": 0,
                               "description": "Runs to skip, for the next page."}
                },
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "workflow_run_events",
            description: "Read a durable run's event history in pages. Pass the \
next_after_seq of one page as after_seq to read the next; has_more says whether \
there is one.",
            schema: json!({
                "type": "object",
                "properties": {
                    "run_id": run_id_schema("The run whose history to read."),
                    "after_seq": {"type": "integer", "minimum": 0,
                                  "description": "Only events after this sequence \
            number. Defaults to 0, the start."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200,
                              "description": "Events per page. Defaults to 50."}
                },
                "required": ["run_id"],
                "additionalProperties": false
            }),
            mutating: false,
        },
        Tool {
            name: "workflow_run_resume",
            description: "Continue a durable run that is paused, or whose runner \
exited (its lease must have lapsed first). Give the saved workflow it was started \
from; a workflow changed since is refused. A run stopped between steps continues \
from its checkpoint with its original inputs. A run interrupted inside a step is \
finalised as unknown_effect and nothing is replayed: an action that may already have \
happened is never repeated automatically.",
            schema: json!({
                "type": "object",
                "properties": {
                    "run_id": run_id_schema("The run to continue."),
                    "name": {
                        "type": "string",
                        "maxLength": 200,
                        "description": "The saved workflow the run was started from."
                    },
                    "durable_actions": durable_actions_schema(),
                    "wait_ms": wait_schema()
                },
                "required": ["run_id", "name"],
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "workflow_run_pause",
            description: "Ask a durable run to pause at its next step boundary. This \
records the request; the answer's status says whether it has paused yet. \
workflow_run_resume continues it.",
            schema: json!({
                "type": "object",
                "properties": {"run_id": run_id_schema("The run to pause.")},
                "required": ["run_id"],
                "additionalProperties": false
            }),
            mutating: true,
        },
        Tool {
            name: "workflow_run_cancel",
            description: "Ask a durable run to stop for good at its next step boundary. \
Cancel cannot be withdrawn. A paused run, or one whose runner has exited, is finalised \
as cancelled by workflow_run_resume.",
            schema: json!({
                "type": "object",
                "properties": {"run_id": run_id_schema("The run to cancel.")},
                "required": ["run_id"],
                "additionalProperties": false
            }),
            mutating: true,
        },
    ]
}

/// Why a durable start produced no run.
pub(crate) enum StartError {
    /// Durable execution would not take this workflow, and nothing was created
    /// or dispatched. The caller may choose to run it in memory instead.
    Refused(Value),
    /// Anything else, as a structured tool error.
    Failed(String),
}

/// The background runners this server is driving, and the store they use.
pub struct Runs {
    store: Option<PathBuf>,
    lease_ttl_seconds: f64,
    /// Runs this process is executing. `None` until the spawn has returned.
    active: Arc<Mutex<BTreeMap<String, Option<JoinHandle<()>>>>>,
    /// The last error a runner of ours stopped with, for runs it left behind.
    runner_errors: Arc<Mutex<BTreeMap<String, Value>>>,
}

impl Default for Runs {
    fn default() -> Self {
        Self {
            store: None,
            lease_ttl_seconds: durable_exec::DEFAULT_LEASE_TTL_SECONDS,
            active: Arc::default(),
            runner_errors: Arc::default(),
        }
    }
}

/// What waiting on a runner came to.
enum Waited {
    Finished(Result<(), Box<AutomationError>>),
    StillRunning,
}

/// Desktop serialisation a runner holds while it may dispatch actions.
struct DesktopHold {
    _lease: Arc<aad_control::lease::InputLease>,
    gate: Option<Arc<Mutex<()>>>,
}

/// Removes a run from the active set however its runner ends, panics included,
/// so a dead runner is never reported as attached.
struct Detach {
    active: Arc<Mutex<BTreeMap<String, Option<JoinHandle<()>>>>>,
    run_id: String,
}

impl Drop for Detach {
    fn drop(&mut self) {
        lock(&self.active).remove(&self.run_id);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Runs {
    /// Use this store instead of the per-user default.
    pub fn with_store(mut self, path: PathBuf) -> Self {
        self.store = Some(path);
        self
    }

    /// Hold run leases this long between renewals.
    pub fn with_lease_ttl_seconds(mut self, seconds: f64) -> Self {
        self.lease_ttl_seconds = seconds;
        self
    }

    pub fn store_path(&self) -> PathBuf {
        self.store
            .clone()
            .unwrap_or_else(durable::default_store_path)
    }

    fn open(&self) -> Result<JournalStore, String> {
        let path = self.store_path();
        JournalStore::open(&path).map_err(|error| store_failure(&error, &path))
    }

    fn is_attached(&self, run_id: &str) -> bool {
        lock(&self.active).contains_key(run_id)
    }

    /// Dispatch one of [`NAMES`].
    pub(crate) fn call(
        &self,
        session: &Arc<aad_control::Session>,
        name: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        match name {
            "workflow_run_status" => self.status(arguments),
            "workflow_run_list" => self.list(arguments),
            "workflow_run_events" => self.events(arguments),
            "workflow_run_resume" => self.resume(session, arguments),
            "workflow_run_pause" => self.control(arguments, DesiredState::Pause),
            "workflow_run_cancel" => self.control(arguments, DesiredState::Cancel),
            other => Err(failure(
                "MCP.UNKNOWN_TOOL",
                &format!("{other:?} is not a durable run tool"),
                None,
            )),
        }
    }

    /// `run_workflow` with `durable: true`.
    pub(crate) fn start(
        &self,
        session: &Arc<aad_control::Session>,
        arguments: &Value,
    ) -> Result<Value, StartError> {
        use StartError::{Failed, Refused};
        tools::check_run_arguments(arguments, true).map_err(Failed)?;
        let (name, descriptor) = tools::compiled_workflow(arguments).map_err(Failed)?;
        // Not a durable refusal: an in-memory run refuses scripts as well, so
        // there is nothing to fall back to.
        tools::refuse_scripts(&name, &descriptor).map_err(Failed)?;
        let inputs = tools::workflow_inputs(arguments).map_err(Failed)?;
        let mode = action_mode(arguments).map_err(Failed)?;
        let wait = wait_for(arguments).map_err(Failed)?;
        let run_id = match arguments.get("run_id") {
            None => uuid::Uuid::new_v4().to_string(),
            Some(_) => run_id(arguments).map_err(Failed)?,
        };

        // Checked here as well as by the executor, so the commonest refusal is
        // immediate and certainly precedes any run being created.
        durable_exec::assert_durable_plan_with_mode(&descriptor, mode)
            .map_err(|error| Refused(refusal(&name, &error)))?;

        let path = self.store_path();
        let store = JournalStore::open(&path).map_err(|error| {
            Refused(serde_json::from_str(&store_failure(&error, &path)).unwrap_or(Value::Null))
        })?;
        if store.get_run(&run_id).is_ok() {
            return Err(Failed(failure(
                "MCP.RUN_EXISTS",
                &format!("a run named {run_id:?} already exists"),
                Some("Choose another run_id, or follow that run with workflow_run_status."),
            )));
        }

        let providers = tools::session_providers(session).map_err(Failed)?;
        let options = DurableOptions::default()
            .with_inputs(inputs)
            .with_owner_id(owner_id())
            .with_lease_ttl_seconds(self.lease_ttl_seconds)
            .with_base_directory(workflow_directory(&name))
            .with_providers(providers)
            .with_granted_permissions(tools::WORKFLOW_PERMISSIONS)
            .with_action_mode(mode);
        let hold = desktop_hold(session, &descriptor, arguments).map_err(Failed)?;

        let id = run_id.clone();
        let waited = self.spawn(&run_id, hold, wait, &store, move |executor| {
            executor.start(&descriptor, Some(&id), options)
        });
        match waited {
            Waited::Finished(Err(error)) if store.get_run(&run_id).is_err() => {
                // Nothing was created, so nothing ran: a refusal in every sense
                // that matters to the caller, whatever the executor called it.
                lock(&self.runner_errors).remove(&run_id);
                Err(Refused(refusal(&name, &error)))
            }
            _ => Ok(self.report(&store, &run_id, Some(&name))),
        }
    }

    fn resume(
        &self,
        session: &Arc<aad_control::Session>,
        arguments: &Value,
    ) -> Result<Value, String> {
        tools::check_arguments(arguments, &["run_id", "name", "durable_actions", "wait_ms"])?;
        let run_id = run_id(arguments)?;
        let (name, descriptor) = tools::compiled_workflow(arguments)?;
        tools::refuse_scripts(&name, &descriptor)?;
        let mode = action_mode(arguments)?;
        let wait = wait_for(arguments)?;

        let store = self.open()?;
        let run = store
            .get_run(&run_id)
            .map_err(|error| journal_failure(&error))?;
        if self.is_attached(&run_id) {
            return Err(failure(
                "MCP.RUN_ACTIVE",
                &format!("this server is already executing run {run_id}"),
                Some(
                    "Follow it with workflow_run_status; pause or cancel it if it \
should stop.",
                ),
            ));
        }
        if run.is_terminal() {
            let hint = if run.status == RunStatus::UnknownEffect {
                "It was interrupted inside a step, so whether that step took effect is \
unknown, and it is never replayed automatically. Inspect the application, then start \
a new run if the work still needs doing."
            } else {
                "A finished run cannot be continued; start a new run instead."
            };
            let mut payload = json!({
                "code": "DURABLE.ALREADY_TERMINAL",
                "message": format!("run {run_id} already finished as {}", run.status.as_str()),
                "retryable": false,
                "effect": "not_applied",
                "hint": hint,
            });
            payload["run"] = self.report(&store, &run_id, None);
            return Err(serde_json::to_string(&payload).unwrap_or_default());
        }

        let providers = tools::session_providers(session)?;
        // No inputs: they were persisted when the run was created, and the
        // second half of a run must not execute against different values.
        let options = DurableOptions::default()
            .with_owner_id(owner_id())
            .with_lease_ttl_seconds(self.lease_ttl_seconds)
            .with_base_directory(workflow_directory(&name))
            .with_providers(providers)
            .with_granted_permissions(tools::WORKFLOW_PERMISSIONS)
            .with_action_mode(mode);
        let hold = desktop_hold(session, &descriptor, arguments)?;

        let id = run_id.clone();
        let waited = self.spawn(&run_id, hold, wait, &store, move |executor| {
            executor.resume(&descriptor, &id, options)
        });
        if let Waited::Finished(Err(error)) = waited {
            let mut payload = brief(&error);
            if let Some(hint) = resume_hint(&error, &run) {
                payload["hint"] = json!(hint);
            }
            payload["run"] = self.report(&store, &run_id, Some(&name));
            return Err(serde_json::to_string(&payload).unwrap_or_default());
        }
        Ok(self.report(&store, &run_id, Some(&name)))
    }

    /// Run `work` on a background thread and wait up to `wait` for it.
    fn spawn<F>(
        &self,
        run_id: &str,
        hold: Option<DesktopHold>,
        wait: Duration,
        store: &JournalStore,
        work: F,
    ) -> Waited
    where
        F: FnOnce(&DurableExecutor) -> Result<DurableOutcome, AutomationError> + Send + 'static,
    {
        let path = self.store_path();
        let (sender, receiver) = mpsc::channel();
        lock(&self.runner_errors).remove(run_id);
        lock(&self.active).insert(run_id.to_string(), None);
        let detach = Detach {
            active: self.active.clone(),
            run_id: run_id.to_string(),
        };
        let errors = self.runner_errors.clone();
        let id = run_id.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("aad-durable-{run_id}"))
            .spawn(move || {
                let result = {
                    let gate = hold.as_ref().and_then(|hold| hold.gate.clone());
                    let _serialized = gate.as_ref().map(|gate| lock(gate));
                    let _hold = hold;
                    JournalStore::open(&path)
                        .map_err(|error| {
                            AutomationError::new(error.code(), error.message())
                                .with_effect("not_applied")
                        })
                        .and_then(|journal| work(&DurableExecutor::new(journal)))
                        .map(|_| ())
                    // The desktop locks and the journal are released here,
                    // before anyone is told the runner is done.
                };
                if let Err(error) = &result {
                    lock(&errors).insert(id, brief(error));
                }
                drop(detach);
                // The caller may have stopped waiting; that is not an error.
                let _ = sender.send(result);
            });
        match spawned {
            Ok(handle) => {
                if let Some(slot) = lock(&self.active).get_mut(run_id) {
                    *slot = Some(handle);
                }
            }
            Err(error) => {
                lock(&self.active).remove(run_id);
                return Waited::Finished(Err(Box::new(
                    AutomationError::new(
                        "MCP.RUNNER_UNAVAILABLE",
                        format!("could not start a runner thread: {error}"),
                    )
                    .with_effect("not_applied"),
                )));
            }
        }

        match receiver.recv_timeout(wait) {
            Ok(result) => return Waited::Finished(result.map_err(Box::new)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Waited::Finished(Err(Box::new(runner_panicked())))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // Still going. Make sure the run row exists before answering, so the
        // run_id handed back can be looked up straight away.
        let deadline = Instant::now() + CREATION_GRACE;
        while store.get_run(run_id).is_err() && Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => return Waited::Finished(result.map_err(Box::new)),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Waited::Finished(Err(Box::new(runner_panicked())))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        Waited::StillRunning
    }

    /// Ask every run this server drives to pause, and give them a moment to.
    ///
    /// Called when the client goes away. A run between steps then stops as
    /// `paused` and releases its lease; one still inside a step when the
    /// process exits is left for `resume` to reconcile.
    pub fn shutdown(&self) {
        let running: Vec<String> = lock(&self.active).keys().cloned().collect();
        if running.is_empty() {
            return;
        }
        if let Ok(store) = self.open() {
            for run_id in &running {
                if let Ok(run) = store.get_run(run_id) {
                    if run.desired_state == DesiredState::Run && !run.is_terminal() {
                        let _ = store.compare_and_set_desired_state(
                            run_id,
                            DesiredState::Run,
                            DesiredState::Pause,
                            Some((
                                "run.pause_requested",
                                &json!({
                                    "fromDesiredState": "run",
                                    "toDesiredState": "pause",
                                    "reason": "mcp_client_disconnected",
                                }),
                            )),
                        );
                    }
                }
            }
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while !lock(&self.active).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn status(&self, arguments: &Value) -> Result<Value, String> {
        tools::check_arguments(arguments, &["run_id"])?;
        let run_id = run_id(arguments)?;
        let store = self.open()?;
        match store.get_run(&run_id) {
            Ok(_) => Ok(self.report(&store, &run_id, None)),
            Err(JournalError::NotFound(_)) if self.is_attached(&run_id) => Ok(json!({
                "kind": "DurableWorkflowRun",
                "durable": true,
                "run_id": run_id,
                "status": "starting",
                "attached": true,
                "next": "The runner is validating the workflow; ask again shortly.",
            })),
            Err(error) => Err(journal_failure(&error)),
        }
    }

    fn list(&self, arguments: &Value) -> Result<Value, String> {
        tools::check_arguments(arguments, &["status", "limit", "offset"])?;
        let status = match arguments.get("status") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(
                RunStatus::parse(text)
                    .map_err(|error| failure("MCP.INVALID_ARGUMENT", error.message(), None))?,
            ),
            Some(_) => {
                return Err(failure(
                    "MCP.INVALID_ARGUMENT",
                    "status must be a string",
                    None,
                ))
            }
        };
        let limit = integer(arguments, "limit", 20, 1, 100)?;
        let offset = integer(arguments, "offset", 0, 0, i64::MAX - 1)?;
        let store = self.open()?;
        // One extra row says whether there is another page without a count query.
        let mut runs = store
            .list_runs(status, limit + 1, offset)
            .map_err(|error| journal_failure(&error))?;
        let has_more = runs.len() as i64 > limit;
        runs.truncate(limit as usize);
        let rows: Vec<Value> = runs
            .iter()
            .map(|run| {
                json!({
                    "run_id": run.run_id,
                    "workflow": run.workflow_name,
                    "status": run.status.as_str(),
                    "desired_state": run.desired_state.as_str(),
                    "attached": self.is_attached(&run.run_id),
                    "created_at": run.created_at,
                    "updated_at": run.updated_at,
                    "finished_at": run.finished_at,
                })
            })
            .collect();
        let mut payload = json!({
            "kind": "DurableWorkflowRunList",
            "count": rows.len(),
            "runs": rows,
            "has_more": has_more,
            "store": self.store_path().to_string_lossy(),
        });
        if has_more {
            payload["next_offset"] = json!(offset + limit);
        }
        Ok(payload)
    }

    fn events(&self, arguments: &Value) -> Result<Value, String> {
        tools::check_arguments(arguments, &["run_id", "after_seq", "limit"])?;
        let run_id = run_id(arguments)?;
        let after = integer(arguments, "after_seq", 0, 0, i64::MAX - 1)?;
        let limit = integer(arguments, "limit", 50, 1, 200)?;
        let store = self.open()?;
        let mut events = store
            .list_events(&run_id, after, limit + 1)
            .map_err(|error| journal_failure(&error))?;
        let has_more = events.len() as i64 > limit;
        events.truncate(limit as usize);
        // Holds its place when the page is empty, so polling never rewinds.
        let next = events.last().map(|event| event.seq).unwrap_or(after);
        Ok(json!({
            "kind": "DurableWorkflowRunEvents",
            "run_id": run_id,
            "count": events.len(),
            "events": events
                .iter()
                .map(|event| json!({
                    "seq": event.seq,
                    "type": event.event_type,
                    "payload": event.payload,
                    "created_at": event.created_at,
                }))
                .collect::<Vec<_>>(),
            "next_after_seq": next,
            "has_more": has_more,
        }))
    }

    /// Record a pause or cancel request, as `aad pause` / `aad cancel` do.
    ///
    /// Only asks: the runner honours it at its next step boundary, so the
    /// answer reports the run as it is, not as it will be.
    fn control(&self, arguments: &Value, desired: DesiredState) -> Result<Value, String> {
        tools::check_arguments(arguments, &["run_id"])?;
        let run_id = run_id(arguments)?;
        let store = self.open()?;
        let run = store
            .get_run(&run_id)
            .map_err(|error| journal_failure(&error))?;
        if run.is_terminal() {
            return Err(failure(
                "RUN.TERMINAL",
                &format!(
                    "terminal run {run_id} is immutable ({})",
                    run.status.as_str()
                ),
                None,
            ));
        }
        // Asking twice is a success: the wish is already recorded.
        if run.desired_state != desired {
            // Cancel is sticky: downgrading it would revive a run somebody
            // already decided to stop.
            if run.desired_state == DesiredState::Cancel {
                return Err(failure(
                    "RUN.CANCEL_PENDING",
                    &format!("run {run_id} already has a sticky cancel request"),
                    None,
                ));
            }
            let event = match desired {
                DesiredState::Cancel => "run.cancel_requested",
                _ => "run.pause_requested",
            };
            store
                .compare_and_set_desired_state(
                    &run_id,
                    run.desired_state,
                    desired,
                    Some((
                        event,
                        &json!({
                            "fromDesiredState": run.desired_state.as_str(),
                            "toDesiredState": desired.as_str(),
                        }),
                    )),
                )
                .map_err(|error| journal_failure(&error))?;
        }
        Ok(self.report(&store, &run_id, None))
    }

    /// What an agent needs to know about a run, and what it can do next.
    fn report(&self, store: &JournalStore, run_id: &str, name: Option<&str>) -> Value {
        let run = match store.get_run(run_id) {
            Ok(run) => run,
            Err(error) => {
                return json!({
                    "kind": "DurableWorkflowRun",
                    "durable": true,
                    "run_id": run_id,
                    "status": if self.is_attached(run_id) { "starting" } else { "unknown" },
                    "attached": self.is_attached(run_id),
                    "error": serde_json::from_str::<Value>(&journal_failure(&error))
                        .unwrap_or(Value::Null),
                })
            }
        };
        let attached = self.is_attached(run_id);
        let recent = store
            .list_recent_events(run_id, RECENT_EVENTS)
            .unwrap_or_default();
        let checkpoint = run.checkpoint.as_ref();
        let step_id = recent
            .iter()
            .rev()
            .find_map(|event| event.payload.get("stepId").and_then(Value::as_str));
        let lease_live = run
            .lease_expires_at
            .is_some_and(|expires| expires > durable::now_seconds());

        let mut payload = json!({
            "kind": "DurableWorkflowRun",
            "durable": true,
            "run_id": run.run_id,
            "workflow": run.workflow_name,
            "status": run.status.as_str(),
            "desired_state": run.desired_state.as_str(),
            "terminal": run.is_terminal(),
            "outputs": run.output,
            "error": run.error,
            "executed_steps": checkpoint.and_then(|c| c.get("executedSteps")).cloned(),
            "current_step": {
                "step_id": step_id,
                "phase": checkpoint.and_then(|c| c.get("phase")).cloned(),
                "next_step_index": checkpoint.and_then(|c| c.get("nextTopLevelIndex")).cloned(),
            },
            "owner": run.owner_id.as_ref().map(|owner| json!({
                "owner_id": owner,
                "lease_expires_at": run.lease_expires_at,
                "lease_live": lease_live,
            })),
            "attached": attached,
            "last_event": recent.last().map(|event| json!({
                "seq": event.seq,
                "type": event.event_type,
                "created_at": event.created_at,
            })),
            "created_at": run.created_at,
            "updated_at": run.updated_at,
            "finished_at": run.finished_at,
            "store": self.store_path().to_string_lossy(),
        });
        if let Some(name) = name {
            payload["name"] = json!(name);
        }
        if run.status == RunStatus::UnknownEffect {
            let error = run.error.clone().unwrap_or(Value::Null);
            payload["unknown_effect"] = json!({
                "message": error.get("message").cloned(),
                "phase": error.pointer("/details/phase").cloned(),
                "next_step_index": error.pointer("/details/nextTopLevelIndex").cloned(),
                "remedy": error.pointer("/details/remedy").cloned(),
            });
        }
        if !run.is_terminal() && !attached {
            if let Some(error) = lock(&self.runner_errors).get(run_id) {
                payload["runner_error"] = error.clone();
            }
        }
        payload["next"] = json!(next_step(&run, attached, lease_live));
        payload
    }
}

/// Plain advice for the state a run is in.
fn next_step(run: &RunRecord, attached: bool, lease_live: bool) -> &'static str {
    match run.status {
        RunStatus::Succeeded => "Finished. outputs holds the result.",
        RunStatus::Failed | RunStatus::TimedOut => {
            "Finished without succeeding; error says why and whether it took effect."
        }
        RunStatus::Cancelled => "Cancelled at a step boundary; nothing was in flight.",
        RunStatus::UnknownEffect => {
            "Interrupted inside a step: whether that step took effect is unknown, and it \
is never retried automatically. Inspect the application before repeating anything."
        }
        RunStatus::Paused if run.desired_state == DesiredState::Cancel => {
            "Paused with a cancel requested; workflow_run_resume finalises it as cancelled."
        }
        RunStatus::Paused => "Paused at a step boundary; workflow_run_resume continues it.",
        _ if attached => {
            "Running in this server. Poll workflow_run_status; workflow_run_pause and \
workflow_run_cancel take effect at the next step boundary."
        }
        _ if lease_live => {
            "Another process holds this run's lease and is executing it, or exited \
moments ago. Check again after the lease expires."
        }
        _ => {
            "No live runner holds this run; its process probably exited. \
workflow_run_resume takes it over: a run stopped between steps continues, one \
interrupted inside a step becomes unknown_effect with nothing replayed."
        }
    }
}

/// Extra guidance for a resume the executor refused.
fn resume_hint(error: &AutomationError, run: &RunRecord) -> Option<&'static str> {
    match error.code.as_str() {
        "JOURNAL.LEASE_CONFLICT" => Some(if run.status == RunStatus::Running {
            "Another runner still holds this run's lease. If its process has exited, \
try again once the lease expires (owner.lease_expires_at)."
        } else {
            "Another runner holds this run's lease."
        }),
        "DURABLE.PLAN_MISMATCH" => Some(
            "The saved workflow has changed since this run started; a run can only be \
resumed against the plan it was checkpointed with.",
        ),
        _ => None,
    }
}

fn refusal(name: &str, error: &AutomationError) -> Value {
    let mut payload = brief(error);
    // Nothing was created, so nothing ran, whatever the error itself says.
    payload["effect"] = json!("not_applied");
    payload["run_created"] = json!(false);
    payload["hint"] = json!(format!(
        "Durable execution refused workflow {name:?} before anything ran. It accepts \
no scripts, no write actions and no sensitive inputs or outputs; top-level read-only \
actions need durable_actions: \"read_only\". Run it without durable (it cannot then \
be resumed), or pass fallback_in_memory: true to do that automatically."
    ));
    payload
}

fn runner_panicked() -> AutomationError {
    AutomationError::new(
        "MCP.RUNNER_FAILED",
        "the durable runner stopped unexpectedly; the run, if created, is resumable",
    )
    .with_effect("unknown")
}

fn store_failure(error: &JournalError, path: &std::path::Path) -> String {
    let mut payload: Value =
        serde_json::from_str(&failure(error.code(), error.message(), None)).unwrap_or_default();
    payload["store"] = json!(path.to_string_lossy());
    serde_json::to_string(&payload).unwrap_or_default()
}

fn journal_failure(error: &JournalError) -> String {
    let hint = matches!(error, JournalError::NotFound(_))
        .then_some("workflow_run_list shows the runs in the store.");
    failure(error.code(), error.message(), hint)
}

fn owner_id() -> String {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    format!("mcp-{}-{}", std::process::id(), &unique[..8])
}

/// Where a saved workflow's relative references resolve from.
fn workflow_directory(name: &str) -> PathBuf {
    aad_runtime::recordings::workflow_path(name)
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(aad_runtime::recordings::recordings_dir)
}

/// The desktop locks a runner must hold, if the workflow can touch the desktop.
///
/// Durable execution only ever dispatches read-only actions, but they still
/// observe through the same native providers as `control_act`, so they are
/// serialised the same way an in-memory `run_workflow` is.
fn desktop_hold(
    session: &Arc<aad_control::Session>,
    descriptor: &aad_core::WorkflowDescriptor,
    arguments: &Value,
) -> Result<Option<DesktopHold>, String> {
    let acts = descriptor
        .all_steps()
        .iter()
        .any(|step| step.step_type == aad_core::model::StepType::Action);
    if !acts {
        return Ok(None);
    }
    let through_session = tools::steps_through_session(&json!({"name": arguments["name"]}))?;
    let lease = session
        .input_lease("desktop-input")
        .map_err(|error| serde_json::to_string(&error.to_json()).unwrap_or_default())?;
    Ok(Some(DesktopHold {
        _lease: lease,
        gate: (!through_session).then(|| session.gate()),
    }))
}

fn run_id(arguments: &Value) -> Result<String, String> {
    match arguments.get("run_id").and_then(Value::as_str) {
        Some(id)
            if !id.is_empty()
                && id.len() <= 128
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) =>
        {
            Ok(id.to_string())
        }
        Some(_) => Err(failure(
            "MCP.INVALID_ARGUMENT",
            "run_id must be 1-128 letters, digits, '.', '_' or '-'",
            None,
        )),
        None => Err(failure("MCP.INVALID_ARGUMENT", "run_id is required", None)),
    }
}

fn action_mode(arguments: &Value) -> Result<DurableActionMode, String> {
    match arguments.get("durable_actions") {
        None | Some(Value::Null) => Ok(DurableActionMode::Deny),
        Some(Value::String(mode)) if mode == "deny" => Ok(DurableActionMode::Deny),
        Some(Value::String(mode)) if mode == "read_only" => Ok(DurableActionMode::ReadOnly),
        Some(_) => Err(failure(
            "MCP.INVALID_ARGUMENT",
            "durable_actions must be \"deny\" or \"read_only\"",
            None,
        )),
    }
}

fn wait_for(arguments: &Value) -> Result<Duration, String> {
    let millis = integer(arguments, "wait_ms", DEFAULT_WAIT_MS, 0, MAX_WAIT_MS)?;
    Ok(Duration::from_millis(millis as u64))
}

fn integer(arguments: &Value, key: &str, default: i64, min: i64, max: i64) -> Result<i64, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_i64()
            .filter(|number| (min..=max).contains(number))
            .ok_or_else(|| {
                failure(
                    "MCP.INVALID_ARGUMENT",
                    &format!("{key} must be an integer from {min} to {max}"),
                    None,
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests::{pure_workflow, stub_driver, stub_session, ScratchStore};

    fn runs(store: &ScratchStore) -> Runs {
        Runs::default().with_store(store.directory.join("runs.sqlite3"))
    }

    fn parse(payload: &str) -> Value {
        serde_json::from_str(payload).unwrap_or_else(|_| json!({"raw": payload}))
    }

    fn started(runs: &Runs, arguments: Value) -> Value {
        match runs.start(&stub_session(), &arguments) {
            Ok(payload) => payload,
            Err(StartError::Refused(refusal)) => panic!("refused: {refusal}"),
            Err(StartError::Failed(payload)) => panic!("failed: {payload}"),
        }
    }

    fn call(runs: &Runs, tool: &str, arguments: Value) -> Result<Value, Value> {
        runs.call(&stub_session(), tool, &arguments)
            .map_err(|payload| parse(&payload))
    }

    /// Ask for a run's status until `done` holds, or fail after a minute.
    fn wait_until(runs: &Runs, run_id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status =
                call(runs, "workflow_run_status", json!({"run_id": run_id})).expect("status");
            if done(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "gave up waiting: {status}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A workflow of `set` steps, each burning a little time in a bounded loop,
    /// so a run is reliably still going when the test acts on it. `set`,
    /// `block` and `while` are all durable-eligible.
    fn slow_workflow(steps: usize, spin: u64) -> Value {
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
        let counter = json!({"schema": {"type": "integer"}, "mutable": true, "initial": 0});
        let mut variables = serde_json::Map::new();
        variables.insert("count".into(), counter.clone());
        for index in 0..steps {
            variables.insert(format!("spin{index}"), counter.clone());
        }
        json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "mcp.slow"},
            "budgets": {"max_duration": "120s", "max_executed_steps": 500000},
            "variables": Value::Object(variables),
            "steps": body,
            "outputs": {"total": {"value": "${{ vars.count }}"}}
        })
    }

    /// A recorded-style workflow: one desktop `action` step, which durable
    /// execution refuses in its default mode.
    fn action_workflow() -> Value {
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
        workflow
    }

    #[test]
    fn a_durable_run_is_journalled_and_can_be_followed() {
        let store = ScratchStore::new("durable-follow");
        store.save("greeter", pure_workflow());
        let runs = runs(&store);

        let run = started(
            &runs,
            json!({"name": "greeter", "inputs": {"who": "agent"}, "durable": true,
                   "run_id": "job-1"}),
        );
        assert_eq!(run["kind"], json!("DurableWorkflowRun"));
        assert_eq!(run["run_id"], json!("job-1"));
        assert_eq!(run["status"], json!("succeeded"), "{run}");
        assert_eq!(run["outputs"]["greeting"], json!("agent"));
        assert_eq!(run["attached"], json!(false), "the runner has finished");

        // Read back through the status tool, from a fresh store connection.
        let status = call(&runs, "workflow_run_status", json!({"run_id": "job-1"})).unwrap();
        assert_eq!(status["status"], json!("succeeded"));
        assert_eq!(status["terminal"], json!(true));
        assert_eq!(status["executed_steps"], json!(1));
        assert_eq!(status["current_step"]["step_id"], json!("greet"));
        assert!(status["next"].as_str().unwrap().contains("outputs"));

        // The history pages, and the pages join up without gaps or repeats.
        let first = call(
            &runs,
            "workflow_run_events",
            json!({"run_id": "job-1", "limit": 2}),
        )
        .unwrap();
        assert_eq!(first["count"], json!(2));
        assert_eq!(first["has_more"], json!(true));
        let mut seqs: Vec<i64> = first["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["seq"].as_i64().unwrap())
            .collect();
        let mut types: Vec<String> = first["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_string())
            .collect();
        let mut after = first["next_after_seq"].as_i64().unwrap();
        loop {
            let page = call(
                &runs,
                "workflow_run_events",
                json!({"run_id": "job-1", "after_seq": after, "limit": 2}),
            )
            .unwrap();
            for event in page["events"].as_array().unwrap() {
                seqs.push(event["seq"].as_i64().unwrap());
                types.push(event["type"].as_str().unwrap().to_string());
            }
            after = page["next_after_seq"].as_i64().unwrap();
            if page["has_more"] == json!(false) {
                break;
            }
        }
        let expected: Vec<i64> = (seqs[0]..seqs[0] + seqs.len() as i64).collect();
        assert_eq!(seqs, expected, "contiguous history");
        assert_eq!(types.first().map(String::as_str), Some("run.created"));
        assert!(
            types.iter().any(|t| t == "run.segment_entered"),
            "{types:?}"
        );

        let listed = call(&runs, "workflow_run_list", json!({})).unwrap();
        assert_eq!(listed["runs"][0]["run_id"], json!("job-1"));
        assert_eq!(listed["has_more"], json!(false));

        // The very store `aad status` reads: a run started over MCP is not
        // private to the server that started it.
        let journal = JournalStore::open(runs.store_path()).expect("open");
        assert_eq!(
            journal.get_run("job-1").expect("run").status,
            RunStatus::Succeeded
        );

        // A run id names one run.
        let again = runs.start(
            &stub_session(),
            &json!({"name": "greeter", "durable": true, "run_id": "job-1"}),
        );
        let Err(StartError::Failed(payload)) = again else {
            panic!("a reused run id must be refused");
        };
        assert_eq!(parse(&payload)["code"], json!("MCP.RUN_EXISTS"));
    }

    #[test]
    fn a_workflow_durable_execution_refuses_is_reported_not_run_in_memory() {
        let store = ScratchStore::new("durable-refused");
        store.save("recorded", action_workflow());
        let runs = runs(&store);

        let Err(StartError::Refused(refusal)) = runs.start(
            &stub_session(),
            &json!({"name": "recorded", "durable": true}),
        ) else {
            panic!("an action step must be refused in the default mode");
        };
        assert_eq!(refusal["code"], json!("DURABLE.UNSUPPORTED_PLAN"));
        assert_eq!(refusal["effect"], json!("not_applied"));
        assert_eq!(refusal["run_created"], json!(false));
        assert!(refusal["hint"]
            .as_str()
            .unwrap()
            .contains("fallback_in_memory"));
        let listed = call(&runs, "workflow_run_list", json!({})).unwrap();
        assert_eq!(
            listed["count"],
            json!(0),
            "nothing may be created: {listed}"
        );

        // Through the server: a refusal is an error unless fallback was asked for.
        let leases = store.directory.join("leases");
        // SAFETY: serialised by the ScratchStore lock.
        unsafe { std::env::set_var("AAD_CONTROL_LEASE_DIR", &leases) };
        let server = crate::Server::local(
            aad_control::Session::new(
                Vec::new(),
                aad_control::Policy {
                    allow_input: true,
                    ..Default::default()
                },
            )
            .with_providers(vec![stub_driver()]),
        )
        .with_run_store(runs.store_path());
        let request = |arguments: Value| {
            let response = server
                .handle(&crate::Request {
                    id: Some(json!(1)),
                    method: "tools/call".into(),
                    params: json!({"name": "run_workflow", "arguments": arguments}),
                })
                .unwrap();
            let result = &response["result"];
            (
                result["isError"].as_bool().unwrap(),
                parse(result["content"][0]["text"].as_str().unwrap()),
            )
        };
        let (is_error, payload) = request(json!({"name": "recorded", "durable": true}));
        assert!(is_error);
        assert_eq!(payload["code"], json!("DURABLE.UNSUPPORTED_PLAN"));

        let (is_error, payload) = request(json!({
            "name": "recorded", "durable": true, "fallback_in_memory": true
        }));
        unsafe { std::env::remove_var("AAD_CONTROL_LEASE_DIR") };
        assert!(!is_error, "{payload}");
        assert_eq!(payload["kind"], json!("WorkflowRun"));
        assert_eq!(payload["status"], json!("succeeded"));
        assert_eq!(payload["durable"], json!(false));
        assert_eq!(
            payload["durable_refusal"]["code"],
            json!("DURABLE.UNSUPPORTED_PLAN")
        );
    }

    #[test]
    fn a_run_outlasting_the_call_comes_back_running_and_survives_the_server_leaving() {
        let store = ScratchStore::new("durable-long");
        store.save("slow", slow_workflow(20, 400));
        let runs = runs(&store);

        let run = started(
            &runs,
            json!({"name": "slow", "durable": true, "wait_ms": 0}),
        );
        let run_id = run["run_id"].as_str().unwrap().to_string();
        assert!(
            matches!(run["status"].as_str(), Some("pending" | "running")),
            "{run}"
        );
        assert_eq!(run["attached"], json!(true));
        assert!(run["next"]
            .as_str()
            .unwrap()
            .contains("workflow_run_status"));

        // The client goes away: the run stops at a boundary and lets go.
        // shutdown waits only briefly; on a loaded machine the runner may reach
        // its next step boundary after that, and pauses there all the same.
        runs.shutdown();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let status = loop {
            let status = call(&runs, "workflow_run_status", json!({"run_id": run_id})).unwrap();
            if status["status"] == json!("paused") || std::time::Instant::now() > deadline {
                break status;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        assert_eq!(status["status"], json!("paused"), "{status}");
        assert_eq!(status["attached"], json!(false));
        assert_eq!(status["owner"], Value::Null, "pausing releases the lease");

        // A later server (or `aad resume`) picks it up from the checkpoint.
        let later = Runs::default().with_store(runs.store_path());
        let resumed = call(
            &later,
            "workflow_run_resume",
            json!({"run_id": run_id, "name": "slow", "wait_ms": 60000}),
        )
        .unwrap();
        assert_eq!(resumed["status"], json!("succeeded"), "{resumed}");
        assert_eq!(
            resumed["outputs"]["total"],
            json!(20),
            "every step ran once"
        );
    }

    #[test]
    fn resume_after_a_crash_never_replays_the_interrupted_step() {
        let store = ScratchStore::new("durable-crash");
        let mut workflow = pure_workflow();
        workflow["steps"] = json!([
            {"id": "step0", "type": "set", "assign": {"vars.greeting": "a"}},
            {"id": "step1", "type": "set", "assign": {"vars.greeting": "b"}},
            {"id": "step2", "type": "set", "assign": {"vars.greeting": "c"}}
        ]);
        store.save("three", workflow);
        let runs = runs(&store);

        // Exactly what a server leaves behind when it dies inside a step: the
        // run is `running`, its checkpoint says a step was entered, and the
        // dead owner's lease has not lapsed yet.
        let (_, descriptor) = tools::compiled_workflow(&json!({"name": "three"})).unwrap();
        let digest = aad_runtime::plan_digest(&descriptor);
        let journal = JournalStore::open(runs.store_path()).expect("open");
        journal
            .create_run(
                "crashed",
                &descriptor.name,
                &json!({}),
                &descriptor.raw,
                None,
                Some(&digest),
                None,
            )
            .expect("create");
        let now = durable::now_seconds();
        let lease = journal
            .claim_owner("crashed", "mcp-dead", 0.5, now)
            .expect("claim");
        journal
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
            .expect("running");
        journal
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
                    "variables": {"greeting": "a"},
                }),
                None,
                None,
                now,
            )
            .expect("checkpoint");

        // While the dead owner's lease is live, nobody may take over.
        let fenced = call(
            &runs,
            "workflow_run_resume",
            json!({"run_id": "crashed", "name": "three"}),
        )
        .expect_err("a live lease fences a second runner");
        assert_eq!(fenced["code"], json!("JOURNAL.LEASE_CONFLICT"));
        assert!(fenced["hint"].as_str().unwrap().contains("lease"));
        let status = call(&runs, "workflow_run_status", json!({"run_id": "crashed"})).unwrap();
        assert_eq!(status["current_step"]["step_id"], json!("step1"));
        assert_eq!(status["current_step"]["phase"], json!("in_top_level_step"));

        std::thread::sleep(Duration::from_millis(700));
        let status = call(&runs, "workflow_run_status", json!({"run_id": "crashed"})).unwrap();
        assert_eq!(status["owner"]["lease_live"], json!(false));
        assert!(status["next"]
            .as_str()
            .unwrap()
            .contains("workflow_run_resume"));

        let reconciled = call(
            &runs,
            "workflow_run_resume",
            json!({"run_id": "crashed", "name": "three"}),
        )
        .expect("reconcile");
        assert_eq!(
            reconciled["status"],
            json!("unknown_effect"),
            "{reconciled}"
        );
        assert_eq!(reconciled["error"]["code"], json!("DURABLE.UNKNOWN_EFFECT"));
        assert_eq!(
            reconciled["unknown_effect"]["phase"],
            json!("in_top_level_step")
        );
        assert!(reconciled["unknown_effect"]["remedy"].is_string());
        assert!(reconciled["next"]
            .as_str()
            .unwrap()
            .contains("never retried"));

        // Nothing was dispatched while reconciling.
        let events = call(
            &runs,
            "workflow_run_events",
            json!({"run_id": "crashed", "limit": 200}),
        )
        .unwrap();
        let entered = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == json!("run.segment_entered"))
            .count();
        assert_eq!(entered, 1, "the interrupted step must not be re-entered");

        // And asking again does not get it replayed either.
        let again = call(
            &runs,
            "workflow_run_resume",
            json!({"run_id": "crashed", "name": "three"}),
        )
        .expect_err("terminal");
        assert_eq!(again["code"], json!("DURABLE.ALREADY_TERMINAL"));
        assert!(again["hint"].as_str().unwrap().contains("never replayed"));
    }

    #[test]
    fn cancel_stops_a_running_run_and_a_finished_run_refuses_control() {
        let store = ScratchStore::new("durable-cancel");
        store.save("slow", slow_workflow(20, 400));
        let runs = runs(&store);

        // A running run: the request is honoured at the next step boundary.
        let run = started(
            &runs,
            json!({"name": "slow", "durable": true, "wait_ms": 0}),
        );
        let run_id = run["run_id"].as_str().unwrap().to_string();
        let asked = call(&runs, "workflow_run_cancel", json!({"run_id": run_id})).unwrap();
        assert!(
            asked["desired_state"] == json!("cancel") || asked["status"] == json!("cancelled"),
            "{asked}"
        );
        let finished = wait_until(&runs, &run_id, |status| status["terminal"] == json!(true));
        assert_eq!(finished["status"], json!("cancelled"), "{finished}");
        assert_eq!(finished["attached"], json!(false));
        for tool in ["workflow_run_cancel", "workflow_run_pause"] {
            let refused = call(&runs, tool, json!({"run_id": run_id})).expect_err(tool);
            assert_eq!(refused["code"], json!("RUN.TERMINAL"), "{tool}");
        }

        // A paused run has no runner to honour a cancel, so resume finalises it.
        let run = started(
            &runs,
            json!({"name": "slow", "durable": true, "wait_ms": 0}),
        );
        let run_id = run["run_id"].as_str().unwrap().to_string();
        call(&runs, "workflow_run_pause", json!({"run_id": run_id})).unwrap();
        let paused = wait_until(&runs, &run_id, |status| status["attached"] == json!(false));
        assert_eq!(paused["status"], json!("paused"), "{paused}");
        let asked = call(&runs, "workflow_run_cancel", json!({"run_id": run_id})).unwrap();
        assert_eq!(asked["desired_state"], json!("cancel"));
        assert!(asked["next"]
            .as_str()
            .unwrap()
            .contains("workflow_run_resume"));
        // Asking twice is not a conflict, but a pause cannot walk a cancel back.
        call(&runs, "workflow_run_cancel", json!({"run_id": run_id})).unwrap();
        let downgraded =
            call(&runs, "workflow_run_pause", json!({"run_id": run_id})).expect_err("sticky");
        assert_eq!(downgraded["code"], json!("RUN.CANCEL_PENDING"));
        let finalised = call(
            &runs,
            "workflow_run_resume",
            json!({"run_id": run_id, "name": "slow"}),
        )
        .unwrap();
        assert_eq!(finalised["status"], json!("cancelled"), "{finalised}");
        let missing =
            call(&runs, "workflow_run_cancel", json!({"run_id": "ghost"})).expect_err("ghost");
        assert_eq!(missing["code"], json!("JOURNAL.RUN_NOT_FOUND"));
    }

    #[test]
    fn arguments_outside_the_schema_are_refused() {
        let store = ScratchStore::new("durable-arguments");
        store.save("greeter", pure_workflow());
        let runs = runs(&store);

        for (tool, arguments) in [
            ("workflow_run_status", json!({"run_id": "a", "extra": 1})),
            ("workflow_run_status", json!({"run_id": "../a"})),
            ("workflow_run_events", json!({"run_id": "a", "limit": 0})),
            ("workflow_run_list", json!({"status": "sleeping"})),
            (
                "workflow_run_resume",
                json!({"run_id": "a", "name": "greeter", "inputs": {}}),
            ),
        ] {
            let refused = call(&runs, tool, arguments.clone()).expect_err(tool);
            assert_eq!(
                refused["code"],
                json!("MCP.INVALID_ARGUMENT"),
                "{tool} {arguments}"
            );
        }
        for arguments in [
            json!({"name": "greeter", "durable": true, "durable_actions": "write"}),
            json!({"name": "greeter", "durable": true, "wait_ms": -1}),
            json!({"name": "greeter", "durable": true, "colour": "red"}),
        ] {
            let Err(StartError::Failed(payload)) = runs.start(&stub_session(), &arguments) else {
                panic!("must be refused: {arguments}");
            };
            assert_eq!(parse(&payload)["code"], json!("MCP.INVALID_ARGUMENT"));
        }
        // Durable-only arguments on an in-memory run are a mistake, not a no-op.
        let stray =
            tools::run_workflow(&stub_session(), &json!({"name": "greeter", "wait_ms": 10}))
                .expect_err("wait_ms without durable");
        assert_eq!(parse(&stray)["code"], json!("MCP.INVALID_ARGUMENT"));
    }

    #[test]
    fn run_tools_are_annotated_and_gated_by_what_they_do() {
        let listed = tools::list_payload();
        let read_only = |name: &str| -> bool {
            listed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["annotations"]["readOnlyHint"].as_bool())
                .unwrap_or_else(|| panic!("{name} is published"))
        };
        for name in NAMES {
            assert_eq!(read_only(name), !MUTATING.contains(name), "{name}");
        }

        // A read-only server lets an agent look at runs but not change them.
        let store = ScratchStore::new("durable-gated");
        let server = crate::Server::local(aad_control::Session::new(
            Vec::new(),
            aad_control::Policy::default(),
        ))
        .with_run_store(store.directory.join("runs.sqlite3"));
        let call = |name: &str, arguments: Value| {
            let response = server
                .handle(&crate::Request {
                    id: Some(json!(1)),
                    method: "tools/call".into(),
                    params: json!({"name": name, "arguments": arguments}),
                })
                .unwrap();
            let result = &response["result"];
            (
                result["isError"].as_bool().unwrap(),
                parse(result["content"][0]["text"].as_str().unwrap()),
            )
        };
        let (is_error, payload) = call("workflow_run_list", json!({}));
        assert!(!is_error, "{payload}");
        for name in MUTATING {
            let (is_error, payload) = call(name, json!({"run_id": "a", "name": "b"}));
            assert!(is_error, "{name}");
            assert_eq!(payload["code"], json!("POLICY.DENIED"), "{name}");
        }
    }
}
