//! Durable execution: run a workflow so it survives a process restart.
//!
//! The executor drives the engine one top-level step at a time, committing a
//! checkpoint between segments. A crash therefore loses at most the segment in
//! flight, and a later process can resume from the last committed boundary.
//!
//! The safety rule that shapes everything here: **an interrupted segment is not
//! replayed unless a validated intent proves that replay is safe.** When a
//! process dies inside an ordinary step, the journal cannot know whether the
//! step's side effect reached the desktop. Retrying might click a button twice;
//! reporting failure might claim nothing happened when something did. Such a
//! run is finalised as `UNKNOWN_EFFECT` with zero further dispatch. The explicit
//! `read-only` mode is the narrow exception: a top-level observation with a
//! durable `action_intent` and public output projection can be validated and
//! safely replayed after a crash.
//!
//! Control is cooperative and honoured only at segment boundaries, which are the
//! points where the run's state is known and recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use aad_core::{AutomationError, WorkflowDescriptor};
use serde_json::{json, Map, Value};

use crate::durable::{
    self, DesiredState, JournalError, JournalStore, OwnerLease, RunRecord, RunStatus,
};
use crate::engine::{self, FinalizationIntent, RunOptions, Segment, SegmentState, Segmented};
use crate::journal::RunStatus as EngineStatus;
use crate::provider::ProviderRegistry;

