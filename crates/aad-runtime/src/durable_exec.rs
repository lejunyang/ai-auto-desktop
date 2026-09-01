//! Durable execution: run a workflow so it survives a process restart.
//!
//! The executor drives the engine one top-level step at a time, committing a
//! checkpoint between segments. A crash therefore loses at most the segment in
//! flight, and a later process can resume from the last committed boundary.
//!
//! The safety rule that shapes everything here: **an interrupted segment is not
//! replayed.** When a process dies inside a step, the journal cannot know
//! whether the step's side effect reached the desktop. Retrying might click a
//! button twice; reporting failure might claim nothing happened when something
//! did. So a run interrupted mid-step is finalised as `UNKNOWN_EFFECT` with zero
//! further dispatch, and a human decides. That is why this module currently
//! accepts only workflows with no actions at all (`deny` mode): without the
//! `action_intent` machinery there is no way to prove a dispatch was safe to
//! repeat.
//!
//! Control is cooperative and honoured only at segment boundaries, which are the
//! points where the run's state is known and recorded.

use std::path::PathBuf;

use aad_core::{AutomationError, WorkflowDescriptor};
use serde_json::{json, Map, Value};

use crate::durable::{
    self, DesiredState, JournalError, JournalStore, OwnerLease, RunRecord, RunStatus,
};
use crate::engine::{self, RunOptions, Segment, SegmentState, Segmented};
use crate::journal::RunStatus as EngineStatus;

/// The checkpoint format. Bumped when its meaning changes, so a checkpoint
/// written by an incompatible version is refused rather than misread.
pub const CHECKPOINT_VERSION: u32 = 1;

/// How long a lease is held before it must be renewed.
///
/// Long enough that an ordinary slow step does not lose the run, short enough
/// that a crashed runner's work becomes claimable without a long wait.
pub const DEFAULT_LEASE_TTL_SECONDS: f64 = 60.0;

/// Where a run was when it was checkpointed.
///
/// The distinction drives recovery: only `BetweenSteps` is a safe place to
/// resume, because it is the only phase where no step was in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// A top-level step was executing. Its effect is unproven.
    InStep,
    /// No step was in flight; the next one has not begun.
    BetweenSteps,
    /// Workflow cleanup was running.
    Finalizing,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InStep => "in_top_level_step",
            Self::BetweenSteps => "between_top_level_steps",
            Self::Finalizing => "finalizing",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "in_top_level_step" => Some(Self::InStep),
            "between_top_level_steps" => Some(Self::BetweenSteps),
            "finalizing" => Some(Self::Finalizing),
            _ => None,
        }
    }

    /// Whether execution may continue from here without risking a repeat.
    fn is_resumable(self) -> bool {
        matches!(self, Self::BetweenSteps)
    }
}

/// Why a durable run stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// Reached a terminal status.
    Finished,
    /// Honoured a pause request at a segment boundary.
    Paused,
}

/// The outcome of a durable attempt.
#[derive(Clone, Debug)]
pub struct DurableOutcome {
    pub run: RunRecord,
    pub stopped: Stopped,
}

impl DurableOutcome {
    pub fn to_json(&self) -> Value {
        self.run.to_json()
    }
}

/// Options for a durable run.
pub struct DurableOptions {
    pub inputs: Map<String, Value>,
    pub owner_id: String,
    pub lease_ttl_seconds: f64,
    pub base_directory: PathBuf,
}

impl Default for DurableOptions {
    fn default() -> Self {
        Self {
            inputs: Map::new(),
            // Identifies which process holds the run, for diagnosis; the token
            // is what actually authorises writes.
            owner_id: format!("runner-{}", uuid::Uuid::new_v4().simple()),
            lease_ttl_seconds: DEFAULT_LEASE_TTL_SECONDS,
            base_directory: std::env::current_dir().unwrap_or_else(|_| ".".into()),
        }
    }
}

impl DurableOptions {
    pub fn with_inputs(mut self, inputs: Map<String, Value>) -> Self {
        self.inputs = inputs;
        self
    }

    pub fn with_owner_id(mut self, owner_id: impl Into<String>) -> Self {
        self.owner_id = owner_id.into();
        self
    }

    /// Hold the lease for this long between renewals.
    ///
    /// Shortening it makes a crashed runner's work claimable sooner, at the cost
    /// of losing the run if a single step outlives the TTL.
    pub fn with_lease_ttl_seconds(mut self, seconds: f64) -> Self {
        self.lease_ttl_seconds = seconds;
        self
    }

