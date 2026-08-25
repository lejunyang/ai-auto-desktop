"""Crash-safe orchestration for strictly serial durable workflows."""

from __future__ import annotations

from dataclasses import dataclass
import re
import time
from typing import Any, Callable, Mapping, Sequence
import uuid

from .errors import AutomationError, ensure_automation_error
from .journal import (
    DesiredState,
    JournalConflictError,
    JournalError,
    JournalStore,
    OwnerLease,
    RunRecord,
    RunStatus,
    durable_descriptor_eligible,
)
from .model import CompiledStep, RunResult, WorkflowDescriptor, thaw
from .plugin import ProcessPlugin
from .run_service import DispatchState, RunService, RunServiceError
from .runtime import (
    RUNTIME_STATE_SCHEMA_VERSION,
    RuntimeSegmentState,
    WorkflowRunner,
    canonical_plan_digest,
)


DURABLE_CHECKPOINT_SCHEMA_VERSION = 2
DEFAULT_LEASE_TTL_SECONDS = 30.0


@dataclass(frozen=True, slots=True)
class DurableExecutionResult:
    """One durable execution invocation and its current persisted run."""

    run: RunRecord
    result: RunResult | None = None

    def to_dict(self) -> dict[str, Any]:
        payload = self.run.to_dict()
        if self.result is not None:
            payload["executionResult"] = self.result.to_dict()
        return payload


@dataclass(frozen=True, slots=True)
class _PreparedAction:
    step: CompiledStep
    binding: Any
    binding_digest: str
    dispatch_deadline_epoch_ms: int