/// The checkpoint format. Bumped when its meaning changes, so a checkpoint
/// written by an incompatible version is refused rather than misread.
pub const CHECKPOINT_VERSION: u32 = 2;
const LEGACY_CHECKPOINT_VERSION: u32 = 1;

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
    /// A read-only action has a durable, validated dispatch intent.
    ActionIntent,
    /// Workflow cleanup was running.
    Finalizing,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InStep => "in_top_level_step",
            Self::BetweenSteps => "between_top_level_steps",
            Self::ActionIntent => "action_intent",
            Self::Finalizing => "finalizing",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "in_top_level_step" => Some(Self::InStep),
            "between_top_level_steps" => Some(Self::BetweenSteps),
            "action_intent" => Some(Self::ActionIntent),
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
    pub providers: ProviderRegistry,
    pub granted_permissions: BTreeSet<String>,
    pub action_mode: DurableActionMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DurableActionMode {
    #[default]
    Deny,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinalizationStage {
    Intent,
    Started,
    Result,
}

impl FinalizationStage {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "intent" => Some(Self::Intent),
            "started" => Some(Self::Started),
            "result" => Some(Self::Result),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct FinalizedRun {
    status: RunStatus,
    output: Option<Value>,
    error: Option<Value>,
    executed_steps: u64,
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
            providers: ProviderRegistry::new(),
            granted_permissions: BTreeSet::new(),
            action_mode: DurableActionMode::Deny,
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

    pub fn with_providers(mut self, providers: ProviderRegistry) -> Self {
        self.providers = providers;
        self
    }

    pub fn with_granted_permissions<I, S>(mut self, permissions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.granted_permissions = permissions.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_action_mode(mut self, mode: DurableActionMode) -> Self {
        self.action_mode = mode;
        self
    }
}

#[derive(Clone, Debug)]
struct DurableBinding {
    contract: aad_plugin::manifest::ActionContract,
    provider_digest: String,
    contract_digest: String,
    projection_digest: String,
    selected: Vec<String>,
    definitions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
struct PreparedAction {
    step: aad_core::model::CompiledStep,
    binding: DurableBinding,
    binding_digest: String,
    args: Value,
    dispatch_deadline_ms: u64,
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
        assert_durable_plan_with_mode(descriptor, options.action_mode)?;
        self.require_file_journal(descriptor, options.action_mode)?;
        preflight_actions(descriptor, &options)?;

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
        assert_durable_plan_with_mode(descriptor, options.action_mode)?;
        self.require_file_journal(descriptor, options.action_mode)?;
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
        preflight_actions(descriptor, &options)?;
        let digest = engine::plan_digest(descriptor);
        if run.plan_digest.as_deref() != Some(digest.as_str()) {
            return Err(durable_error(
                "DURABLE.PLAN_MISMATCH",
                "the descriptor does not match the plan this run was created from",
            )
            .with_detail("expected", json!(run.plan_digest))
            .with_detail("actual", json!(digest)));
        }

        let lease = self.claim(run_id, &options)?;

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
            .with_base_directory(options.base_directory.clone())
            .with_providers(options.providers.clone())
            .with_granted_permissions(options.granted_permissions.clone());
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
                &encode_checkpoint(digest, Phase::BetweenSteps, deadline, &segmented.snapshot()),
                Some(RunStatus::Running),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;

        if started.desired_state == DesiredState::Cancel {
            return self.finalize_body(
                digest,
                lease,
                &mut segmented,
                deadline,
                Err(cancelled_error("cancelled before execution began")),
            );
        }
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
        assert_durable_plan_with_mode(descriptor, options.action_mode)?;
        self.require_file_journal(descriptor, options.action_mode)?;
        let run = self.journal.get_run(run_id).map_err(journal_error)?;

        if run.is_terminal() {
            return Err(durable_error(
                "DURABLE.ALREADY_TERMINAL",
                format!("run {run_id} already finished as {}", run.status.as_str()),
            )
            .with_detail("status", Value::String(run.status.as_str().into())));
        }
        // The plan must be the one that was checkpointed. Resuming a changed
        // workflow from an old checkpoint would execute a step the recorded
        // state never described.
        let digest = engine::plan_digest(descriptor);
        if run.plan_digest.as_deref() != Some(digest.as_str()) {
            return Err(durable_error(
                "DURABLE.PLAN_MISMATCH",
                "the descriptor does not match the plan this run was created from",
            )
            .with_detail("expected", json!(run.plan_digest))
            .with_detail("actual", json!(digest)));
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

        let Some(raw) = run.checkpoint.clone() else {
            if run.desired_state == DesiredState::Cancel {
                return self.finalize_cancelled(run_id, lease, "cancelled before resuming");
            }
            return Err(durable_error(
                "DURABLE.NO_CHECKPOINT",
                format!("run {run_id} has no checkpoint to resume from"),
            ));
        };
        let (phase, deadline, state) = decode_checkpoint(&raw, &digest)?;
        let finalization = finalization_stage(&raw)?;

        // A completed finalization is already a durable terminal decision. No
        // runner or provider is needed, and cleanup must not be replayed.
        if finalization == Some(FinalizationStage::Result) {
            let result = decode_finalized_run(&raw, state.executed_steps)?;
            return self.commit_finalized(run_id, lease, result);
        }
        // Once cleanup may have started, its effect is as unknowable as an
        // interrupted body step. A concurrent cancel must not disguise that.
        if finalization == Some(FinalizationStage::Started) {
            return self.finalize_unknown_effect(run_id, lease, "finalization_started", &state);
        }

        // A run interrupted inside a step, or during cleanup, cannot be
        // continued: whether its side effect landed is unknowable from here.
        // Finalise it as UNKNOWN_EFFECT with no further dispatch and let a
        // person decide, rather than guessing on their behalf.
        if !phase.is_resumable()
            && phase != Phase::ActionIntent
            && finalization != Some(FinalizationStage::Intent)
        {
            return self.finalize_unknown_effect(run_id, lease, phase.as_str(), &state);
        }
        if run.desired_state == DesiredState::Cancel && phase == Phase::ActionIntent {
            return self.finalize_cancelled(run_id, lease, "cancelled before resuming");
        }

        // Asking to resume is asking to run, so clear a standing pause request.
        let run = if run.desired_state == DesiredState::Pause {
            match self.journal.compare_and_set_desired_state(
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
            ) {
                Ok(run) => run,
                // Losing this CAS means somebody else cleared the pause between
                // the read above and this write. The point was for the run not
                // to be paused, and it is not paused -- so failing here would
                // report an error for the state the caller asked for and got.
                // Re-read and carry on; a cancel that arrived instead is still
                // honoured at the next boundary, where it is checked anyway.
                Err(JournalError::Conflict(_)) => {
                    self.journal.get_run(run_id).map_err(journal_error)?
                }
                Err(error) => return Err(journal_error(error)),
            }
        } else {
            run
        };

        let run_options = RunOptions::default()
            .with_inputs(as_map(&run.inputs))
            .with_base_directory(options.base_directory.clone())
            .with_providers(options.providers.clone())
            .with_granted_permissions(options.granted_permissions.clone());
        preflight_actions(descriptor, &options)?;
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

        if finalization == Some(FinalizationStage::Intent) {
            let intent = decode_finalization_intent(&raw)?;
            if intent.error.is_none() && segmented.next_step().is_some() {
                return Err(durable_error(
                    "DURABLE.CHECKPOINT_INVALID",
                    "successful finalization intent still has unexecuted steps",
                ));
            }
            return self.finalize_prepared(
                &digest,
                &running.run_id,
                lease,
                &mut segmented,
                deadline,
                intent,
            );
        }
        if running.desired_state == DesiredState::Cancel {
            return self.finalize_body(
                &digest,
                lease,
                &mut segmented,
                deadline,
                Err(cancelled_error("cancelled before resuming")),
            );
        }
        // Resuming does not reset the body budget. A persisted finalization
        // intent has already crossed into cleanup, which has its own deadline.
        if finalization.is_none() && deadline <= durable::now_seconds() {
            return self.finalize_body(
                &digest,
                lease,
                &mut segmented,
                deadline,
                Err(workflow_timeout_error(deadline)),
            );
        }
        if phase == Phase::ActionIntent {
            return self.resume_action_intent(
                &digest,
                running,
                lease,
                &mut segmented,
                deadline,
                &options,
                &raw,
            );
        }
        self.drive(&digest, running, lease, &mut segmented, deadline, &options)
    }

    fn claim(&self, run_id: &str, options: &DurableOptions) -> Result<OwnerLease, AutomationError> {
        self.journal
            .claim_owner(
                run_id,
                &options.owner_id,
                options.lease_ttl_seconds,
                durable::now_seconds(),
            )
            .map_err(journal_error)
    }

    fn require_file_journal(
        &self,
        descriptor: &WorkflowDescriptor,
        mode: DurableActionMode,
    ) -> Result<(), AutomationError> {
        if mode == DurableActionMode::ReadOnly
            && descriptor
                .steps
                .iter()
                .any(|step| step.step_type == aad_core::model::StepType::Action)
            && self.journal.path == std::path::Path::new(":memory:")
        {
            return Err(durable_error(
                "DURABLE.UNSUPPORTED_JOURNAL",
                "durable read-only actions require a file-backed journal for lease heartbeats",
            ));
        }
        Ok(())
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
                    // Nothing is in flight here. Preserve ordinary runtime
                    // semantics by running workflow-level cleanup durably.
                    return self.finalize_body(
                        digest,
                        lease,
                        segmented,
                        deadline,
                        Err(cancelled_error("cancelled at a segment boundary")),
                    );
                }
                DesiredState::Run => {}
            }
            run = current;

            let Some(step_id) = segmented.next_step_id().map(str::to_string) else {
                return self.finalize_body(digest, lease, segmented, deadline, Ok(()));
            };
            if segmented
                .next_step()
                .is_some_and(|step| step.step_type == aad_core::model::StepType::Action)
            {
                return self
                    .dispatch_read_only_action(digest, run, lease, segmented, deadline, options);
            }

            // Record that a step is about to run *before* running it. If the
            // process dies now, recovery sees `in_top_level_step` and knows the
            // effect is unproven instead of assuming nothing happened.
            //
            // Requiring `desired_state = run` here is what makes this write the
            // dispatch authorisation: a pause or cancel that arrived since the
            // read above must not be overtaken by a step going out anyway.
            let dispatched = self.journal.append_event_with_checkpoint(
                &lease,
                "run.segment_entered",
                &json!({"stepId": step_id}),
                &encode_checkpoint(digest, Phase::InStep, deadline, &segmented.snapshot()),
                Some(RunStatus::Running),
                Some(DesiredState::Run),
                durable::now_seconds(),
            );
            if let Err(JournalError::Conflict(_)) = &dispatched {
                // Losing that CAS is the mechanism working, not a fault: an
                // operator asked to stop in the window between the read above and
                // this write. Route into the control path rather than failing the
                // run -- the request was granted, so reporting an error would be a
                // lie, and no step has been dispatched.
                let current = self.journal.get_run(&run.run_id).map_err(journal_error)?;
                match current.desired_state {
                    DesiredState::Pause => {
                        return self.honour_pause(digest, lease, segmented, deadline);
                    }
                    DesiredState::Cancel => {
                        return self.finalize_cancelled(
                            &run.run_id,
                            lease,
                            "cancelled before the next segment was dispatched",
                        );
                    }
                    // Intent is unchanged, so the conflict was about something
                    // else -- a status this process no longer agrees with. Not
                    // ours to absorb.
                    DesiredState::Run => {}
                }
            }
            dispatched.map_err(journal_error)?;

            let outcome = segmented.run_segment();
            let progressed = match outcome {
                Ok(segment) => segment,
                Err(error) => {
                    // The step itself failed. That is an ordinary outcome, so
                    // give the workflow's handler and cleanup their chance.
                    return self.finalize_body(digest, lease, segmented, deadline, Err(error));
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

    fn dispatch_read_only_action(
        &self,
        digest: &str,
        run: RunRecord,
        lease: OwnerLease,
        segmented: &mut Segmented<'_>,
        deadline: f64,
        options: &DurableOptions,
    ) -> Result<DurableOutcome, AutomationError> {
        let prepared = match prepare_action(segmented, deadline) {
            Ok(prepared) => prepared,
            Err(error) => {
                let safe = AutomationError::new(
                    "DURABLE.ACTION_PREPARATION_FAILED",
                    "durable action preparation failed before dispatch",
                )
                .with_category("durable")
                .with_effect("not_applied")
                .with_detail("stepId", json!(segmented.next_step_id()))
                .with_detail("reason", json!(error.code));
                return self.finalize_body(digest, lease, segmented, deadline, Err(safe));
            }
        };
        let state = segmented.reserve_action_attempt()?;
        let operation_id = uuid::Uuid::new_v4().simple().to_string();
        let checkpoint = encode_action_intent(digest, deadline, &state, &prepared, &operation_id);
        let persisted = self.journal.append_event_with_checkpoint(
            &lease,
            "run.action_intent",
            &json!({"stepId": prepared.step.id, "operationId": operation_id}),
            &checkpoint,
            Some(RunStatus::Running),
            Some(DesiredState::Run),
            durable::now_seconds(),
        );
        if let Err(JournalError::Conflict(_)) = persisted {
            segmented.release_action_attempt()?;
            let current = self.journal.get_run(&run.run_id).map_err(journal_error)?;
            return match current.desired_state {
                DesiredState::Pause => self.honour_pause(digest, lease, segmented, deadline),
                DesiredState::Cancel => self.finalize_cancelled(
                    &run.run_id,
                    lease,
                    "cancelled before a read-only action was dispatched",
                ),
                DesiredState::Run => Err(durable_error(
                    "DURABLE.STATE_CONFLICT",
                    "action dispatch authorization lost its expected state",
                )),
            };
        }
        persisted.map_err(journal_error)?;
        self.authorize_and_run_action(
            digest, run, lease, segmented, deadline, options, prepared, checkpoint,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn authorize_and_run_action(
        &self,
        digest: &str,
        run: RunRecord,
        mut lease: OwnerLease,
        segmented: &mut Segmented<'_>,
        deadline: f64,
        options: &DurableOptions,
        prepared: PreparedAction,
        checkpoint: Value,
    ) -> Result<DurableOutcome, AutomationError> {
        let operation_id = checkpoint["actionIntent"]["operationId"].clone();
        let authorized = self.journal.append_event_with_checkpoint(
            &lease,
            "run.action_dispatch_authorized",
            &json!({"stepId": prepared.step.id, "operationId": operation_id}),
            &checkpoint,
            Some(RunStatus::Running),
            Some(DesiredState::Run),
            durable::now_seconds(),
        );
        if let Err(JournalError::Conflict(_)) = authorized {
            let current = self.journal.get_run(&run.run_id).map_err(journal_error)?;
            return match current.desired_state {
                DesiredState::Pause => {
                    let paused = self
                        .journal
                        .set_status(
                            &lease,
                            RunStatus::Running,
                            RunStatus::Paused,
                            Some(("run.paused", &json!({"nextStepId": prepared.step.id}))),
                            None,
                            None,
                            Some(DesiredState::Pause),
                            durable::now_seconds(),
                        )
                        .map_err(journal_error)?;
                    Ok(DurableOutcome {
                        run: paused,
                        stopped: Stopped::Paused,
                    })
                }
                DesiredState::Cancel => self.finalize_cancelled(
                    &run.run_id,
                    lease,
                    "cancelled before a read-only action was dispatched",
                ),
                DesiredState::Run => Err(durable_error(
                    "DURABLE.STATE_CONFLICT",
                    "action dispatch authorization lost its expected state",
                )),
            };
        }
        authorized.map_err(journal_error)?;

        let result = self.invoke_with_lease_heartbeat(segmented, &prepared, &lease, options);
        lease = self
            .journal
            .heartbeat_owner(&lease, options.lease_ttl_seconds, durable::now_seconds())
            .map_err(|_| {
                AutomationError::new(
                    "DURABLE.LEASE_HEARTBEAT_FAILED",
                    "durable action lease was lost after provider completion",
                )
                .with_category("durable")
                .with_effect("unknown")
            })?;
        let progressed = match segmented.run_reserved_action(result) {
            Ok(segment) => segment,
            Err(error) => {
                return self.finalize_body(digest, lease, segmented, deadline, Err(error));
            }
        };
        let boundary =
            encode_checkpoint(digest, Phase::BetweenSteps, deadline, &segmented.snapshot());
        self.journal
            .append_event_with_checkpoint(
                &lease,
                "run.segment_exited",
                &json!({
                    "stepId": prepared.step.id,
                    "operationId": operation_id,
                }),
                &boundary,
                Some(RunStatus::Running),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;
        let intent = self.journal.get_run(&run.run_id).map_err(journal_error)?;
        if intent.desired_state == DesiredState::Pause {
            return self.honour_pause(digest, lease, segmented, deadline);
        }
        if intent.desired_state == DesiredState::Cancel {
            return self.finalize_cancelled(
                &run.run_id,
                lease,
                "cancelled after a read-only action completed",
            );
        }
        if progressed != Segment::Advanced {
            return self.finalize_body(digest, lease, segmented, deadline, Ok(()));
        }
        self.drive(digest, run, lease, segmented, deadline, options)
    }

    fn invoke_with_lease_heartbeat(
        &self,
        segmented: &Segmented<'_>,
        prepared: &PreparedAction,
        lease: &OwnerLease,
        options: &DurableOptions,
    ) -> Result<Value, AutomationError> {
        let path = self.journal.path.clone();
        let held = lease.clone();
        let ttl = options.lease_ttl_seconds;
        let (stop_sender, stop_receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let keeper = std::thread::spawn(move || -> Result<(), JournalError> {
            let journal = match JournalStore::open_with_timeout(&path, 500) {
                Ok(journal) => journal,
                Err(error) => {
                    let _ = ready_sender.send(false);
                    return Err(error);
                }
            };
            if let Err(error) = journal.heartbeat_owner(&held, ttl, durable::now_seconds()) {
                let _ = ready_sender.send(false);
                return Err(error);
            }
            let _ = ready_sender.send(true);
            let interval = Duration::from_secs_f64((ttl / 4.0).max(0.05));
            loop {
                match stop_receiver.recv_timeout(interval) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        journal.heartbeat_owner(&held, ttl, durable::now_seconds())?;
                    }
                }
            }
        });
        let ready_timeout = Duration::from_secs_f64((ttl / 2.0).clamp(0.05, 1.0));
        if ready_receiver.recv_timeout(ready_timeout) != Ok(true) {
            let _ = stop_sender.send(());
            let _ = keeper.join();
            return Err(AutomationError::new(
                "DURABLE.LEASE_HEARTBEAT_FAILED",
                "durable action lease heartbeat failed before dispatch",
            )
            .with_category("durable")
            .with_effect("not_applied"));
        }
        let result = invoke_durable_action(segmented, prepared);
        let _ = stop_sender.send(());
        let heartbeat = keeper.join().map_err(|_| {
            durable_error(
                "DURABLE.LEASE_HEARTBEAT_FAILED",
                "durable action lease heartbeat thread failed",
            )
        })?;
        if heartbeat.is_err() {
            // The caller performs a synchronous fenced heartbeat immediately
            // after this returns. Preserve the provider result if that succeeds:
            // a temporary keeper-connection failure does not make a read-only
            // observation ambiguous. This marker is diagnostic-only.
            let _ = self.journal.append_event(
                lease,
                "run.lease_heartbeat_failed",
                &json!({"recovered": true}),
                durable::now_seconds(),
            );
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn resume_action_intent(
        &self,
        digest: &str,
        run: RunRecord,
        lease: OwnerLease,
        segmented: &mut Segmented<'_>,
        deadline: f64,
        options: &DurableOptions,
        checkpoint: &Value,
    ) -> Result<DurableOutcome, AutomationError> {
        let mut prepared = prepare_action(segmented, deadline)?;
        prepared.dispatch_deadline_ms = checkpoint
            .get("actionIntent")
            .and_then(|value| value.get("dispatchDeadlineEpochMs"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                durable_error(
                    "DURABLE.CHECKPOINT_INVALID",
                    "action intent dispatch deadline is invalid",
                )
            })?;
        validate_action_intent(checkpoint, &prepared, segmented.snapshot().executed_steps)?;
        self.authorize_and_run_action(
            digest,
            run,
            lease,
            segmented,
            deadline,
            options,
            prepared,
            checkpoint.clone(),
        )
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
                &encode_checkpoint(digest, Phase::BetweenSteps, deadline, &segmented.snapshot()),
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

    /// Persist the pre-cleanup boundary, run cleanup once, persist its result,
    /// and then commit the terminal status.
    #[allow(clippy::too_many_arguments)]
    fn finalize_body(
        &self,
        digest: &str,
        lease: OwnerLease,
        segmented: &mut Segmented<'_>,
        deadline: f64,
        body: Result<(), AutomationError>,
    ) -> Result<DurableOutcome, AutomationError> {
        let intent = segmented.prepare_finalization(body);
        let run_id = lease.run_id.clone();
        self.finalize_prepared(digest, &run_id, lease, segmented, deadline, intent)
    }

    fn finalize_prepared(
        &self,
        digest: &str,
        run_id: &str,
        lease: OwnerLease,
        segmented: &mut Segmented<'_>,
        deadline: f64,
        intent: FinalizationIntent,
    ) -> Result<DurableOutcome, AutomationError> {
        let intent_checkpoint = encode_finalization_intent(
            digest,
            deadline,
            &segmented.snapshot(),
            &intent,
            FinalizationStage::Intent,
        );
        self.journal
            .append_event_with_checkpoint(
                &lease,
                "run.finalization_intent",
                &json!({}),
                &intent_checkpoint,
                Some(RunStatus::Running),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;
        let started_checkpoint = encode_finalization_intent(
            digest,
            deadline,
            &segmented.snapshot(),
            &intent,
            FinalizationStage::Started,
        );
        self.journal
            .append_event_with_checkpoint(
                &lease,
                "run.finalization_started",
                &json!({}),
                &started_checkpoint,
                Some(RunStatus::Running),
                None,
                durable::now_seconds(),
            )
            .map_err(journal_error)?;

        // The engine owns handler and cleanup semantics; duplicating them here
        // would let durable and ordinary runs drift apart.
        let result = segmented.finish_prepared(intent);
        let finalized = finalized_run(&result);
        let result_checkpoint =
            encode_finalization_result(digest, deadline, &segmented.snapshot(), &finalized);
        let completed = self.journal.append_event_with_checkpoint(
            &lease,
            "run.finalization_completed",
            &json!({"status": finalized.status.as_str()}),
            &result_checkpoint,
            Some(RunStatus::Running),
            Some(DesiredState::Run),
            durable::now_seconds(),
        );
        if let Err(JournalError::Conflict(_)) = completed {
            // A pause/cancel can arrive after cleanup has completed. Persist the
            // definitive result before applying that intent, so a later resume
            // never repeats cleanup.
            self.journal
                .append_event_with_checkpoint(
                    &lease,
                    "run.finalization_completed",
                    &json!({"status": finalized.status.as_str()}),
                    &result_checkpoint,
                    Some(RunStatus::Running),
                    None,
                    durable::now_seconds(),
                )
                .map_err(journal_error)?;
        } else {
            completed.map_err(journal_error)?;
        }
        self.commit_finalized(run_id, lease, finalized)
    }

    fn commit_finalized(
        &self,
        run_id: &str,
        lease: OwnerLease,
        finalized: FinalizedRun,
    ) -> Result<DurableOutcome, AutomationError> {
        for _ in 0..16 {
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

            let (status, output, error, event_type, payload) = if current.desired_state
                == DesiredState::Cancel
                && finalized.status == RunStatus::Succeeded
            {
                let error =
                    cancelled_error("cancelled after workflow finalization completed").to_json();
                (
                    RunStatus::Cancelled,
                    None,
                    Some(error),
                    "run.cancelled",
                    json!({"reason": "cancelled after workflow finalization completed"}),
                )
            } else {
                (
                    finalized.status,
                    finalized.output.clone(),
                    finalized.error.clone(),
                    "run.finished",
                    json!({
                        "status": finalized.status.as_str(),
                        "executedSteps": finalized.executed_steps,
                    }),
                )
            };
            match self.journal.set_status(
                &lease,
                current.status,
                status,
                Some((event_type, &payload)),
                output.as_ref(),
                error.as_ref(),
                Some(current.desired_state),
                durable::now_seconds(),
            ) {
                Ok(run) => {
                    return Ok(DurableOutcome {
                        run,
                        stopped: Stopped::Finished,
                    });
                }
                Err(JournalError::Conflict(_)) => continue,
                Err(error) => return Err(journal_error(error)),
            }
        }
        Err(durable_error(
            "DURABLE.STATE_CONFLICT",
            "control state did not stabilize during terminal commit",
        ))
    }

    /// Finalise a run whose in-flight effect cannot be established.
    ///
    /// No steps are dispatched, not even cleanup: this process has no idea what
    /// the dead one had already done, and cleanup could compound it.
    fn finalize_unknown_effect(
        &self,
        run_id: &str,
        lease: OwnerLease,
        phase: &str,
        state: &SegmentState,
    ) -> Result<DurableOutcome, AutomationError> {
        let error = json!({
            "code": "DURABLE.UNKNOWN_EFFECT",
            "message": format!(
                "the run was interrupted during {} and its effect cannot be established",
                phase
            ),
            "category": "durable",
            "effect": "unknown",
            "retryable": false,
            "details": {
                "phase": phase,
                "nextTopLevelIndex": state.next_index,
                "remedy": "inspect the target application and decide whether to \
                           repeat the interrupted step",
            },
        });
        self.terminate(
            run_id,
            lease,
            RunStatus::UnknownEffect,
            ("run.unknown_effect", json!({"phase": phase})),
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

fn prepare_action(
    segmented: &Segmented<'_>,
    workflow_deadline: f64,
) -> Result<PreparedAction, AutomationError> {
    let step = segmented
        .next_step()
        .cloned()
        .ok_or_else(|| durable_error("DURABLE.INVALID_STATE", "no durable action is ready"))?;
    let binding = durable_binding(
        segmented.descriptor(),
        segmented.providers(),
        segmented.granted_permissions(),
        &step,
    )?;
    let args = match step.get("with") {
        Some(value) => crate::template::resolve(value, &segmented.scope())?,
        None => Value::Object(Map::new()),
    };
    engine::validate_schema(
        &args,
        binding.contract.input_schema.as_ref(),
        "ACTION.INPUT_INVALID",
        step.get_str("uses").unwrap_or_default(),
        true,
    )?;
    let dispatch_deadline_ms = action_deadline_ms(&step, &binding.contract, workflow_deadline);
    let binding_digest = engine::digest_json(&json!({
        "uses": step.get_str("uses"),
        "input": args,
        "providerDigest": binding.provider_digest,
        "contractDigest": binding.contract_digest,
        "projectionDigest": binding.projection_digest,
    }));
    Ok(PreparedAction {
        step,
        binding,
        binding_digest,
        args,
        dispatch_deadline_ms,
    })
}

fn durable_binding(
    descriptor: &WorkflowDescriptor,
    providers: &ProviderRegistry,
    granted_permissions: &BTreeSet<String>,
    step: &aad_core::model::CompiledStep,
) -> Result<DurableBinding, AutomationError> {
    let uses = step.get_str("uses").unwrap_or_default();
    let Some((_, contract)) = providers.resolve(uses) else {
        return Err(durable_error(
            "CAPABILITY.MISSING",
            format!("no provider offers action {uses:?}"),
        ));
    };
    let provider_name = uses
        .rsplit_once('.')
        .map(|(provider, _)| provider)
        .unwrap_or_default();
    let provider = providers.get(provider_name).ok_or_else(|| {
        durable_error(
            "CAPABILITY.MISSING",
            format!("provider {provider_name:?} is missing"),
        )
    })?;
    engine::enforce_action_policy(
        // The descriptor is already represented by the action and its provider
        // inside Segmented; use the workflow reached through the step's policy
        // checks in the ordinary engine.
        descriptor,
        granted_permissions,
        provider.manifest(),
        contract,
        step,
    )?;
    if contract.has_artifacts() {
        return Err(durable_error(
            "DURABLE.UNSUPPORTED_PLAN",
            "durable actions cannot use ephemeral artifact transport",
        )
        .with_detail("uses", json!(uses)));
    }
    let descriptor_effect = step
        .get("effect")
        .and_then(Value::as_object)
        .and_then(|effect| effect.get("class"))
        .and_then(Value::as_str);
    if contract.effect_class.as_deref() != Some("read_only")
        || descriptor_effect.is_some_and(|effect| effect != "read_only")
    {
        return Err(durable_error(
            "DURABLE.UNSUPPORTED_PLAN",
            "durable action must be explicitly read-only in both contracts",
        )
        .with_detail("uses", json!(uses)));
    }
    if contract.errors.is_empty()
        || contract
            .errors
            .iter()
            .any(|error| error.get("effect").and_then(Value::as_str) != Some("not_applied"))
    {
        return Err(durable_error(
            "DURABLE.UNSUPPORTED_PLAN",
            "durable action errors must be non-empty and not_applied",
        )
        .with_detail("uses", json!(uses)));
    }
    let public = |value: Option<&Value>| {
        value.and_then(Value::as_object).is_some_and(|map| {
            ["input", "output", "error"]
                .iter()
                .all(|field| map.get(*field).and_then(Value::as_str) == Some("public"))
        })
    };
    if !public(contract.sensitivity.as_ref()) || !public(step.get("sensitivity")) {
        return Err(durable_error(
            "DURABLE.SENSITIVE_ACTION",
            "durable action input, output, and error must be explicitly public",
        )
        .with_detail("uses", json!(uses)));
    }
    let provider_fields = contract
        .durability
        .as_ref()
        .and_then(|value| value.get("checkpoint_fields"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action provider has no checkpoint field whitelist",
            )
        })?;
    if provider_fields.len() > 128 {
        return Err(durable_error(
            "DURABLE.UNSUPPORTED_PLAN",
            "durable action declares too many checkpoint fields",
        ));
    }
    let mut pointers = BTreeSet::new();
    for (alias, definition) in provider_fields {
        if !valid_identifier(alias) {
            return Err(durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable checkpoint field alias is invalid",
            )
            .with_detail("field", json!(alias)));
        }
        let definition = definition.as_object().ok_or_else(|| {
            durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable checkpoint field is invalid",
            )
        })?;
        if !definition.contains_key("schema")
            || definition
                .keys()
                .any(|key| !matches!(key.as_str(), "pointer" | "schema" | "missing"))
        {
            return Err(durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable checkpoint field shape is invalid",
            )
            .with_detail("field", json!(alias)));
        }
        let pointer = definition
            .get("pointer")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                durable_error(
                    "DURABLE.UNSUPPORTED_PLAN",
                    "durable checkpoint field pointer is missing",
                )
            })?;
        if !valid_json_pointer(pointer) || pointer.len() > 1024 || !pointers.insert(pointer) {
            return Err(durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable checkpoint pointers must be unique, non-root, and bounded",
            )
            .with_detail("field", json!(alias)));
        }
        let missing = definition
            .get("missing")
            .and_then(Value::as_str)
            .unwrap_or("error");
        if !matches!(missing, "error" | "omit" | "null") {
            return Err(durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable checkpoint missing policy is invalid",
            )
            .with_detail("field", json!(alias)));
        }
        if missing == "null" {
            engine::validate_schema(
                &Value::Null,
                definition.get("schema"),
                "DURABLE.UNSUPPORTED_PLAN",
                alias,
                true,
            )?;
        }
    }
    let checkpoint = step
        .get("checkpoint")
        .and_then(Value::as_object)
        .and_then(|value| value.get("output"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action checkpoint output must be explicit",
            )
        })?;
    let mode = checkpoint.get("mode").and_then(Value::as_str);
    let selected = match mode {
        Some("omit") => Vec::new(),
        Some("project") => {
            let fields: Vec<String> = checkpoint
                .get("fields")
                .and_then(Value::as_array)
                .map(|fields| {
                    fields
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if fields.is_empty()
                || fields
                    .iter()
                    .any(|field| !provider_fields.contains_key(field))
            {
                return Err(durable_error(
                    "DURABLE.UNSUPPORTED_PLAN",
                    "durable action projection is not provider-approved",
                ));
            }
            fields
        }
        _ => {
            return Err(durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action projection is not provider-approved",
            ));
        }
    };
    let definitions: BTreeMap<String, Value> = selected
        .iter()
        .map(|field| (field.clone(), provider_fields[field].clone()))
        .collect();
    let projection = json!({
        "mode": mode, "fields": selected, "definitions": definitions
    });
    Ok(DurableBinding {
        contract: contract.clone(),
        provider_digest: engine::digest_json(&provider.manifest().raw),
        contract_digest: engine::digest_json(&contract.raw),
        projection_digest: engine::digest_json(&projection),
        selected,
        definitions,
    })
}

fn valid_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn valid_json_pointer(value: &str) -> bool {
    if value.is_empty() || !value.starts_with('/') || value.len() > 1024 {
        return false;
    }
    let tokens: Vec<&str> = value.split('/').skip(1).collect();
    if tokens.len() > 64 {
        return false;
    }
    tokens.iter().all(|token| {
        let bytes = token.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'~' {
                if index + 1 >= bytes.len() || !matches!(bytes[index + 1], b'0' | b'1') {
                    return false;
                }
                index += 2;
            } else {
                index += 1;
            }
        }
        true
    })
}

fn preflight_actions(
    descriptor: &WorkflowDescriptor,
    options: &DurableOptions,
) -> Result<(), AutomationError> {
    if options.action_mode != DurableActionMode::ReadOnly {
        return Ok(());
    }
    for step in &descriptor.steps {
        if step.step_type == aad_core::model::StepType::Action {
            durable_binding(
                descriptor,
                &options.providers,
                &options.granted_permissions,
                step,
            )
            .map_err(|error| {
                durable_error(
                    "DURABLE.ACTION_PREFLIGHT_FAILED",
                    "durable action provider preflight failed",
                )
                .with_detail("stepId", json!(step.id))
                .with_detail("reason", json!(error.code))
            })?;
        }
    }
    Ok(())
}

fn action_deadline_ms(
    step: &aad_core::model::CompiledStep,
    contract: &aad_plugin::manifest::ActionContract,
    workflow_deadline: f64,
) -> u64 {
    let now = durable::now_seconds();
    let mut deadline = workflow_deadline;
    for text in [
        step.get_str("timeout"),
        step.get_str("attempt_timeout"),
        contract.timeout.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(seconds) = aad_core::parse_duration(text) {
            deadline = deadline.min(now + seconds);
        }
    }
    (deadline.max(0.0) * 1000.0) as u64
}

fn invoke_durable_action(
    segmented: &Segmented<'_>,
    prepared: &PreparedAction,
) -> Result<Value, AutomationError> {
    let uses = prepared.step.get_str("uses").unwrap_or_default();
    let Some((provider, _)) = segmented.providers().resolve(uses) else {
        return Err(durable_error(
            "CAPABILITY.MISSING",
            "durable action provider disappeared",
        ));
    };
    let now_ms = (durable::now_seconds() * 1000.0) as u64;
    if prepared.dispatch_deadline_ms <= now_ms {
        return Err(AutomationError::new(
            "ACTION.TIMEOUT",
            "durable action deadline expired before dispatch",
        )
        .with_category("action")
        .with_effect("not_applied"));
    }
    let timeout = Duration::from_millis(prepared.dispatch_deadline_ms - now_ms);
    let output = provider
        .invoke(uses, prepared.args.clone(), Some(timeout))
        .map_err(|error| {
            if error.code == "PLUGIN.HOST_TIMEOUT" {
                return AutomationError::new(
                    "ACTION.TIMEOUT",
                    "durable read-only action timed out",
                )
                .with_category("action")
                .with_effect("not_applied");
            }
            let declared =
                prepared.binding.contract.errors.iter().any(|item| {
                    item.get("code").and_then(Value::as_str) == Some(error.code.as_str())
                });
            if declared {
                AutomationError::new(
                    error.code,
                    "durable action failed with a declared provider error",
                )
                .with_category("action")
                .with_effect("not_applied")
            } else {
                AutomationError::new(
                    "ACTION.UNDECLARED_ERROR",
                    "durable action returned an undeclared error",
                )
                .with_category("action")
                .with_effect("unknown")
            }
        })?;
    engine::validate_schema(
        &output,
        prepared.binding.contract.output_schema.as_ref(),
        "ACTION.OUTPUT_INVALID",
        uses,
        true,
    )?;
    project_output(&prepared.binding, &output)
}

fn project_output(binding: &DurableBinding, output: &Value) -> Result<Value, AutomationError> {
    if binding.selected.is_empty() {
        return Ok(Value::Null);
    }
    let mut projected = Map::new();
    for field in &binding.selected {
        let definition = binding
            .definitions
            .get(field)
            .and_then(Value::as_object)
            .ok_or_else(|| {
                durable_error(
                    "DURABLE.BINDING_MISMATCH",
                    "checkpoint field definition is invalid",
                )
            })?;
        let pointer = definition
            .get("pointer")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                durable_error(
                    "DURABLE.BINDING_MISMATCH",
                    "checkpoint field pointer is invalid",
                )
            })?;
        let value = match output.pointer(pointer) {
            Some(value) => value.clone(),
            None if definition.get("missing").and_then(Value::as_str) == Some("omit") => continue,
            None if definition.get("missing").and_then(Value::as_str) == Some("null") => {
                Value::Null
            }
            None => {
                return Err(AutomationError::new(
                    "ACTION.OUTPUT_INVALID",
                    "durable checkpoint field is missing",
                )
                .with_category("action")
                .with_effect("not_applied"))
            }
        };
        engine::validate_schema(
            &value,
            definition.get("schema"),
            "ACTION.OUTPUT_INVALID",
            field,
            true,
        )?;
        projected.insert(field.clone(), value);
    }
    Ok(Value::Object(projected))
}

fn encode_action_intent(
    digest: &str,
    deadline: f64,
    state: &SegmentState,
    prepared: &PreparedAction,
    operation_id: &str,
) -> Value {
    let mut checkpoint = encode_checkpoint(digest, Phase::ActionIntent, deadline, state);
    checkpoint["actionIntent"] = json!({
        "version": 2,
        "operationId": operation_id,
        "stepId": prepared.step.id,
        "reservationOrdinal": state.executed_steps,
        "attempt": 1,
        "dispatchDeadlineEpochMs": prepared.dispatch_deadline_ms,
        "providerDigest": prepared.binding.provider_digest,
        "contractDigest": prepared.binding.contract_digest,
        "projectionDigest": prepared.binding.projection_digest,
        "bindingDigest": prepared.binding_digest,
    });
    checkpoint
}

fn encode_finalization_intent(
    digest: &str,
    deadline: f64,
    state: &SegmentState,
    intent: &FinalizationIntent,
    stage: FinalizationStage,
) -> Value {
    let mut checkpoint = encode_checkpoint(digest, Phase::Finalizing, deadline, state);
    checkpoint["finalization"] = json!({
        "version": 1,
        "stage": match stage {
            FinalizationStage::Intent => "intent",
            FinalizationStage::Started => "started",
            FinalizationStage::Result => "result",
        },
        "outputSet": intent.error.is_none(),
        "output": if intent.error.is_none() {
            Value::Object(intent.outputs.clone())
        } else {
            Value::Null
        },
        "error": intent
            .error
            .as_ref()
            .map(AutomationError::to_json)
            .unwrap_or(Value::Null),
    });
    checkpoint
}

fn finalized_run(result: &crate::journal::RunResult) -> FinalizedRun {
    let status = match result.status {
        EngineStatus::Succeeded => RunStatus::Succeeded,
        EngineStatus::Failed => RunStatus::Failed,
        EngineStatus::TimedOut => RunStatus::TimedOut,
        EngineStatus::Cancelled => RunStatus::Cancelled,
        EngineStatus::UnknownEffect => RunStatus::UnknownEffect,
    };
    let error = if status == RunStatus::Succeeded {
        None
    } else {
        Some(
            result
                .error
                .as_ref()
                .map(AutomationError::to_json)
                .unwrap_or_else(|| {
                    durable_error(
                        "DURABLE.TERMINAL",
                        format!("workflow ended as {}", status.as_str()),
                    )
                    .to_json()
                }),
        )
    };
    FinalizedRun {
        status,
        output: (status == RunStatus::Succeeded).then(|| Value::Object(result.outputs.clone())),
        error,
        executed_steps: result.executed_steps,
    }
}

fn encode_finalization_result(
    digest: &str,
    deadline: f64,
    state: &SegmentState,
    result: &FinalizedRun,
) -> Value {
    let mut checkpoint = encode_checkpoint(digest, Phase::Finalizing, deadline, state);
    checkpoint["finalization"] = json!({
        "version": 1,
        "stage": "result",
        "result": {
            "status": result.status.as_str(),
            "output": result.output,
            "error": result.error,
            "executedSteps": result.executed_steps,
        },
    });
    checkpoint
}

fn finalization_stage(checkpoint: &Value) -> Result<Option<FinalizationStage>, AutomationError> {
    let Some(finalization) = checkpoint.get("finalization") else {
        return Ok(None);
    };
    let finalization = finalization.as_object().ok_or_else(|| {
        durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "finalization payload must be an object",
        )
    })?;
    if finalization.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "finalization payload version is unsupported",
        ));
    }
    let stage = finalization
        .get("stage")
        .and_then(Value::as_str)
        .and_then(FinalizationStage::parse)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "finalization stage is invalid",
            )
        })?;
    Ok(Some(stage))
}