    pub fn with_base_directory(mut self, directory: PathBuf) -> Self {
        self.base_directory = directory;
        self
    }
}

/// Drives durable runs against a journal.
pub struct DurableExecutor {
    journal: JournalStore,
}

impl DurableExecutor {
    pub fn new(journal: JournalStore) -> Self {
        Self { journal }
    }

    pub fn journal(&self) -> &JournalStore {
        &self.journal
    }

    /// Create a run and execute it until it finishes or honours a pause.
    pub fn start(
        &self,
        descriptor: &WorkflowDescriptor,
        run_id: Option<&str>,
        options: DurableOptions,
    ) -> Result<DurableOutcome, AutomationError> {
        assert_durable_plan(descriptor)?;

        let run_id = run_id
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let digest = engine::plan_digest(descriptor);

        self.journal
            .create_run(
                &run_id,
                &descriptor.name,
                &Value::Object(options.inputs.clone()),
                &descriptor.raw,
                workflow_version(descriptor),
                Some(&digest),
                Some(("run.created", &json!({"workflow": descriptor.name}))),
            )
            .map_err(journal_error)?;

        let lease = self.claim(&run_id, &options)?;
        let inputs = options.inputs.clone();
        self.launch(descriptor, &digest, &run_id, lease, inputs, options)
    }

    /// Execute a run that was created separately and is still pending.
    ///
    /// Separating creation from execution is what lets an operator record intent
    /// against a run *before* anything is dispatched — including a pause or
    /// cancel that must be honoured before the first step.
    pub fn execute(
        &self,
        descriptor: &WorkflowDescriptor,
        run_id: &str,
        options: DurableOptions,
    ) -> Result<DurableOutcome, AutomationError> {
        assert_durable_plan(descriptor)?;
        let run = self.journal.get_run(run_id).map_err(journal_error)?;
        if run.status != RunStatus::Pending {
            return Err(durable_error(
                "DURABLE.INVALID_STATE",
                format!(
                    "run {run_id} is {} rather than pending",
                    run.status.as_str()
                ),
            )
            .with_detail("status", Value::String(run.status.as_str().into())));
        }
        let digest = engine::plan_digest(descriptor);
        if let Some(recorded) = &run.plan_digest {
            if recorded != &digest {
                return Err(durable_error(
                    "DURABLE.PLAN_MISMATCH",
                    "the descriptor does not match the plan this run was created from",
                )
                .with_detail("expected", Value::String(recorded.clone()))
                .with_detail("actual", Value::String(digest)));
            }
        }

        let lease = self.claim(run_id, &options)?;

        // A cancel recorded before execution began means nothing should be
        // dispatched at all, so honour it from `pending` directly.
        if run.desired_state == DesiredState::Cancel {
            return self.finalize_cancelled(run_id, lease, "cancelled before execution began");
        }

        let inputs = as_map(&run.inputs);
        self.launch(descriptor, &digest, run_id, lease, inputs, options)
    }