class DurableExecutor:
    """Drive :class:`WorkflowRunner` through journaled safe boundaries.

    Heartbeats happen synchronously at every durable boundary.  This class does
    not claim to interrupt an already dispatched OS/plugin call asynchronously.
    """

    def __init__(
        self,
        journal: JournalStore,
        *,
        owner_id: str | None = None,
        lease_ttl_seconds: float = DEFAULT_LEASE_TTL_SECONDS,
        runner_factory: Callable[..., WorkflowRunner] = WorkflowRunner,
        durable_action_mode: str = "deny",
    ) -> None:
        self.journal = journal
        self.service = RunService(journal)
        self.owner_id = owner_id or f"runner-{uuid.uuid4().hex}"
        if isinstance(lease_ttl_seconds, bool) or lease_ttl_seconds <= 0:
            raise ValueError("lease_ttl_seconds must be positive")
        self.lease_ttl_seconds = float(lease_ttl_seconds)
        self.runner_factory = runner_factory
        normalized_mode = str(durable_action_mode).replace("-", "_")
        if normalized_mode not in {"deny", "read_only"}:
            raise ValueError("durable_action_mode must be deny or read_only")
        self.durable_action_mode = normalized_mode

    def _new_runner(
        self,
        descriptor: WorkflowDescriptor,
        *,
        plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None,
        allow_scripts: bool,
        granted_permissions: Sequence[str] | None,
    ) -> WorkflowRunner:
        arguments = {
            "plugins": plugins,
            "allow_scripts": allow_scripts,
            "granted_permissions": granted_permissions,
        }
        if self.durable_action_mode == "read_only":
            arguments["durable_action_mode"] = "read_only"
        return self.runner_factory(descriptor, **arguments)

    def start(
        self,
        descriptor: WorkflowDescriptor,
        *,
        inputs: Mapping[str, Any] | None = None,
        run_id: str | None = None,
        plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None = None,
        allow_scripts: bool = False,
        granted_permissions: Sequence[str] | None = None,
    ) -> DurableExecutionResult:
        """Create and execute a new durable run."""

        actual_inputs = dict(inputs or {})
        self._assert_durable_plan(descriptor)
        runner = self._new_runner(
            descriptor, plugins=plugins, allow_scripts=allow_scripts,
            granted_permissions=granted_permissions,
        )
        try:
            state = runner.initialize(actual_inputs)
            if self.durable_action_mode == "read_only":
                self._preflight_actions(runner)
            run = self.service.create(
                run_id=run_id,
                workflow_name=descriptor.name,
                workflow_version=self._workflow_version(descriptor),
                plan_digest=canonical_plan_digest(descriptor),
                inputs=actual_inputs,
                descriptor=descriptor,
            )
            lease = self._claim(run.run_id)
            started, _ = self.journal.set_status_with_event(
                run.run_id,
                expected=RunStatus.PENDING,
                status=RunStatus.RUNNING,
                owner_id=lease.owner_id,
                token=lease.token,
                event_type="run.started",
                event_payload={"ownerId": lease.owner_id},
                checkpoint=self._checkpoint(state),
                sensitive=False,
            )
            return self._execute_loop(runner, started, lease)
        except Exception as exc:
            runner.close()
            if isinstance(exc, AutomationError):
                raise
            raise self._map_error(exc, run_id=run_id or "uncreated") from exc

    def execute(
        self,
        run_id: str,
        descriptor: WorkflowDescriptor,
        *,
        plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None = None,
        allow_scripts: bool = False,
        granted_permissions: Sequence[str] | None = None,
    ) -> DurableExecutionResult:
        """Execute a pending run created separately by :class:`RunService`."""

        run = self.service.get(run_id)
        if run.status is not RunStatus.PENDING:
            raise self._error(
                "DURABLE.INVALID_STATE",
                f"run {run_id} is not pending",
                run=run,
            )
        self._validate_plan(run, descriptor)
        runner = self._new_runner(
            descriptor, plugins=plugins, allow_scripts=allow_scripts,
            granted_permissions=granted_permissions,
        )
        try:
            state = runner.initialize(run.inputs)
            if self.durable_action_mode == "read_only":
                self._preflight_actions(runner)
            lease = self._claim(run_id)
            started, _ = self.journal.set_status_with_event(
                run_id,
                expected=RunStatus.PENDING,
                status=RunStatus.RUNNING,
                owner_id=lease.owner_id,
                token=lease.token,
                event_type="run.started",
                event_payload={"ownerId": lease.owner_id},
                checkpoint=self._checkpoint(state),
                sensitive=False,
            )
            return self._execute_loop(runner, started, lease)
        except Exception as exc:
            runner.close()
            if isinstance(exc, AutomationError):
                raise
            raise self._map_error(exc, run_id=run_id) from exc

    def resume(
        self,
        run_id: str,
        descriptor: WorkflowDescriptor,
        *,
        plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None = None,
        allow_scripts: bool = False,
        granted_permissions: Sequence[str] | None = None,
        request_run: bool = False,
    ) -> DurableExecutionResult:
        """Resume only a proven safe boundary; unsafe recovery is terminal."""

        self._assert_durable_plan(descriptor)
        run = self.service.get(run_id)
        self._validate_plan(run, descriptor)
        if run.terminal:
            raise self._error(
                "DURABLE.TERMINAL",
                f"terminal run {run_id} cannot be resumed",
                run=run,
            )
        checkpoint = self._require_checkpoint(run)
        phase = checkpoint.get("phase")
        action_intent = self._is_action_intent(checkpoint)
        if phase != "between_top_level_steps" and not action_intent:
            lease = self._claim(run_id)
            terminal = self._reject_unsafe_recovery(run, lease, phase=phase)
            return DurableExecutionResult(terminal)
        if request_run and run.desired_state is DesiredState.PAUSE:
            run = self.service.request_resume(run_id)
        if run.desired_state is DesiredState.PAUSE:
            if run.status is RunStatus.PAUSED:
                return DurableExecutionResult(run)
            lease = self._claim(run_id)
            paused = self.service.runner_safe_point(
                run_id, owner_id=lease.owner_id, token=lease.token
            )
            return DurableExecutionResult(paused)
        lease = self._claim(run_id)
        current = self.service.get(run_id)
        if current.desired_state is DesiredState.CANCEL:
            if action_intent:
                return DurableExecutionResult(
                    self.service.runner_safe_point(
                        run_id, owner_id=lease.owner_id, token=lease.token,
                        dispatch_state=DispatchState.BEFORE_DISPATCH,
                    )
                )
            return self._resume_cancellation(
                current, descriptor, checkpoint, lease, plugins,
                allow_scripts, granted_permissions,
            )
        runner = self._new_runner(
            descriptor, plugins=plugins, allow_scripts=allow_scripts,
            granted_permissions=granted_permissions,
        )
        try:
            if action_intent:
                if current.status is RunStatus.PAUSED:
                    current, _ = self.journal.set_status_with_event(
                        run_id, expected=RunStatus.PAUSED,
                        status=RunStatus.RUNNING, owner_id=lease.owner_id,
                        token=lease.token, event_type="run.resumed",
                        event_payload={"ownerId": lease.owner_id},
                        checkpoint=checkpoint, sensitive=False,
                    )
                elif current.status is not RunStatus.RUNNING:
                    raise self._error(
                        "DURABLE.INVALID_STATE",
                        f"run {run_id} cannot resume from {current.status.value}",
                        run=current,
                    )
                return self._resume_action_intent(
                    runner, current, lease, checkpoint
                )
            try:
                runner.import_state(self._runtime_state(checkpoint), inputs=run.inputs)
            except AutomationError as exc:
                if exc.code != "WORKFLOW.TIMEOUT":
                    raise
                runner.import_state(
                    self._runtime_state(checkpoint),
                    inputs=run.inputs,
                    allow_expired=True,
                )
                finalizing = runner.prepare_finalize()
                self.journal.save_checkpoint(
                    run_id, self._checkpoint(finalizing),
                    owner_id=lease.owner_id, token=lease.token, sensitive=False,
                )
                result = runner.finalize(error=exc)
                return self._commit_result(run_id, lease, result)
            if current.status is RunStatus.PAUSED:
                current, _ = self.journal.set_status_with_event(
                    run_id,
                    expected=RunStatus.PAUSED,
                    status=RunStatus.RUNNING,
                    owner_id=lease.owner_id,
                    token=lease.token,
                    event_type="run.resumed",
                    event_payload={"ownerId": lease.owner_id},
                    checkpoint=checkpoint,
                    sensitive=False,
                )
            elif current.status is not RunStatus.RUNNING:
                raise self._error(
                    "DURABLE.INVALID_STATE",
                    f"run {run_id} cannot resume from {current.status.value}",
                    run=current,
                )
            return self._execute_loop(runner, current, lease)
        except BaseException as exc:
            runner.close()
            if isinstance(exc, AutomationError):
                raise
            raise self._map_error(exc, run_id=run_id) from exc

    def _execute_loop(
        self, runner: WorkflowRunner, run: RunRecord, lease: OwnerLease
    ) -> DurableExecutionResult:
        while True:
            lease = self._heartbeat(lease)
            intent = self.service.get(run.run_id)
            if intent.desired_state is DesiredState.PAUSE:
                paused = self.service.runner_safe_point(
                    run.run_id, owner_id=lease.owner_id, token=lease.token
                )
                runner.close()
                return DurableExecutionResult(paused)
            if intent.desired_state is DesiredState.CANCEL:
                runner.request_segment_cancellation()
                prepared = runner.export_state()
                self.journal.save_checkpoint(
                    run.run_id, self._checkpoint(prepared),
                    owner_id=lease.owner_id, token=lease.token, sensitive=False,
                )
                result = runner.finalize()
                return self._commit_result(run.run_id, lease, result)

            prepared = runner.prepare_segment()
            if prepared.step_id is None:
                finalizing = runner.prepare_finalize()
                self.journal.save_checkpoint(
                    run.run_id, self._checkpoint(finalizing),
                    owner_id=lease.owner_id, token=lease.token, sensitive=False,
                )
                result = runner.finalize()
                return self._commit_result(run.run_id, lease, result)
            step = runner.descriptor.steps[prepared.state.next_top_level_index]
            if step.type == "action":
                return self._dispatch_read_only_action(
                    runner, run, lease, step
                )
            try:
                self.journal.append_event_with_checkpoint(
                    run.run_id,
                    "run.segment_entered",
                    {"stepId": prepared.step_id},
                    self._checkpoint(prepared.state),
                    owner_id=lease.owner_id,
                    token=lease.token,
                    expected_status=RunStatus.RUNNING,
                    expected_desired_state=DesiredState.RUN,
                    sensitive=False,
                )
            except JournalConflictError:
                runner.abort_prepared_segment()
                intent = self.service.get(run.run_id)
                if intent.desired_state is DesiredState.PAUSE:
                    paused = self.service.runner_safe_point(
                        run.run_id, owner_id=lease.owner_id, token=lease.token
                    )
                    runner.close()
                    return DurableExecutionResult(paused)
                if intent.desired_state is DesiredState.CANCEL:
                    runner.request_segment_cancellation()
                    prepared_cancel = runner.export_state()
                    self.journal.save_checkpoint(
                        run.run_id, self._checkpoint(prepared_cancel),
                        owner_id=lease.owner_id, token=lease.token, sensitive=False,
                    )
                    result = runner.finalize()
                    return self._commit_result(run.run_id, lease, result)
                raise
            segment = runner.run_segment()
            lease = self._heartbeat(lease)
            if segment.state.phase == "between_top_level_steps":
                self.journal.append_event_with_checkpoint(
                    run.run_id,
                    "run.segment_completed",
                    {
                        "stepId": segment.step_id,
                        "nextTopLevelIndex": segment.state.next_top_level_index,
                    },
                    self._checkpoint(segment.state),
                    owner_id=lease.owner_id,
                    token=lease.token,
                    sensitive=False,
                )
                continue

            self.journal.save_checkpoint(
                run.run_id, self._checkpoint(segment.state),
                owner_id=lease.owner_id, token=lease.token, sensitive=False,
            )
            result = runner.finalize()
            return self._commit_result(run.run_id, lease, result)

    def _dispatch_read_only_action(
        self, runner: WorkflowRunner, run: RunRecord, lease: OwnerLease,
        step: CompiledStep,
    ) -> DurableExecutionResult:
        try:
            prepared = self._prepare_action(runner, step)
            state = runner.reserve_prepared_action_attempt()
        except Exception as exc:
            # No provider dispatch has happened.  Convert the prepared segment
            # back to a safe boundary and persist a redacted terminal failure
            # instead of abandoning a running row with a live lease.
            runner.abort_prepared_segment()
            finalizing = runner.prepare_finalize()
            self.journal.save_checkpoint(
                run.run_id, self._checkpoint(finalizing),
                owner_id=lease.owner_id, token=lease.token, sensitive=False,
            )
            original = ensure_automation_error(exc)
            safe_error = AutomationError(
                "DURABLE.ACTION_PREPARATION_FAILED",
                "Durable action preparation failed before dispatch",
                category="durable",
                phase="prepare",
                retryable=False,
                effect="not_applied",
                details={"stepId": step.id, "originalCode": original.code},
            )
            result = runner.finalize(error=safe_error)
            return self._commit_result(run.run_id, lease, result)
        checkpoint = self._action_intent_checkpoint(state, prepared)
        try:
            self.journal.append_event_with_checkpoint(
                run.run_id, "run.action_intent",
                {"stepId": step.id, "operationId": checkpoint["actionIntent"]["operationId"]},
                checkpoint, owner_id=lease.owner_id, token=lease.token,
                expected_status=RunStatus.RUNNING,
                expected_desired_state=DesiredState.RUN, sensitive=False,
            )
        except JournalConflictError:
            runner.release_prepared_action_attempt()
            runner.abort_prepared_segment()
            return self._apply_control_before_dispatch(runner, run.run_id, lease)
        return self._authorize_and_run_action(
            runner, self.service.get(run.run_id), lease, prepared, checkpoint
        )

    def _resume_action_intent(
        self, runner: WorkflowRunner, run: RunRecord, lease: OwnerLease,
        checkpoint: Mapping[str, Any],
    ) -> DurableExecutionResult:
        runner.restore_action_intent(
            self._runtime_state(checkpoint), inputs=run.inputs,
            allow_expired=True,
        )
        index = checkpoint["nextTopLevelIndex"]
        if isinstance(index, bool) or not isinstance(index, int) or not 0 <= index < len(runner.descriptor.steps):
            raise self._error(
                "DURABLE.CHECKPOINT_INVALID",
                "action intent step index is invalid", run=run,
            )
        step = runner.descriptor.steps[index]
        intent_deadline = checkpoint["actionIntent"].get(
            "dispatchDeadlineEpochMs"
        )
        if (
            isinstance(intent_deadline, bool)
            or not isinstance(intent_deadline, int)
            or intent_deadline < 0
        ):
            raise self._error(
                "DURABLE.CHECKPOINT_INVALID",
                "action intent dispatch deadline is invalid", run=run,
            )
        binding = runner.durable_action_binding(step)
        prepared = _PreparedAction(
            step, binding, runner.durable_action_binding_digest(step, binding),
            intent_deadline,
        )
        prepared = self._validate_action_intent(checkpoint, prepared, run)
        return self._authorize_and_run_action(
            runner, run, lease, prepared, checkpoint
        )

    def _authorize_and_run_action(
        self, runner: WorkflowRunner, run: RunRecord, lease: OwnerLease,
        prepared: _PreparedAction, checkpoint: Mapping[str, Any],
    ) -> DurableExecutionResult:
        intent = checkpoint["actionIntent"]
        try:
            self.journal.append_event_with_checkpoint(
                run.run_id, "run.action_dispatch_authorized",
                {"stepId": prepared.step.id, "operationId": intent["operationId"]},
                checkpoint, owner_id=lease.owner_id, token=lease.token,
                expected_status=RunStatus.RUNNING,
                expected_desired_state=DesiredState.RUN, sensitive=False,
            )
        except JournalConflictError:
            return self._apply_control_before_dispatch(runner, run.run_id, lease)
        if prepared.dispatch_deadline_epoch_ms <= int(time.time() * 1_000):
            segment = runner.run_durable_action_segment(
                prepared.binding, prepared.dispatch_deadline_epoch_ms
            )
            self.journal.save_checkpoint(
                run.run_id, self._checkpoint(segment.state),
                owner_id=lease.owner_id, token=lease.token, sensitive=False,
            )
            result = runner.finalize()
            return self._commit_result(run.run_id, lease, result)
        segment = runner.run_durable_action_segment(
            prepared.binding, prepared.dispatch_deadline_epoch_ms
        )
        lease = self._heartbeat(lease)
        if segment.state.phase == "between_top_level_steps":
            boundary = self._checkpoint(segment.state)
            try:
                self.journal.append_event_with_checkpoint(
                    run.run_id, "run.segment_completed",
                    {"stepId": segment.step_id, "nextTopLevelIndex": segment.state.next_top_level_index,
                     "operationId": intent["operationId"]},
                    boundary, owner_id=lease.owner_id, token=lease.token,
                    expected_status=RunStatus.RUNNING,
                    expected_desired_state=DesiredState.RUN, sensitive=False,
                )
            except JournalConflictError:
                # The read-only result is already projected and safe to persist.
                # Save it before applying a concurrently requested pause/cancel
                # so resume never replays an already completed observation.
                self.journal.save_checkpoint(
                    run.run_id, boundary, owner_id=lease.owner_id,
                    token=lease.token, sensitive=False,
                )
                return self._apply_control_after_read_only_dispatch(
                    runner, run.run_id, lease
                )
            return self._execute_loop(runner, self.service.get(run.run_id), lease)
        self.journal.save_checkpoint(
            run.run_id, self._checkpoint(segment.state),
            owner_id=lease.owner_id, token=lease.token, sensitive=False,
        )
        result = runner.finalize()
        return self._commit_result(run.run_id, lease, result)

    def _apply_control_before_dispatch(
        self, runner: WorkflowRunner, run_id: str, lease: OwnerLease
    ) -> DurableExecutionResult:
        current = self.service.get(run_id)
        if current.desired_state not in {DesiredState.PAUSE, DesiredState.CANCEL}:
            raise self._error(
                "DURABLE.STATE_CONFLICT",
                "action dispatch authorization lost its expected state",
                run=current,
            )
        controlled = self.service.runner_safe_point(
            run_id, owner_id=lease.owner_id, token=lease.token,
            dispatch_state=DispatchState.BEFORE_DISPATCH,
        )
        runner.close()
        return DurableExecutionResult(controlled)

    def _apply_control_after_read_only_dispatch(
        self, runner: WorkflowRunner, run_id: str, lease: OwnerLease
    ) -> DurableExecutionResult:
        current = self.service.get(run_id)
        if current.desired_state not in {DesiredState.PAUSE, DesiredState.CANCEL}:
            raise self._error(
                "DURABLE.STATE_CONFLICT",
                "read-only action completion lost its expected state",
                run=current,
            )
        controlled = self.service.runner_safe_point(
            run_id, owner_id=lease.owner_id, token=lease.token,
            dispatch_state=DispatchState.EFFECT_CONFIRMED,
        )
        runner.close()
        return DurableExecutionResult(controlled)

    def _prepare_action(
        self, runner: WorkflowRunner, step: CompiledStep
    ) -> _PreparedAction:
        binding = runner.durable_action_binding(step)
        digest = runner.durable_action_binding_digest(step, binding)
        deadline = runner.durable_action_deadlines(
            step, binding
        )["dispatchDeadlineEpochMs"]
        if not isinstance(deadline, int):
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action requires a finite dispatch deadline",
                category="durable", details={"stepId": step.id},
            )
        return _PreparedAction(step, binding, digest, deadline)

    @staticmethod
    def _action_intent_checkpoint(
        state: RuntimeSegmentState, prepared: _PreparedAction
    ) -> dict[str, Any]:
        return {
            "checkpointSchemaVersion": 2, **{
                **state.to_dict(), "phase": "action_intent"
            },
            "actionIntent": {
                "version": 2, "operationId": uuid.uuid4().hex,
                "stepId": prepared.step.id,
                "reservationOrdinal": state.executed_attempts,
                "attempt": 1,
                "dispatchDeadlineEpochMs": prepared.dispatch_deadline_epoch_ms,
                "providerDigest": prepared.binding.provider_digest,
                "contractDigest": prepared.binding.contract_digest,
                "projectionDigest": prepared.binding.projection_digest,
                "bindingDigest": prepared.binding_digest,
            },
        }

    def _validate_action_intent(
        self, checkpoint: Mapping[str, Any], prepared: _PreparedAction,
        run: RunRecord,
    ) -> _PreparedAction:
        intent = checkpoint["actionIntent"]
        expected = {
            "version": 2, "stepId": prepared.step.id,
            "reservationOrdinal": checkpoint["executedAttempts"],
            "attempt": 1,
            "providerDigest": prepared.binding.provider_digest,
            "contractDigest": prepared.binding.contract_digest,
            "projectionDigest": prepared.binding.projection_digest,
            "bindingDigest": prepared.binding_digest,
        }
        if any(intent.get(key) != value for key, value in expected.items()):
            raise self._error(
                "DURABLE.BINDING_MISMATCH",
                "action intent no longer matches its binding", run=run,
            )
        deadline = intent.get("dispatchDeadlineEpochMs")
        if isinstance(deadline, bool) or not isinstance(deadline, int) or deadline < 0:
            raise self._error(
                "DURABLE.CHECKPOINT_INVALID",
                "action intent dispatch deadline is invalid", run=run,
            )
        return _PreparedAction(
            prepared.step, prepared.binding, prepared.binding_digest, deadline
        )

    def _resume_cancellation(
        self,
        run: RunRecord,
        descriptor: WorkflowDescriptor,
        checkpoint: Mapping[str, Any],
        lease: OwnerLease,
        plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None,
        allow_scripts: bool,
        granted_permissions: Sequence[str] | None,
    ) -> DurableExecutionResult:
        runner = self._new_runner(
            descriptor, plugins=plugins, allow_scripts=allow_scripts,
            granted_permissions=granted_permissions,
        )
        try:
            runner.import_state(
                self._runtime_state(checkpoint), inputs=run.inputs, allow_expired=True
            )
            runner.request_segment_cancellation()
            prepared = runner.export_state()
            self.journal.save_checkpoint(
                run.run_id, self._checkpoint(prepared),
                owner_id=lease.owner_id, token=lease.token, sensitive=False,
            )
            result = runner.finalize()
            return self._commit_result(run.run_id, lease, result)
        finally:
            runner.close()

    def _commit_result(
        self, run_id: str, lease: OwnerLease, result: RunResult
    ) -> DurableExecutionResult:
        status = RunStatus(result.status)
        output = result.output if status is RunStatus.SUCCEEDED else None
        error = None if status is RunStatus.SUCCEEDED else (
            result.error.to_dict()
            if result.error is not None
            else AutomationError(
                "DURABLE.TERMINAL",
                f"workflow ended as {status.value}",
                category="durable",
            ).to_dict()
        )
        # Finalization has already produced a definitive terminal result.  A
        # racing pause must not create an unrecoverable ``paused/finalizing``
        # row; commit the result against PAUSE instead.  A racing cancel may
        # supersede only a successful, confirmed read-only result.  Failures,
        # timeouts and unknown effects remain truthful terminal outcomes.
        for _ in range(16):
            current = self.service.get(run_id)
            if (
                current.desired_state is DesiredState.CANCEL
                and status is RunStatus.SUCCEEDED
            ):
                cancelled = self.service.runner_safe_point(
                    run_id, owner_id=lease.owner_id, token=lease.token,
                    dispatch_state=DispatchState.EFFECT_CONFIRMED,
                )
                return DurableExecutionResult(cancelled)
            try:
                terminal, _ = self.journal.set_status_with_event(
                    run_id, expected=current.status, status=status,
                    owner_id=lease.owner_id, token=lease.token, output=output,
                    error=error, event_type="run.finished",
                    event_payload={"status": status.value},
                    expected_desired_state=current.desired_state,
                    sensitive=False,
                )
                return DurableExecutionResult(terminal, result)
            except JournalConflictError:
                # Operator intent changed after the read.  Re-read and either
                # commit against that exact intent or apply sticky cancel.
                continue
        latest = self.service.get(run_id)
        raise self._error(
            "DURABLE.STATE_CONFLICT",
            "control state did not stabilize during terminal commit",
            run=latest,
        )

    def _reject_unsafe_recovery(
        self, run: RunRecord, lease: OwnerLease, *, phase: Any
    ) -> RunRecord:
        error = AutomationError(
            "DURABLE.UNSAFE_RECOVERY",
            "checkpoint is not at a replay-safe top-level boundary",
            category="durable",
            phase="restore",
            effect="unknown",
            details={"runId": run.run_id, "checkpointPhase": phase},
        ).to_dict()
        terminal, _ = self.journal.set_status_with_event(
            run.run_id,
            expected=run.status,
            status=RunStatus.UNKNOWN_EFFECT,
            owner_id=lease.owner_id,
            token=lease.token,
            error=error,
            event_type="run.recovery_rejected",
            event_payload={"phase": phase},
            sensitive=False,
        )
        return terminal

    def _claim(self, run_id: str) -> OwnerLease:
        try:
            return self.journal.claim_owner(
                run_id, owner_id=self.owner_id,
                ttl_seconds=self.lease_ttl_seconds,
            )
        except Exception as exc:
            raise self._map_error(exc, run_id=run_id) from exc

    def _heartbeat(self, lease: OwnerLease) -> OwnerLease:
        try:
            return self.journal.heartbeat_owner(
                lease.run_id, owner_id=lease.owner_id, token=lease.token,
                ttl_seconds=self.lease_ttl_seconds,
            )
        except Exception as exc:
            raise self._map_error(exc, run_id=lease.run_id) from exc

    @staticmethod
    def _checkpoint(state: RuntimeSegmentState) -> dict[str, Any]:
        return {
            "checkpointSchemaVersion": DURABLE_CHECKPOINT_SCHEMA_VERSION,
            **state.to_dict(),
        }

    def _require_checkpoint(self, run: RunRecord) -> Mapping[str, Any]:
        value = run.checkpoint
        if not isinstance(value, Mapping):
            raise self._error(
                "DURABLE.CHECKPOINT_INVALID",
                "run has no durable checkpoint", run=run,
            )
        required = {
            "checkpointSchemaVersion",
            "schemaVersion",
            "runtimeVersion",
            "descriptorDigest",
            "phase",
            "nextTopLevelIndex",
            "executedAttempts",
            "deadlineEpochMs",
            "variables",
            "contextSteps",
            "stepRecords",
        }
        allowed = required | {"actionIntent"}
        keys = frozenset(value)
        if keys not in {frozenset(required), frozenset(allowed)}:
            raise self._error(
                "DURABLE.CHECKPOINT_INVALID",
                "checkpoint fields do not match the supported schema", run=run,
            )
        version = value.get("checkpointSchemaVersion")
        if version not in {1, DURABLE_CHECKPOINT_SCHEMA_VERSION}:
            raise self._error(
                "DURABLE.CHECKPOINT_VERSION",
                "checkpoint schema version is unsupported", run=run,
            )
        if "actionIntent" in value:
            if version != 2 or value.get("phase") != "action_intent":
                raise self._error(
                    "DURABLE.CHECKPOINT_INVALID",
                    "action intent checkpoint phase or version is invalid", run=run,
                )
            intent = value["actionIntent"]
            expected = {
                "version", "operationId", "stepId",
                "reservationOrdinal", "attempt",
                "dispatchDeadlineEpochMs", "providerDigest",
                "contractDigest", "projectionDigest", "bindingDigest",
            }
            if not isinstance(intent, Mapping) or set(intent) != expected:
                raise self._error(
                    "DURABLE.CHECKPOINT_INVALID",
                    "action intent fields are invalid", run=run,
                )
            integer_fields = ("reservationOrdinal", "attempt", "dispatchDeadlineEpochMs")
            if (
                intent.get("version") != 2
                or intent.get("attempt") != 1
                or any(
                    isinstance(intent.get(name), bool)
                    or not isinstance(intent.get(name), int)
                    or intent.get(name) < 0
                    for name in integer_fields
                )
                or not isinstance(intent.get("operationId"), str)
                or not intent["operationId"]
                or any(
                    not isinstance(intent.get(name), str)
                    or re.fullmatch(r"sha256:[0-9a-f]{64}", intent[name]) is None
                    for name in ("providerDigest", "contractDigest", "projectionDigest", "bindingDigest")
                )
            ):
                raise self._error(
                    "DURABLE.CHECKPOINT_INVALID",
                    "action intent field types are invalid", run=run,
                )
        elif version == 2 and value.get("phase") == "action_intent":
            raise self._error(
                "DURABLE.CHECKPOINT_INVALID",
                "action intent payload is missing", run=run,
            )
        return value

    @staticmethod
    def _runtime_state(checkpoint: Mapping[str, Any]) -> dict[str, Any]:
        return {
            key: value
            for key, value in checkpoint.items()
            if key not in {"checkpointSchemaVersion", "actionIntent"}
        }

    @staticmethod
    def _is_action_intent(checkpoint: Mapping[str, Any]) -> bool:
        return (
            checkpoint.get("checkpointSchemaVersion") == 2
            and checkpoint.get("phase") == "action_intent"
            and isinstance(checkpoint.get("actionIntent"), Mapping)
        )

    def _assert_durable_plan(self, descriptor: WorkflowDescriptor) -> None:
        if not durable_descriptor_eligible(descriptor):
            raise AutomationError(
                "DURABLE.SENSITIVE_DESCRIPTOR",
                "durable execution rejects sensitive workflow inputs and outputs",
                category="durable",
            )
        if int(descriptor.budgets.get("max_concurrency", 1)) != 1:
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable execution requires max_concurrency=1",
                category="durable",
            )
        raw_steps = descriptor.raw.get("steps", ())
        if not isinstance(raw_steps, (list, tuple)) or any(
            not isinstance(step, Mapping) or "depends_on" in step
            for step in raw_steps
        ):
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable execution requires implicit legacy top-level dependencies",
                category="durable",
            )
        top_level = {id(step) for step in descriptor.steps}
        unsupported = []
        for step in descriptor.all_steps():
            if step.type == "script":
                unsupported.append(step.id)
            elif step.type == "action" and (
                self.durable_action_mode != "read_only"
                or id(step) not in top_level
            ):
                unsupported.append(step.id)
        if unsupported:
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable execution rejects scripts and non-opted-in or nested actions",
                category="durable",
                details={"unsupportedSteps": unsupported},
            )
        previous: str | None = None
        for step in descriptor.steps:
            expected = () if previous is None else (previous,)
            if step.depends_on != expected:
                raise AutomationError(
                    "DURABLE.UNSUPPORTED_PLAN",
                    "durable execution requires the implicit top-level serial chain",
                    category="durable",
                    details={
                        "stepId": step.id,
                        "expectedDependsOn": list(expected),
                        "actualDependsOn": list(step.depends_on),
                    },
                )
            previous = step.id
        if self.durable_action_mode == "read_only":
            for step in descriptor.steps:
                if step.type != "action":
                    continue
                retry = thaw(step.params.get(
                    "retry", descriptor.defaults.get("retry", {})
                ))
                if (
                    "if" in step.params or "precondition" in step.params
                    or "postcondition" in step.params
                    or step.on_error is not None or bool(step.finally_steps)
                    or int(retry.get("max_attempts", 1)) != 1
                ):
                    raise AutomationError(
                        "DURABLE.UNSUPPORTED_PLAN",
                        "durable read-only actions require one unconditional top-level attempt without handlers",
                        category="durable", details={"stepId": step.id},
                    )

    def _preflight_actions(self, runner: WorkflowRunner) -> None:
        for step in runner.descriptor.steps:
            if step.type == "action":
                try:
                    runner.preflight_durable_action(step)
                except AutomationError as exc:
                    raise AutomationError(
                        "DURABLE.ACTION_PREFLIGHT_FAILED",
                        "durable action provider preflight failed",
                        category="durable", retryable=False,
                        effect="not_applied",
                        details={"stepId": step.id},
                    ) from None

    def _validate_plan(
        self, run: RunRecord, descriptor: WorkflowDescriptor
    ) -> None:
        self._assert_durable_plan(descriptor)
        digest = canonical_plan_digest(descriptor)
        if run.plan_digest != digest:
            raise self._error(
                "DURABLE.PLAN_MISMATCH",
                "descriptor does not match the durable run",
                run=run,
                details={"expectedPlanDigest": run.plan_digest, "planDigest": digest},
            )

    @staticmethod
    def _workflow_version(descriptor: WorkflowDescriptor) -> str | None:
        value = descriptor.metadata.get("version")
        return value if isinstance(value, str) else None

    @staticmethod
    def _error(
        code: str, message: str, *, run: RunRecord,
        details: Mapping[str, Any] | None = None,
    ) -> AutomationError:
        merged = {"runId": run.run_id, "status": run.status.value}
        merged.update(details or {})
        return AutomationError(
            code, message, category="durable", phase="execute", details=merged
        )

    @staticmethod
    def _map_error(exc: BaseException, *, run_id: str) -> AutomationError:
        if isinstance(exc, AutomationError):
            return exc
        if isinstance(exc, RunServiceError):
            return exc
        if isinstance(exc, JournalError):
            return RunService._error(
                "RUN.JOURNAL_FAILURE",
                str(exc) or type(exc).__name__,
                operation="durable_execute",
                run_id=run_id,
                retryable=True,
                cause=exc,
            )
        return AutomationError(
            "DURABLE.JOURNAL_FAILURE",
            str(exc) or type(exc).__name__,
            category="durable",
            phase="execute",
            details={"runId": run_id},
            cause=ensure_automation_error(exc),
        )


__all__ = [
    "DEFAULT_LEASE_TTL_SECONDS",
    "DURABLE_CHECKPOINT_SCHEMA_VERSION",
    "DurableExecutionResult",
    "DurableExecutor",
]