fn decode_finalization_intent(checkpoint: &Value) -> Result<FinalizationIntent, AutomationError> {
    let finalization = checkpoint["finalization"].as_object().ok_or_else(|| {
        durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "finalization intent payload is missing",
        )
    })?;
    let expected: BTreeSet<&str> = ["version", "stage", "outputSet", "output", "error"]
        .into_iter()
        .collect();
    if finalization
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != expected
        || finalization.get("stage").and_then(Value::as_str) != Some("intent")
    {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "finalization intent fields are invalid",
        ));
    }
    let output_set = finalization
        .get("outputSet")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "finalization outputSet must be a boolean",
            )
        })?;
    let output = finalization.get("output").cloned().unwrap_or(Value::Null);
    let error = decode_optional_error(finalization.get("error"))?;
    if !output_set && !output.is_null() || output_set != error.is_none() {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "finalization intent output and error are inconsistent",
        ));
    }
    let outputs = if output_set {
        output.as_object().cloned().ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "finalization output must be an object",
            )
        })?
    } else {
        Map::new()
    };
    Ok(FinalizationIntent { outputs, error })
}

fn decode_finalized_run(
    checkpoint: &Value,
    fallback_executed_steps: u64,
) -> Result<FinalizedRun, AutomationError> {
    let result = checkpoint["finalization"]["result"]
        .as_object()
        .ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "finalization result payload is missing",
            )
        })?;
    let expected: BTreeSet<&str> = ["status", "output", "error", "executedSteps"]
        .into_iter()
        .collect();
    if result.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "finalization result fields are invalid",
        ));
    }
    let status = match result.get("status").and_then(Value::as_str) {
        Some("succeeded") => RunStatus::Succeeded,
        Some("failed") => RunStatus::Failed,
        Some("timed_out") => RunStatus::TimedOut,
        Some("cancelled") => RunStatus::Cancelled,
        Some("unknown_effect") => RunStatus::UnknownEffect,
        _ => {
            return Err(durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "finalization result status is invalid",
            ));
        }
    };
    let output = result
        .get("output")
        .cloned()
        .filter(|value| !value.is_null());
    let error = result
        .get("error")
        .cloned()
        .filter(|value| !value.is_null());
    if status == RunStatus::Succeeded {
        if error.is_some() || output.as_ref().is_none_or(|value| !value.is_object()) {
            return Err(durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "successful finalization result is invalid",
            ));
        }
    } else if output.is_some() || error.as_ref().is_none_or(|value| !valid_error_json(value)) {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "failed finalization result is invalid",
        ));
    }
    let executed_steps = result
        .get("executedSteps")
        .and_then(Value::as_u64)
        .filter(|value| *value == fallback_executed_steps)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "finalization result attempt count does not match its checkpoint",
            )
        })?;
    Ok(FinalizedRun {
        status,
        output,
        error,
        executed_steps,
    })
}

