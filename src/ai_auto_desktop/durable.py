"""Crash-safe orchestration for strictly serial durable workflows."""

from __future__ import annotations

from dataclasses import dataclass
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
)
from .model import RunResult, WorkflowDescriptor
from .plugin import ProcessPlugin
from .run_service import DispatchState, RunService, RunServiceError
from .runtime import (
    RUNTIME_STATE_SCHEMA_VERSION,
    RuntimeSegmentState,
    WorkflowRunner,
    canonical_plan_digest,
)


DURABLE_CHECKPOINT_SCHEMA_VERSION = 1
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
    ) -> None:
        self.journal = journal
        self.service = RunService(journal)
        self.owner_id = owner_id or f"runner-{uuid.uuid4().hex}"
        if isinstance(lease_ttl_seconds, bool) or lease_ttl_seconds <= 0:
            raise ValueError("lease_ttl_seconds must be positive")
        self.lease_ttl_seconds = float(lease_ttl_seconds)
        self.runner_factory = runner_factory

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

        self._assert_durable_plan(descriptor)
        actual_inputs = dict(inputs or {})
        run = self.service.create(
            run_id=run_id,
            workflow_name=descriptor.name,
            workflow_version=self._workflow_version(descriptor),
            plan_digest=canonical_plan_digest(descriptor),
            inputs=actual_inputs,
            descriptor=descriptor,
        )
        lease = self._claim(run.run_id)
        runner = self.runner_factory(
            descriptor,
            plugins=plugins,
            allow_scripts=allow_scripts,
            granted_permissions=granted_permissions,
        )
        try:
            state = runner.initialize(actual_inputs)
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
        except BaseException as exc:
            runner.close()
            if isinstance(exc, AutomationError):
                raise
            raise self._map_error(exc, run_id=run.run_id) from exc

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
        lease = self._claim(run_id)
        runner = self.runner_factory(
            descriptor,
            plugins=plugins,
            allow_scripts=allow_scripts,
            granted_permissions=granted_permissions,
        )
        try:
            state = runner.initialize(run.inputs)
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
        except BaseException as exc:
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
        if phase != "between_top_level_steps":
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
        if run.desired_state is DesiredState.CANCEL:
            return self._resume_cancellation(
                run, descriptor, checkpoint, lease, plugins,
                allow_scripts, granted_permissions,
            )
        runner = self.runner_factory(
            descriptor,
            plugins=plugins,
            allow_scripts=allow_scripts,
            granted_permissions=granted_permissions,
        )
        try:
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
                terminal = self._commit_result(run_id, lease, result)
                return DurableExecutionResult(terminal, result)
            if run.status is RunStatus.PAUSED:
                run, _ = self.journal.set_status_with_event(
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
            elif run.status is not RunStatus.RUNNING:
                raise self._error(
                    "DURABLE.INVALID_STATE",
                    f"run {run_id} cannot resume from {run.status.value}",
                    run=run,
                )
            return self._execute_loop(runner, run, lease)
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
                terminal = self._commit_result(run.run_id, lease, result)
                return DurableExecutionResult(terminal, result)

            prepared = runner.prepare_segment()
            if prepared.step_id is None:
                finalizing = runner.prepare_finalize()
                self.journal.save_checkpoint(
                    run.run_id, self._checkpoint(finalizing),
                    owner_id=lease.owner_id, token=lease.token, sensitive=False,
                )
                result = runner.finalize()
                terminal = self._commit_result(run.run_id, lease, result)
                return DurableExecutionResult(terminal, result)
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
                    terminal = self._commit_result(run.run_id, lease, result)
                    return DurableExecutionResult(terminal, result)
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
            terminal = self._commit_result(run.run_id, lease, result)
            return DurableExecutionResult(terminal, result)

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
        runner = self.runner_factory(
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
            terminal = self._commit_result(run.run_id, lease, result)
            return DurableExecutionResult(terminal, result)
        finally:
            runner.close()

    def _commit_result(
        self, run_id: str, lease: OwnerLease, result: RunResult
    ) -> RunRecord:
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
        current = self.service.get(run_id)
        terminal, _ = self.journal.set_status_with_event(
            run_id,
            expected=current.status,
            status=status,
            owner_id=lease.owner_id,
            token=lease.token,
            output=output,
            error=error,
            event_type="run.finished",
            event_payload={"status": status.value},
            sensitive=False,
        )
        return terminal

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
        if set(value) != required:
            raise self._error(
                "DURABLE.CHECKPOINT_INVALID",
                "checkpoint fields do not match the supported schema", run=run,
            )
        if value.get("checkpointSchemaVersion") != DURABLE_CHECKPOINT_SCHEMA_VERSION:
            raise self._error(
                "DURABLE.CHECKPOINT_VERSION",
                "checkpoint schema version is unsupported", run=run,
            )
        return value

    @staticmethod
    def _runtime_state(checkpoint: Mapping[str, Any]) -> dict[str, Any]:
        return {
            key: value
            for key, value in checkpoint.items()
            if key != "checkpointSchemaVersion"
        }

    @staticmethod
    def _assert_durable_plan(descriptor: WorkflowDescriptor) -> None:
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
        unsupported = [
            step.id
            for step in descriptor.all_steps()
            if step.type in {"action", "script"}
        ]
        if unsupported:
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "v0 durable execution rejects action and script outputs",
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