    /// Take a pending run to `running`, checkpoint it, and drive it.
    ///
    /// Shared by `start` and `execute` so a run created up front cannot end up
    /// with different budget or checkpoint semantics from one created inline.
    fn launch(
        &self,
        descriptor: &WorkflowDescriptor,
        digest: &str,
        run_id: &str,
        lease: OwnerLease,
        inputs: Map<String, Value>,
        options: DurableOptions,
    ) -> Result<DurableOutcome, AutomationError> {
        // The deadline is fixed now, in wall-clock terms, and stored in the
        // checkpoint. Time spent paused still counts against it: a run given
        // five minutes must not get five more just because it was resumed.
        let deadline = durable::now_seconds() + descriptor.budgets.max_duration;

        let run_options = RunOptions::default()
            .with_inputs(inputs)
            .with_base_directory(options.base_directory.clone());
        let mut segmented = Segmented::begin(descriptor, &run_options, deadline)?;

        let started = self
            .journal
            .set_status(
                &lease,
                RunStatus::Pending,
                RunStatus::Running,
                Some(("run.started", &json!({"ownerId": options.owner_id}))),
                None,
                None,
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;
        self.journal
            .append_event_with_checkpoint(
                &lease,
                "run.checkpointed",
                &json!({"phase": Phase::BetweenSteps.as_str()}),
                &encode_checkpoint(
                    digest,
                    Phase::BetweenSteps,
                    deadline,
                    &segmented.snapshot(),
                ),
                Some(RunStatus::Running),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;

        let _ = run_id;
        self.drive(digest, started, lease, &mut segmented, deadline, &options)
    }

    /// Resume a paused run, or reconcile one whose runner died.
    ///
    /// Two very different jobs behind one verb, because from the outside they
    /// look the same: a run that is not making progress. Which one applies is
    /// decided by the checkpoint's phase, not by guessing.
    pub fn resume(
        &self,
        descriptor: &WorkflowDescriptor,
        run_id: &str,
        options: DurableOptions,
    ) -> Result<DurableOutcome, AutomationError> {
        assert_durable_plan(descriptor)?;
        let run = self.journal.get_run(run_id).map_err(journal_error)?;

        if run.is_terminal() {
            return Err(durable_error(
                "DURABLE.ALREADY_TERMINAL",
                format!(
                    "run {run_id} already finished as {}",
                    run.status.as_str()
                ),
            )
            .with_detail("status", Value::String(run.status.as_str().into())));
        }

        // The plan must be the one that was checkpointed. Resuming a changed
        // workflow from an old checkpoint would execute a step the recorded
        // state never described.
        let digest = engine::plan_digest(descriptor);
        if let Some(recorded) = &run.plan_digest {
            if recorded != &digest {
                return Err(durable_error(
                    "DURABLE.PLAN_MISMATCH",
                    "the descriptor does not match the plan this run was created from",
                )
                .with_detail("expected", Value::String(recorded.clone()))
                .with_detail("actual", Value::String(digest)));
            }
        }

        // Claim before inspecting the checkpoint. Ownership is the more
        // fundamental objection: a runner trying to take over a run somebody
        // else is actively executing must be told the run is owned, not given a
        // verdict about a checkpoint it has no business reading yet. It also
        // stops two runners from both deciding what to do with the same run.
        let lease = self.claim(run_id, &options)?;

        // Record the takeover before deciding anything, so the audit trail names
        // the new owner even when the decision is "this cannot be continued".
        // Writing it only on the resumable path would leave the most alarming
        // outcome -- a run finalised as UNKNOWN_EFFECT -- with no trace of which
        // process made that call.
        if run.status == RunStatus::Running {
            self.journal
                .append_event(
                    &lease,
                    "run.reclaimed",
                    &json!({"ownerId": options.owner_id, "previousStatus": run.status.as_str()}),
                    durable::now_seconds(),
                )
                .map_err(journal_error)?;
        }

        // Intent first among the things this runner may now decide: a cancel
        // requested while the run was down is honoured without executing
        // anything further.
        if run.desired_state == DesiredState::Cancel {
            return self.finalize_cancelled(run_id, lease, "cancelled before resuming");
        }

        let Some(raw) = &run.checkpoint else {
            return Err(durable_error(
                "DURABLE.NO_CHECKPOINT",
                format!("run {run_id} has no checkpoint to resume from"),
            ));
        };
        let (phase, deadline, state) = decode_checkpoint(raw, &digest)?;

        // A run interrupted inside a step, or during cleanup, cannot be
        // continued: whether its side effect landed is unknowable from here.
        // Finalise it as UNKNOWN_EFFECT with no further dispatch and let a
        // person decide, rather than guessing on their behalf.
        if !phase.is_resumable() {
            return self.finalize_unknown_effect(run_id, lease, phase, &state);
        }

        // Resuming does not reset the budget; the original deadline stands and
        // may already have passed.
        if deadline <= durable::now_seconds() {
            return self.finalize_timed_out(run_id, lease, deadline);
        }

        // Asking to resume *is* asking to run, so clear a standing pause request
        // now. Without this the drive loop reads the still-recorded pause at its
        // first boundary and stops again immediately, leaving the run unable to
        // make progress no matter how many times it is resumed.
        //
        // Deliberately after the unsafe-recovery and deadline checks above: a run
        // that cannot be continued must not have its operator's intent quietly
        // rewritten on the way to being refused. A cancel is never cleared here —
        // it is sticky, and was already honoured above.
        let run = if run.desired_state == DesiredState::Pause {
            self.journal
                .compare_and_set_desired_state(
                    run_id,
                    DesiredState::Pause,
                    DesiredState::Run,
                    Some((
                        "run.resume_requested",
                        &json!({
                            "fromDesiredState": DesiredState::Pause.as_str(),
                            "toDesiredState": DesiredState::Run.as_str(),
                        }),
                    )),
                )
                .map_err(journal_error)?
        } else {
            run
        };

        let run_options = RunOptions::default()
            .with_inputs(as_map(&run.inputs))
            .with_base_directory(options.base_directory.clone());
        let mut segmented = Segmented::restore(descriptor, &run_options, deadline, state)?;

        let running = if run.status == RunStatus::Paused {
            self.journal
                .set_status(
                    &lease,
                    RunStatus::Paused,
                    RunStatus::Running,
                    Some(("run.resumed", &json!({"ownerId": options.owner_id}))),
                    None,
                    None,
                    None,
                    durable::now_seconds(),
                )
                .map_err(journal_error)?
        } else {
            // Already `running` on paper: its previous runner died without
            // releasing the lease, which we took over above.
            self.journal.get_run(run_id).map_err(journal_error)?
        };

        self.drive(
            &digest,
            running,
            lease,
            &mut segmented,
            deadline,
            &options,
        )
    }

    fn claim(
        &self,
        run_id: &str,
        options: &DurableOptions,
    ) -> Result<OwnerLease, AutomationError> {
        self.journal
            .claim_owner(
                run_id,
                &options.owner_id,
                options.lease_ttl_seconds,
                durable::now_seconds(),
            )
            .map_err(journal_error)
    }

    /// Execute segments until the run finishes or stops cooperatively.
    #[allow(clippy::too_many_arguments)]
    fn drive(
        &self,
        digest: &str,
        mut run: RunRecord,
        mut lease: OwnerLease,
        segmented: &mut Segmented<'_>,
        deadline: f64,
        options: &DurableOptions,
    ) -> Result<DurableOutcome, AutomationError> {
        loop {
            // Renew before each segment. If the lease has gone the run belongs
            // to somebody else now and this process must stop writing.
            lease = self
                .journal
                .heartbeat_owner(&lease, options.lease_ttl_seconds, durable::now_seconds())
                .map_err(journal_error)?;

            // Re-read intent at the boundary. This is the only place a pause or
            // cancel can be honoured while the state is known and recorded.
            let current = self.journal.get_run(&run.run_id).map_err(journal_error)?;
            match current.desired_state {
                DesiredState::Pause => {
                    return self.honour_pause(digest, lease, segmented, deadline);
                }
                DesiredState::Cancel => {
                    // Nothing is in flight here, so the cancellation is clean
                    // and can be reported as such.
                    return self.finalize_cancelled(
                        &run.run_id,
                        lease,
                        "cancelled at a segment boundary",
                    );
                }
                DesiredState::Run => {}
            }
            run = current;

            let Some(step_id) = segmented.next_step_id().map(str::to_string) else {
                return self.finalize_body(digest, lease, segmented, deadline, Ok(()));
            };

            // Record that a step is about to run *before* running it. If the
            // process dies now, recovery sees `in_top_level_step` and knows the
            // effect is unproven instead of assuming nothing happened.
            self.journal
                .append_event_with_checkpoint(
                    &lease,
                    "run.segment_entered",
                    &json!({"stepId": step_id}),
                    &encode_checkpoint(
                        digest,
                        Phase::InStep,
                        deadline,
                        &segmented.snapshot(),
                    ),
                    Some(RunStatus::Running),
                    Some(DesiredState::Run),
                    durable::now_seconds(),
                )
                .map_err(journal_error)?;

            let outcome = segmented.run_segment();
            let progressed = match outcome {
                Ok(segment) => segment,
                Err(error) => {
                    // The step itself failed. That is an ordinary outcome, so
                    // give the workflow's handler and cleanup their chance.
                    return self.finalize_body(
                        digest,
                        lease,
                        segmented,
                        deadline,
                        Err(error),
                    );
                }
            };

            // Back at a boundary: record the new state as safe to resume from.
            self.journal
                .append_event_with_checkpoint(
                    &lease,
                    "run.segment_exited",
                    &json!({"stepId": step_id}),
                    &encode_checkpoint(
                        digest,
                        Phase::BetweenSteps,
                        deadline,
                        &segmented.snapshot(),
                    ),
                    Some(RunStatus::Running),
                    None,
                    durable::now_seconds(),
                )
                .map_err(journal_error)?;

            if progressed != Segment::Advanced {
                return self.finalize_body(digest, lease, segmented, deadline, Ok(()));
            }
        }
    }

    /// Stop at a boundary, leaving the run resumable.
    fn honour_pause(
        &self,
        digest: &str,
        lease: OwnerLease,
        segmented: &Segmented<'_>,
        deadline: f64,
    ) -> Result<DurableOutcome, AutomationError> {
        // Checkpoint before pausing, so what is stored is exactly the boundary
        // being paused at.
        self.journal
            .append_event_with_checkpoint(
                &lease,
                "run.checkpointed",
                &json!({"phase": Phase::BetweenSteps.as_str()}),
                &encode_checkpoint(
                    digest,
                    Phase::BetweenSteps,
                    deadline,
                    &segmented.snapshot(),
                ),
                Some(RunStatus::Running),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;

        // Reaching `paused` releases the lease in the same commit, so whoever
        // resumes can claim it immediately.
        let run = self
            .journal
            .set_status(
                &lease,
                RunStatus::Running,
                RunStatus::Paused,
                Some((
                    "run.paused",
                    &json!({"nextStepId": segmented.next_step_id()}),
                )),
                None,
                None,
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;
        Ok(DurableOutcome {
            run,
            stopped: Stopped::Paused,
        })
    }

    /// Run workflow cleanup and commit the terminal status.
    #[allow(clippy::too_many_arguments)]
    fn finalize_body(
        &self,
        digest: &str,
        lease: OwnerLease,
        segmented: &mut Segmented<'_>,
        deadline: f64,
        body: Result<(), AutomationError>,
    ) -> Result<DurableOutcome, AutomationError> {
        // Cleanup can itself be interrupted, and its steps may have effects, so
        // the phase is recorded before it starts.
        self.journal
            .append_event_with_checkpoint(
                &lease,
                "run.finalizing",
                &json!({}),
                &encode_checkpoint(
                    digest,
                    Phase::Finalizing,
                    deadline,
                    &segmented.snapshot(),
                ),
                Some(RunStatus::Running),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;

        // The engine owns handler and cleanup semantics; duplicating them here
        // would let durable and ordinary runs drift apart.
        let result = segmented.finish(body);

        let status = match result.status {
            EngineStatus::Succeeded => RunStatus::Succeeded,
            EngineStatus::Failed => RunStatus::Failed,
            EngineStatus::TimedOut => RunStatus::TimedOut,
            EngineStatus::Cancelled => RunStatus::Cancelled,
            // Never folded into a plain failure: the caller must be able to see
            // that a side effect may have happened.
            EngineStatus::UnknownEffect => RunStatus::UnknownEffect,
        };
        let error = result
            .error
            .as_ref()
            .map(AutomationError::to_json)
            .unwrap_or_else(|| json!({"code": "RUNTIME.UNSPECIFIED"}));
        let output = Value::Object(result.outputs.clone());

        let run = self
            .journal
            .set_status(
                &lease,
                RunStatus::Running,
                status,
                Some((
                    "run.finished",
                    &json!({"status": status.as_str(), "executedSteps": result.executed_steps}),
                )),
                (status == RunStatus::Succeeded).then_some(&output),
                (status != RunStatus::Succeeded).then_some(&error),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;
        Ok(DurableOutcome {
            run,
            stopped: Stopped::Finished,
        })
    }

    /// Finalise a run whose in-flight effect cannot be established.
    ///
    /// No steps are dispatched, not even cleanup: this process has no idea what
    /// the dead one had already done, and cleanup could compound it.
    fn finalize_unknown_effect(
        &self,
        run_id: &str,
        lease: OwnerLease,
        phase: Phase,
        state: &SegmentState,
    ) -> Result<DurableOutcome, AutomationError> {
        let error = json!({
            "code": "DURABLE.UNKNOWN_EFFECT",
            "message": format!(
                "the run was interrupted during {} and its effect cannot be established",
                phase.as_str()
            ),
            "category": "durable",
            "effect": "unknown",
            "retryable": false,
            "details": {
                "phase": phase.as_str(),
                "nextTopLevelIndex": state.next_index,
                "remedy": "inspect the target application and decide whether to \
                           repeat the interrupted step",
            },
        });
        self.terminate(
            run_id,
            lease,
            RunStatus::UnknownEffect,
            ("run.unknown_effect", json!({"phase": phase.as_str()})),
            error,
        )
    }

    /// Finalise a run cancelled at a point where nothing was in flight.
    ///
    /// Safe to report as a clean `CANCELLED` precisely because it is reached
    /// only from a segment boundary, or before any segment ran. A cancel that
    /// arrives mid-dispatch is not handled here: that path ends as
    /// `UNKNOWN_EFFECT`, because the effect genuinely is unknown.
    fn finalize_cancelled(
        &self,
        run_id: &str,
        lease: OwnerLease,
        reason: &str,
    ) -> Result<DurableOutcome, AutomationError> {
        let error = json!({
            "code": "WORKFLOW.CANCELLED",
            "message": reason,
            "category": "workflow",
            "effect": "not_applied",
            "retryable": false,
        });
        self.terminate(
            run_id,
            lease,
            RunStatus::Cancelled,
            ("run.cancelled", json!({"reason": reason})),
            error,
        )
    }

    fn finalize_timed_out(
        &self,
        run_id: &str,
        lease: OwnerLease,
        deadline: f64,
    ) -> Result<DurableOutcome, AutomationError> {
        let error = json!({
            "code": "WORKFLOW.TIMEOUT",
            "message": "the run exceeded its maximum duration",
            "category": "workflow",
            "effect": "not_applied",
            "retryable": false,
            "details": {"deadline": deadline},
        });
        self.terminate(
            run_id,
            lease,
            RunStatus::TimedOut,
            ("run.timed_out", json!({"deadline": deadline})),
            error,
        )
    }

    /// Commit a terminal status from whatever non-terminal state the run is in.
    ///
    /// The expected status is read rather than assumed: these paths are reached
    /// both from a `running` run and from a `paused` one being reconciled, and
    /// guessing wrong turns a legitimate finalisation into a spurious conflict.
    /// Reading it inside the same call still leaves the compare-and-set intact,
    /// because `set_status` re-checks it under the writer lock.
    fn terminate(
        &self,
        run_id: &str,
        lease: OwnerLease,
        status: RunStatus,
        event: (&str, Value),
        error: Value,
    ) -> Result<DurableOutcome, AutomationError> {
        let current = self.journal.get_run(run_id).map_err(journal_error)?;
        if current.is_terminal() {
            return Err(durable_error(
                "DURABLE.ALREADY_TERMINAL",
                format!(
                    "run {run_id} already finished as {}",
                    current.status.as_str()
                ),
            ));
        }
        let (event_type, payload) = event;
        let run = self
            .journal
            .set_status(
                &lease,
                current.status,
                status,
                Some((event_type, &payload)),
                None,
                Some(&error),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;
        Ok(DurableOutcome {
            run,
            stopped: Stopped::Finished,
        })
    }
}

/// Whether this workflow can be executed durably at all.
///
/// Conservative by design. Every rejection here is a case where the executor
/// could not honestly account for what happened after a crash, so refusing up
/// front is better than discovering it mid-run.
pub fn assert_durable_plan(descriptor: &WorkflowDescriptor) -> Result<(), AutomationError> {
    if !durable::durable_descriptor_eligible(&descriptor.raw) {
        return Err(durable_error(
            "DURABLE.SENSITIVE_DESCRIPTOR",
            "durable execution rejects workflows declaring sensitive inputs or outputs",
        ));
    }
    // Concurrent top-level steps would make "the next index" meaningless as a
    // resume point.
    if descriptor.budgets.max_concurrency != 1 {
        return Err(durable_error(
            "DURABLE.UNSUPPORTED_PLAN",
            "durable execution requires max_concurrency=1",
        )
        .with_detail(
            "max_concurrency",
            json!(descriptor.budgets.max_concurrency),
        ));
    }
    // An explicit top-level `depends_on` can reorder steps in ways a single
    // "next index" cannot express.
    if let Some(steps) = descriptor.raw.get("steps").and_then(Value::as_array) {
        if steps
            .iter()
            .any(|step| step.get("depends_on").is_some())
        {
            return Err(durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable execution requires implicit sequential top-level steps",
            ));
        }
    }

    // Actions and scripts have effects this mode cannot reason about after an
    // interruption. Naming them is what makes the refusal actionable.
    let unsupported: Vec<String> = descriptor
        .all_steps()
        .iter()
        .filter(|step| {
            matches!(
                step.step_type,
                aad_core::model::StepType::Action | aad_core::model::StepType::Script
            )
        })
        .map(|step| step.id.clone())
        .collect();
    if !unsupported.is_empty() {
        return Err(durable_error(
            "DURABLE.UNSUPPORTED_PLAN",
            "durable execution currently rejects action and script steps because an \
             interrupted dispatch cannot be proven safe to repeat",
        )
        .with_detail("unsupportedSteps", json!(unsupported)));
    }
    Ok(())
}

fn workflow_version(descriptor: &WorkflowDescriptor) -> Option<&str> {
    descriptor
        .metadata
        .get("version")
        .and_then(Value::as_str)
}

/// Serialise the resumable state.
///
/// The schema and runtime versions plus the plan digest are recorded so a
/// checkpoint written by an incompatible build, or against a different
/// workflow, is detected on read instead of misinterpreted.
fn encode_checkpoint(
    digest: &str,
    phase: Phase,
    deadline: f64,
    state: &SegmentState,
) -> Value {
    json!({
        "checkpointVersion": CHECKPOINT_VERSION,
        "runtimeVersion": engine::RUNTIME_VERSION,
        "planDigest": digest,
        "phase": phase.as_str(),
        "deadline": deadline,
        "nextTopLevelIndex": state.next_index,
        "executedSteps": state.executed_steps,
        "unknownEffect": state.unknown_effect,
        "variables": Value::Object(state.variables.clone()),
        "steps": Value::Object(state.steps.clone()),
        "returned": state.returned,
    })
}

/// Read a checkpoint back, refusing anything that cannot be trusted.
fn decode_checkpoint(
    raw: &Value,
    expected_digest: &str,
) -> Result<(Phase, f64, SegmentState), AutomationError> {
    let version = raw
        .get("checkpointVersion")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if version != CHECKPOINT_VERSION as u64 {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_UNSUPPORTED",
            format!(
                "checkpoint version {version} cannot be read by this build \
                 (expected {CHECKPOINT_VERSION})"
            ),
        ));
    }
    if let Some(digest) = raw.get("planDigest").and_then(Value::as_str) {
        if digest != expected_digest {
            return Err(durable_error(
                "DURABLE.PLAN_MISMATCH",
                "the checkpoint was written against a different plan",
            )
            .with_detail("expected", Value::String(digest.into()))
            .with_detail("actual", Value::String(expected_digest.into())));
        }
    }
    let phase = raw
        .get("phase")
        .and_then(Value::as_str)
        .and_then(Phase::parse)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "the checkpoint does not record a recognised phase",
            )
        })?;
    // A missing deadline must not become "no limit"; refuse instead.
    let deadline = raw.get("deadline").and_then(Value::as_f64).ok_or_else(|| {
        durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "the checkpoint does not record an absolute deadline",
        )
    })?;