fn decode_optional_error(
    value: Option<&Value>,
) -> Result<Option<AutomationError>, AutomationError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => AutomationError::from_json(value).map(Some).ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "finalization error payload is invalid",
            )
        }),
    }
}

fn valid_error_json(value: &Value) -> bool {
    AutomationError::from_json(value).is_some()
}

fn validate_action_intent(
    checkpoint: &Value,
    prepared: &PreparedAction,
    executed_steps: u64,
) -> Result<(), AutomationError> {
    let intent = checkpoint
        .get("actionIntent")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "action intent payload is missing",
            )
        })?;
    let expected_fields: BTreeSet<&str> = [
        "version",
        "operationId",
        "stepId",
        "reservationOrdinal",
        "attempt",
        "dispatchDeadlineEpochMs",
        "providerDigest",
        "contractDigest",
        "projectionDigest",
        "bindingDigest",
    ]
    .into_iter()
    .collect();
    let actual_fields: BTreeSet<&str> = intent.keys().map(String::as_str).collect();
    let digest = |name: &str| {
        intent
            .get(name)
            .and_then(Value::as_str)
            .is_some_and(|value| {
                value.len() == 71
                    && value.starts_with("sha256:")
                    && value[7..]
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    };
    let matches = intent.get("version").and_then(Value::as_u64) == Some(2)
        && actual_fields == expected_fields
        && intent
            .get("operationId")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty() && value.len() <= 128)
        && digest("providerDigest")
        && digest("contractDigest")
        && digest("projectionDigest")
        && digest("bindingDigest")
        && intent.get("stepId").and_then(Value::as_str) == Some(prepared.step.id.as_str())
        && intent.get("reservationOrdinal").and_then(Value::as_u64) == Some(executed_steps)
        && intent.get("attempt").and_then(Value::as_u64) == Some(1)
        && intent
            .get("dispatchDeadlineEpochMs")
            .and_then(Value::as_u64)
            == Some(prepared.dispatch_deadline_ms)
        && intent.get("providerDigest").and_then(Value::as_str)
            == Some(prepared.binding.provider_digest.as_str())
        && intent.get("contractDigest").and_then(Value::as_str)
            == Some(prepared.binding.contract_digest.as_str())
        && intent.get("projectionDigest").and_then(Value::as_str)
            == Some(prepared.binding.projection_digest.as_str())
        && intent.get("bindingDigest").and_then(Value::as_str)
            == Some(prepared.binding_digest.as_str());
    if !matches {
        return Err(durable_error(
            "DURABLE.BINDING_MISMATCH",
            "action intent no longer matches its provider, projection, or input",
        ));
    }
    Ok(())
}

/// Whether this workflow can be executed durably at all.
///
/// Conservative by design. Every rejection here is a case where the executor
/// could not honestly account for what happened after a crash, so refusing up
/// front is better than discovering it mid-run.
pub fn assert_durable_plan(descriptor: &WorkflowDescriptor) -> Result<(), AutomationError> {
    assert_durable_plan_with_mode(descriptor, DurableActionMode::Deny)
}

pub fn assert_durable_plan_with_mode(
    descriptor: &WorkflowDescriptor,
    action_mode: DurableActionMode,
) -> Result<(), AutomationError> {
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
        .with_detail("max_concurrency", json!(descriptor.budgets.max_concurrency)));
    }
    // An explicit top-level `depends_on` can reorder steps in ways a single
    // "next index" cannot express.
    if let Some(steps) = descriptor.raw.get("steps").and_then(Value::as_array) {
        if steps.iter().any(|step| step.get("depends_on").is_some()) {
            return Err(durable_error(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable execution requires implicit sequential top-level steps",
            ));
        }
    }

    let top_level: std::collections::HashSet<*const aad_core::model::CompiledStep> =
        descriptor.steps.iter().map(std::ptr::from_ref).collect();
    // Scripts and nested actions have effects this mode cannot reason about
    // after an interruption. Top-level actions are considered further only in
    // the explicitly requested read-only mode.
    let unsupported: Vec<String> = descriptor
        .all_steps()
        .iter()
        .filter(|step| {
            step.step_type == aad_core::model::StepType::Script
                || (step.step_type == aad_core::model::StepType::Action
                    && (action_mode != DurableActionMode::ReadOnly
                        || !top_level.contains(&std::ptr::from_ref(**step))))
        })
        .map(|step| step.id.clone())
        .collect();
    if !unsupported.is_empty() {
        return Err(durable_error(
            "DURABLE.UNSUPPORTED_PLAN",
            "durable execution rejects scripts and non-opted-in or nested actions",
        )
        .with_detail("unsupportedSteps", json!(unsupported)));
    }
    if action_mode == DurableActionMode::ReadOnly {
        for step in &descriptor.steps {
            if step.step_type != aad_core::model::StepType::Action {
                continue;
            }
            let retry = step
                .get("retry")
                .or_else(|| descriptor.defaults.get("retry"))
                .and_then(Value::as_object);
            let attempts = retry
                .and_then(|value| value.get("max_attempts"))
                .and_then(Value::as_u64)
                .unwrap_or(1);
            if step.get("if").is_some()
                || step.get("precondition").is_some()
                || step.get("postcondition").is_some()
                || step.on_error.is_some()
                || !step.finally_steps.is_empty()
                || attempts != 1
            {
                return Err(durable_error(
                    "DURABLE.UNSUPPORTED_PLAN",
                    "durable read-only actions require one unconditional top-level attempt without handlers",
                ).with_detail("stepId", json!(step.id)));
            }
        }
    }
    Ok(())
}