    let state = SegmentState {
        variables: as_map(raw.get("variables").unwrap_or(&Value::Null)),
        steps: as_map(raw.get("steps").unwrap_or(&Value::Null)),
        next_index: raw
            .get("nextTopLevelIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        executed_steps: raw.get("executedSteps").and_then(Value::as_u64).unwrap_or(0),
        unknown_effect: raw
            .get("unknownEffect")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        returned: match raw.get("returned") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.clone()),
        },
    };
    Ok((phase, deadline, state))
}

fn as_map(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn durable_error(code: &str, message: impl Into<String>) -> AutomationError {
    AutomationError::new(code, message)
        .with_category("durable")
        .with_effect("not_applied")
}

/// Translate a journal failure into the automation error shape, preserving the
/// distinction callers act on.
fn journal_error(error: JournalError) -> AutomationError {
    let effect = match &error {
        // A refused write changed nothing, so the caller can retry or report
        // safely; only genuine ambiguity gets `unknown`.
        JournalError::Storage(_) => "unknown",
        _ => "not_applied",
    };
    let retryable = matches!(
        error,
        JournalError::Conflict(_) | JournalError::LeaseConflict(_) | JournalError::Storage(_)
    );
    AutomationError::new(error.code(), error.message())
        .with_category("durable")
        .with_effect(effect)
        .with_retryable(retryable)
}