fn workflow_version(descriptor: &WorkflowDescriptor) -> Option<&str> {
    descriptor.metadata.get("version").and_then(Value::as_str)
}

/// Serialise the resumable state.
///
/// The schema and runtime versions plus the plan digest are recorded so a
/// checkpoint written by an incompatible build, or against a different
/// workflow, is detected on read instead of misinterpreted.
fn encode_checkpoint(digest: &str, phase: Phase, deadline: f64, state: &SegmentState) -> Value {
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
    if version != LEGACY_CHECKPOINT_VERSION as u64 && version != CHECKPOINT_VERSION as u64 {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_UNSUPPORTED",
            format!(
                "checkpoint version {version} cannot be read by this build \
                 (expected {LEGACY_CHECKPOINT_VERSION} or {CHECKPOINT_VERSION})"
            ),
        ));
    }
    let digest = raw
        .get("planDigest")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "the checkpoint plan digest is missing or invalid",
            )
        })?;
    if digest != expected_digest {
        return Err(durable_error(
            "DURABLE.PLAN_MISMATCH",
            "the checkpoint was written against a different plan",
        )
        .with_detail("expected", Value::String(digest.into()))
        .with_detail("actual", Value::String(expected_digest.into())));
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
    if phase == Phase::ActionIntent {
        validate_action_intent_shape(raw)?;
    } else if raw.get("actionIntent").is_some() {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "action intent is present outside the action_intent phase",
        ));
    }
    if raw.get("finalization").is_some() {
        if version != CHECKPOINT_VERSION as u64 || phase != Phase::Finalizing {
            return Err(durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "finalization payload requires the current checkpoint version and finalizing phase",
            ));
        }
        validate_finalization_shape(raw)?;
    } else if version == CHECKPOINT_VERSION as u64 && phase == Phase::Finalizing {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "current finalizing checkpoint is missing its finalization payload",
        ));
    }
    let base_fields: BTreeSet<&str> = [
        "checkpointVersion",
        "runtimeVersion",
        "planDigest",
        "phase",
        "deadline",
        "nextTopLevelIndex",
        "executedSteps",
        "unknownEffect",
        "variables",
        "steps",
        "returned",
    ]
    .into_iter()
    .collect();
    let allowed_fields: BTreeSet<&str> = base_fields
        .into_iter()
        .chain(raw.get("actionIntent").map(|_| "actionIntent"))
        .chain(raw.get("finalization").map(|_| "finalization"))
        .collect();
    let actual_fields = raw
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect::<BTreeSet<_>>())
        .ok_or_else(|| {
            durable_error("DURABLE.CHECKPOINT_INVALID", "checkpoint must be an object")
        })?;
    let missing_required = [
        "checkpointVersion",
        "planDigest",
        "phase",
        "deadline",
        "nextTopLevelIndex",
    ]
    .into_iter()
    .any(|field| !actual_fields.contains(field));
    if missing_required
        || actual_fields
            .iter()
            .any(|field| !allowed_fields.contains(field))
    {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "checkpoint fields do not match the supported schema",
        ));
    }
    if version == CHECKPOINT_VERSION as u64
        && (raw.get("runtimeVersion").and_then(Value::as_str) != Some(engine::RUNTIME_VERSION)
            || raw.get("executedSteps").and_then(Value::as_u64).is_none()
            || !raw.get("variables").is_some_and(Value::is_object)
            || !raw.get("steps").is_some_and(Value::is_object)
            || !raw.get("unknownEffect").is_some_and(Value::is_boolean)
            // A return value may be any JSON value, including null. Presence is
            // therefore the only meaningful shape check, but it must not be
            // confused with an omitted field from a truncated checkpoint.
            || raw.get("returned").is_none())
    {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "current checkpoint runtime or state fields are invalid",
        ));
    }
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
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                durable_error(
                    "DURABLE.CHECKPOINT_INVALID",
                    "checkpoint next top-level index is invalid",
                )
            })?,
        executed_steps: raw
            .get("executedSteps")
            .and_then(Value::as_u64)
            // Legacy v1 checkpoints predate the complete-state requirement.
            // Current v2 checkpoints were rejected above if this is absent or
            // malformed.
            .unwrap_or(0),
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

fn validate_action_intent_shape(raw: &Value) -> Result<(), AutomationError> {
    let intent = raw
        .get("actionIntent")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            durable_error(
                "DURABLE.CHECKPOINT_INVALID",
                "action intent payload is missing",
            )
        })?;
    let fields: BTreeSet<&str> = intent.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = [
        "version",
        "operationId",
        "stepId",
        "reservationOrdinal",
        "attempt",
        "dispatchDeadlineEpochMs",
        "providerDigest",
        "contractDigest",
        "projectionDigest",
        "bindingDigest",
    ]
    .into_iter()
    .collect();
    let valid = fields == expected
        && intent.get("version").and_then(Value::as_u64) == Some(2)
        && intent.get("attempt").and_then(Value::as_u64) == Some(1)
        && intent
            .get("reservationOrdinal")
            .and_then(Value::as_u64)
            .is_some()
        && intent
            .get("dispatchDeadlineEpochMs")
            .and_then(Value::as_u64)
            .is_some()
        && intent
            .get("operationId")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && intent
            .get("stepId")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
    if !valid {
        return Err(durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "action intent fields are invalid",
        ));
    }
    Ok(())
}

fn validate_finalization_shape(raw: &Value) -> Result<(), AutomationError> {
    let stage = finalization_stage(raw)?.ok_or_else(|| {
        durable_error(
            "DURABLE.CHECKPOINT_INVALID",
            "finalization payload is missing",
        )
    })?;
    match stage {
        FinalizationStage::Intent => {
            decode_finalization_intent(raw)?;
        }
        FinalizationStage::Started => {
            let mut intent = raw.clone();
            intent["finalization"]["stage"] = json!("intent");
            decode_finalization_intent(&intent)?;
        }
        FinalizationStage::Result => {
            decode_finalized_run(
                raw,
                raw.get("executedSteps")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            )?;
        }
    }
    Ok(())
}

fn as_map(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn workflow_timeout_error(deadline: f64) -> AutomationError {
    AutomationError::new("WORKFLOW.TIMEOUT", "the run exceeded its maximum duration")
        .with_category("workflow")
        .with_effect("not_applied")
        .with_detail("deadline", json!(deadline))
}

fn cancelled_error(reason: &str) -> AutomationError {
    AutomationError::new("WORKFLOW.CANCELLED", reason)
        .with_category("workflow")
        .with_effect("not_applied")
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
