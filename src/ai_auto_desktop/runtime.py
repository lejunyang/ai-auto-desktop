"""Synchronous v0 workflow execution engine."""

from __future__ import annotations

import ast
from concurrent.futures import Future, ThreadPoolExecutor
from contextlib import contextmanager
from dataclasses import dataclass
import fnmatch
import hashlib
import json
import math
import random
import re
import sys
import threading
import time
from collections.abc import Callable, Iterator, Mapping, Sequence
from typing import Any

from .compiler import parse_duration
from .errors import AutomationError, ensure_automation_error
from .expression import ExpressionError, evaluate_expression
from .model import MISSING, CompiledStep, ErrorHandler, RunResult, WorkflowDescriptor, freeze, thaw
from .plugin import PluginError, ProcessPlugin
from .script import execute_python_script

try:
    import jsonschema
except ImportError:  # pragma: no cover
    jsonschema = None

_TEMPLATE = re.compile(r"\$\{\{(.*?)\}\}", re.DOTALL)
RUNTIME_VERSION = "0.1.0"
RUNTIME_STATE_SCHEMA_VERSION = 1


@dataclass(frozen=True, slots=True)
class RuntimeSegmentState:
    """JSON-compatible runtime state captured at a top-level boundary."""

    schema_version: int
    runtime_version: str
    descriptor_digest: str
    phase: str
    next_top_level_index: int
    executed_attempts: int
    deadline_epoch_ms: int | None
    variables: dict[str, Any]
    context_steps: dict[str, Any]
    step_records: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        return {
            "schemaVersion": self.schema_version,
            "runtimeVersion": self.runtime_version,
            "descriptorDigest": self.descriptor_digest,
            "phase": self.phase,
            "nextTopLevelIndex": self.next_top_level_index,
            "executedAttempts": self.executed_attempts,
            "deadlineEpochMs": self.deadline_epoch_ms,
            "variables": _clone_runtime_value(self.variables),
            "contextSteps": _clone_runtime_value(self.context_steps),
            "stepRecords": _clone_runtime_value(self.step_records),
        }


@dataclass(frozen=True, slots=True)
class SegmentResult:
    """Outcome of executing at most one complete top-level step."""

    step_id: str | None
    terminal_ready: bool
    state: RuntimeSegmentState


@dataclass(frozen=True, slots=True)
class DurableActionBinding:
    """Immutable, canonical provider binding for one durable action."""

    contract: Mapping[str, Any]
    provider_digest: str
    contract_digest: str
    projection_digest: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "contract": _clone_runtime_value(self.contract),
            "providerDigest": self.provider_digest,
            "contractDigest": self.contract_digest,
            "projectionDigest": self.projection_digest,
        }


class _ReturnFlow(Exception):
    def __init__(self, value: Any) -> None:
        self.value = value


class _AttemptBudget:
    """Atomically account for attempts shared by parallel step runners."""

    def __init__(self, executed: int = 0) -> None:
        if isinstance(executed, bool) or not isinstance(executed, int) or executed < 0:
            raise ValueError("executed attempt count must be a non-negative integer")
        self.executed = executed
        self._lock = threading.Lock()

    def ensure_available(self, limit: int | None) -> None:
        if limit is None:
            return
        with self._lock:
            if self.executed >= limit:
                raise AutomationError(
                    "WORKFLOW.STEP_LIMIT",
                    "Workflow step budget exceeded",
                    details={"max_executed_steps": limit},
                )

    def reserve(self, limit: int | None) -> None:
        with self._lock:
            if limit is not None and self.executed >= limit:
                raise AutomationError(
                    "WORKFLOW.STEP_LIMIT",
                    "Workflow step budget exceeded",
                    details={"max_executed_steps": limit},
                )
            self.executed += 1

    def available(self, limit: int | None) -> int | None:
        if limit is None:
            return None
        with self._lock:
            return max(0, limit - self.executed)

    def reserve_batch(self, requested: int, limit: int | None) -> int:
        with self._lock:
            reserved = (
                requested
                if limit is None
                else min(requested, max(0, limit - self.executed))
            )
            self.executed += reserved
            return reserved

    def release(self, count: int = 1) -> None:
        with self._lock:
            self.executed = max(0, self.executed - count)


class _StepOutcome:
    def __init__(
        self,
        runner: "WorkflowRunner",
        *,
        error: AutomationError | None = None,
        returned: _ReturnFlow | None = None,
    ) -> None:
        self.runner = runner
        self.error = error
        self.returned = returned


class WorkflowRunner:
    def __init__(
        self,
        descriptor: WorkflowDescriptor,
        *,
        plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None = None,
        allow_scripts: bool = False,
        granted_permissions: Sequence[str] | None = None,
        event_sink: Callable[[Mapping[str, Any]], None] | None = None,
        durable_action_mode: str = "deny",
    ) -> None:
        if durable_action_mode not in {"deny", "read_only"}:
            raise ValueError(
                "durable_action_mode must be 'deny' or 'read_only'"
            )
        self.descriptor = descriptor
        self.durable_action_mode = durable_action_mode
        self.allow_scripts = allow_scripts
        self.granted_permissions = frozenset(granted_permissions or ())
        self.event_sink = event_sink
        self.plugins: dict[str, ProcessPlugin] = {}
        self._owned: set[str] = set()
        for name, plugin in (plugins or {}).items():
            if isinstance(plugin, ProcessPlugin):
                self.plugins[name] = plugin
            else:
                command = [plugin] if isinstance(plugin, str) else list(plugin)
                self.plugins[name] = ProcessPlugin(command, name=name)
                self._owned.add(name)
        self.events: list[dict[str, Any]] = []
        self.step_records: dict[str, dict[str, Any]] = {}
        self.context: dict[str, Any] = {}
        self.variables: dict[str, Any] = {}
        self._deadline: float | None = None
        self._deadline_stack: list[float] = []
        self._attempt_budget = _AttemptBudget()
        self._cancelled = threading.Event()
        self._run_lock = threading.Lock()
        self._pre_reserved_attempts = 0
        self._executed = 0
        self._segment_phase = "uninitialized"
        self._segment_next_index = 0
        self._segment_deadline_epoch_ms: int | None = None
        self._segment_output: Any = MISSING
        self._segment_error: AutomationError | None = None

    def run(self, inputs: Mapping[str, Any] | None = None) -> RunResult:
        if not self._run_lock.acquire(blocking=False):
            return RunResult(
                "failed",
                error=AutomationError(
                    "RUNTIME.INVALID_STATE",
                    "WorkflowRunner is already running",
                    category="runtime",
                ),
            )
        try:
            return self._run(inputs)
        finally:
            self._cancelled.clear()
            self._run_lock.release()

    def _run(self, inputs: Mapping[str, Any] | None = None) -> RunResult:
        self.events, self.step_records, self._executed = [], {}, 0
        self._deadline_stack = []
        self._attempt_budget = _AttemptBudget()
        self._pre_reserved_attempts = 0
        self._handler_output = MISSING
        budget = _duration(self.descriptor.budgets.get("max_duration"))
        self._deadline = time.monotonic() + budget if budget else None
        error: AutomationError | None = None
        output: Any = MISSING
        try:
            actual_inputs = self._prepare_inputs(dict(inputs or {}))
            self.context = {"inputs": actual_inputs, "vars": {}, "steps": {}}
            self.variables = self._prepare_variables()
            self.context["vars"] = self.variables
            self._check_requirements()
            try:
                self._run_steps(self.descriptor.steps)
            except _ReturnFlow as returned:
                output = returned.value
            except AutomationError as caught:
                error = self._apply_handler(self.descriptor.on_error, caught)
                if error is None:
                    output = getattr(self, "_handler_output", None)
            if output is MISSING and error is None:
                output = self._workflow_outputs()
        except _ReturnFlow as returned:
            output = returned.value
        except AutomationError as caught:
            error = caught
        except Exception as caught:
            error = ensure_automation_error(caught)

        cleanup_timeout = _duration(self.descriptor.budgets.get("cleanup_timeout"), 5.0) or 5.0
        original_deadline = self._deadline
        self._deadline = time.monotonic() + cleanup_timeout
        try:
            self._run_steps(self.descriptor.finally_steps, cleanup=True)
        except _ReturnFlow:
            pass
        except AutomationError as cleanup_error:
            if error is not None:
                error.add_suppressed(cleanup_error)
            else:
                error = AutomationError("WORKFLOW.FINALLY_FAILED", "Workflow cleanup failed", phase="cleanup", cause=cleanup_error)
        finally:
            self._deadline = original_deadline
            self._executed = self._attempt_budget.executed
            for name in self._owned:
                self.plugins[name].close()

        status = "succeeded"
        if error is not None:
            if error.code == "ACTION.UNKNOWN_EFFECT" or error.effect == "unknown": status = "unknown_effect"
            elif error.code in {
                "WORKFLOW.TIMEOUT",
                "ACTION.TIMEOUT",
                "STEP.TIMEOUT",
                "SCRIPT.TIMEOUT",
            }:
                status = "timed_out"
            elif error.code == "WORKFLOW.CANCELLED": status = "cancelled"
            else: status = "failed"
        return RunResult(
            status,
            None if output is MISSING else output,
            dict(self.variables),
            error,
            list(self.events),
            dict(self.step_records),
        )

    def close(self) -> None:
        for name in self._owned: self.plugins[name].close()

    def cancel(self) -> None:
        """Request cooperative cancellation and prevent new step dispatch."""

        self._cancelled.set()

    def initialize(
        self,
        inputs: Mapping[str, Any] | None = None,
        *,
        deadline_epoch_ms: int | None = None,
    ) -> RuntimeSegmentState:
        """Initialize the opt-in, top-level segmented execution API.

        The existing :meth:`run` path remains independent and keeps its DAG
        scheduling behavior.  Segments are intended for a durable coordinator
        that checkpoints only between complete top-level steps.
        """

        with self._segment_lock("initialize"):
            if self._segment_phase not in {"uninitialized", "finalized"}:
                raise self._segment_state_error(
                    "segmented execution is already initialized"
                )
            self.events, self.step_records, self._executed = [], {}, 0
            self._deadline_stack = []
            self._attempt_budget = _AttemptBudget()
            self._pre_reserved_attempts = 0
            self._handler_output = MISSING
            self._cancelled.clear()
            actual_inputs = self._prepare_inputs(dict(inputs or {}))
            self.context = {"inputs": actual_inputs, "vars": {}, "steps": {}}
            self.variables = self._prepare_variables()
            self.context["vars"] = self.variables
            if deadline_epoch_ms is None:
                budget = _duration(self.descriptor.budgets.get("max_duration"))
                deadline_epoch_ms = (
                    int((time.time() + budget) * 1_000)
                    if budget is not None
                    else None
                )
            self._set_segment_deadline(deadline_epoch_ms)
            self._check_control()
            self._check_requirements()
            self._segment_phase = "between_top_level_steps"
            self._segment_next_index = 0
            self._segment_output = MISSING
            self._segment_error = None
            return self._export_segment_state()

    def import_state(
        self,
        state: RuntimeSegmentState | Mapping[str, Any],
        *,
        inputs: Mapping[str, Any] | None = None,
        allow_expired: bool = False,
    ) -> RuntimeSegmentState:
        """Restore a safe segmented boundary without extending its deadline."""

        with self._segment_lock("import_state"):
            normalized = self._normalize_segment_state(state)
            if normalized.phase != "between_top_level_steps":
                raise self._segment_state_error(
                    "only a between_top_level_steps checkpoint is resumable",
                    details={"phase": normalized.phase},
                )
            self._restore_segment_state(
                normalized, inputs=inputs, allow_expired=allow_expired
            )
            return self._export_segment_state()

    def _restore_segment_state(
        self,
        normalized: RuntimeSegmentState,
        *,
        inputs: Mapping[str, Any] | None,
        allow_expired: bool,
    ) -> None:
        actual_inputs = self._prepare_inputs(dict(inputs or {}))
        variables = _clone_runtime_value(normalized.variables)
        context_steps = _clone_runtime_value(normalized.context_steps)
        step_records = _clone_runtime_value(normalized.step_records)
        self._validate_imported_segment_state(
            normalized, variables, context_steps, step_records
        )
        self.events = []
        self.variables = variables
        self.context = {
            "inputs": actual_inputs,
            "vars": self.variables,
            "steps": context_steps,
        }
        self.step_records = step_records
        self._deadline_stack = []
        self._attempt_budget = _AttemptBudget(normalized.executed_attempts)
        self._pre_reserved_attempts = 0
        self._executed = normalized.executed_attempts
        self._handler_output = MISSING
        self._cancelled.clear()
        self._segment_phase = normalized.phase
        self._segment_next_index = normalized.next_top_level_index
        self._segment_output = MISSING
        self._segment_error = None
        self._set_segment_deadline(normalized.deadline_epoch_ms)
        if not allow_expired:
            self._check_control()
        self._check_requirements()

    def export_state(self) -> RuntimeSegmentState:
        """Return a detached JSON-compatible segmented runtime snapshot."""

        with self._segment_lock("export_state"):
            if self._segment_phase == "uninitialized":
                raise self._segment_state_error(
                    "segmented execution has not been initialized"
                )
            return self._export_segment_state()

    def prepare_segment(self) -> SegmentResult:
        """Mark the next top-level step as entered without executing it.

        Durable coordinators persist the returned ``in_top_level_step`` state
        before calling :meth:`run_segment`.  Such a state is intentionally not
        resumable after a crash because the dispatch boundary is unknown.
        """

        with self._segment_lock("prepare_segment"):
            if self._segment_phase != "between_top_level_steps":
                raise self._segment_state_error(
                    "runner is not at a top-level segment boundary",
                    details={"phase": self._segment_phase},
                )
            if self._segment_next_index >= len(self.descriptor.steps):
                return SegmentResult(None, True, self._export_segment_state())
            step = self.descriptor.steps[self._segment_next_index]
            completed = {
                item.id
                for item in self.descriptor.steps[: self._segment_next_index]
            }
            if any(dependency not in completed for dependency in step.depends_on):
                raise self._segment_state_error(
                    f"top-level step {step.id!r} is not ready",
                    details={"dependsOn": list(step.depends_on)},
                )
            self._segment_phase = "in_top_level_step"
            return SegmentResult(step.id, False, self._export_segment_state())

    def run_segment(self) -> SegmentResult:
        """Execute at most one complete top-level step.

        A top-level control-flow step and all of its nested steps, handler, and
        finally block form one indivisible segment.  Exceptions and explicit
        returns are captured as terminal-ready state for :meth:`finalize`.
        """

        with self._segment_lock("run_segment"):
            if self._segment_phase == "between_top_level_steps":
                if self._segment_next_index >= len(self.descriptor.steps):
                    return SegmentResult(None, True, self._export_segment_state())
                self._prepare_segment_locked()
            if self._segment_phase != "in_top_level_step":
                raise self._segment_state_error(
                    "runner has no prepared top-level segment",
                    details={"phase": self._segment_phase},
                )
            step = self.descriptor.steps[self._segment_next_index]
            try:
                self._run_step(step)
            except _ReturnFlow as returned:
                self._segment_output = returned.value
                self._segment_phase = "finalizing"
                return SegmentResult(step.id, True, self._export_segment_state())
            except AutomationError as caught:
                try:
                    error = self._apply_handler(self.descriptor.on_error, caught)
                except _ReturnFlow as returned:
                    self._segment_output = returned.value
                    error = None
                if error is None and self._segment_output is MISSING:
                    self._segment_output = getattr(self, "_handler_output", None)
                self._segment_error = error
                self._segment_phase = "finalizing"
                return SegmentResult(step.id, True, self._export_segment_state())
            except Exception as caught:
                self._segment_error = ensure_automation_error(caught)
                self._segment_phase = "finalizing"
                return SegmentResult(step.id, True, self._export_segment_state())
            self._segment_next_index += 1
            self._segment_phase = "between_top_level_steps"
            return SegmentResult(
                step.id,
                self._segment_next_index == len(self.descriptor.steps),
                self._export_segment_state(),
            )

    def abort_prepared_segment(self) -> RuntimeSegmentState:
        """Return to the prior safe boundary before any segment execution."""

        with self._segment_lock("abort_prepared_segment"):
            if self._segment_phase != "in_top_level_step":
                raise self._segment_state_error(
                    "runner has no unexecuted prepared segment",
                    details={"phase": self._segment_phase},
                )
            self._segment_phase = "between_top_level_steps"
            return self._export_segment_state()

    def reserve_prepared_action_attempt(self) -> RuntimeSegmentState:
        """Reserve one attempt before a durable action intent is written."""

        with self._segment_lock("reserve_prepared_action_attempt"):
            self._require_prepared_durable_action()
            if self._pre_reserved_attempts:
                raise self._segment_state_error(
                    "prepared action attempt is already reserved"
                )
            self._attempt_budget.reserve(
                self.descriptor.budgets.get("max_executed_steps")
            )
            self._executed = self._attempt_budget.executed
            self._pre_reserved_attempts = 1
            return self._export_segment_state(allow_reserved_attempt=True)

    def export_action_intent_state(self) -> RuntimeSegmentState:
        """Export a reserved action attempt without exposing live values."""

        with self._segment_lock("export_action_intent_state"):
            self._require_prepared_durable_action()
            if self._pre_reserved_attempts != 1:
                raise self._segment_state_error(
                    "prepared action attempt is not reserved"
                )
            state = self._export_segment_state(allow_reserved_attempt=True)
            return RuntimeSegmentState(
                state.schema_version,
                state.runtime_version,
                state.descriptor_digest,
                "action_intent",
                state.next_top_level_index,
                state.executed_attempts,
                state.deadline_epoch_ms,
                state.variables,
                state.context_steps,
                state.step_records,
            )

    def restore_action_intent(
        self,
        state: RuntimeSegmentState | Mapping[str, Any],
        *,
        inputs: Mapping[str, Any] | None = None,
        allow_expired: bool = False,
    ) -> RuntimeSegmentState:
        """Restore a v2 read-only action intent with its consumed attempt."""

        with self._segment_lock("restore_action_intent"):
            normalized = self._normalize_segment_state(state)
            if normalized.phase != "action_intent":
                raise self._segment_state_error(
                    "only an action_intent checkpoint can be restored",
                    details={"phase": normalized.phase},
                )
            self._restore_segment_state(
                normalized, inputs=inputs, allow_expired=allow_expired
            )
            self._require_prepared_durable_action()
            if normalized.executed_attempts < 1:
                raise self._segment_state_error(
                    "action intent has no reserved attempt"
                )
            self._pre_reserved_attempts = 1
            return self._export_segment_state(allow_reserved_attempt=True)

    def release_prepared_action_attempt(self) -> RuntimeSegmentState:
        """Rollback a reservation when durable intent persistence loses CAS."""

        with self._segment_lock("release_prepared_action_attempt"):
            self._require_prepared_durable_action()
            if self._pre_reserved_attempts != 1:
                raise self._segment_state_error(
                    "prepared action attempt is not reserved"
                )
            self._pre_reserved_attempts = 0
            self._attempt_budget.release()
            self._executed = self._attempt_budget.executed
            return self._export_segment_state()

    def run_durable_action_segment(
        self,
        binding: DurableActionBinding | Mapping[str, Any],
        dispatch_deadline_epoch_ms: int | None,
    ) -> SegmentResult:
        """Execute one reserved durable action using its frozen binding."""

        with self._segment_lock("run_durable_action_segment"):
            step = self._require_prepared_durable_action()
            if self._pre_reserved_attempts != 1:
                raise self._segment_state_error(
                    "durable action attempt must be reserved before dispatch"
                )
            normalized = self._normalize_durable_binding(binding)
            deadline = self._validate_absolute_deadline(
                dispatch_deadline_epoch_ms, "dispatchDeadlineEpochMs"
            )
            try:
                self._run_step(
                    step,
                    action_contract=normalized.contract,
                    action_deadline_epoch_ms=deadline,
                )
            except AutomationError as caught:
                self._pre_reserved_attempts = 0
                self._segment_error = self._redact_durable_action_error(
                    caught, normalized.contract
                )
                self._segment_phase = "finalizing"
                return SegmentResult(
                    step.id, True, self._export_segment_state()
                )
            except Exception as caught:
                self._pre_reserved_attempts = 0
                self._segment_error = self._redact_durable_action_error(
                    ensure_automation_error(caught), normalized.contract
                )
                self._segment_phase = "finalizing"
                return SegmentResult(
                    step.id, True, self._export_segment_state()
                )
            self._segment_next_index += 1
            self._segment_phase = "between_top_level_steps"
            return SegmentResult(
                step.id,
                self._segment_next_index == len(self.descriptor.steps),
                self._export_segment_state(),
            )

    def _require_prepared_durable_action(self) -> CompiledStep:
        if self.durable_action_mode != "read_only":
            raise self._segment_state_error(
                "durable read-only action mode is not enabled"
            )
        if self._segment_phase not in {"in_top_level_step", "action_intent"}:
            raise self._segment_state_error(
                "runner has no prepared top-level action",
                details={"phase": self._segment_phase},
            )
        if self._segment_next_index >= len(self.descriptor.steps):
            raise self._segment_state_error("prepared action index is out of range")
        step = self.descriptor.steps[self._segment_next_index]
        if step.type != "action":
            raise self._segment_state_error(
                "prepared top-level step is not an action",
                details={"stepId": step.id},
            )
        return step

    def _prepare_segment_locked(self) -> None:
        step = self.descriptor.steps[self._segment_next_index]
        completed = {
            item.id for item in self.descriptor.steps[: self._segment_next_index]
        }
        if any(dependency not in completed for dependency in step.depends_on):
            raise self._segment_state_error(
                f"top-level step {step.id!r} is not ready",
                details={"dependsOn": list(step.depends_on)},
            )
        self._segment_phase = "in_top_level_step"

    def finalize(
        self, *, error: AutomationError | None = None
    ) -> RunResult:
        """Run workflow cleanup once and produce a normal :class:`RunResult`."""

        with self._segment_lock("finalize"):
            if self._segment_phase == "finalized":
                raise self._segment_state_error("workflow is already finalized")
            if self._segment_phase not in {
                "between_top_level_steps",
                "finalizing",
            }:
                raise self._segment_state_error(
                    "runner cannot finalize from its current phase",
                    details={"phase": self._segment_phase},
                )
            if (
                error is None
                and self._segment_error is None
                and self._segment_output is MISSING
                and self._segment_next_index < len(self.descriptor.steps)
            ):
                raise self._segment_state_error(
                    "workflow still has unexecuted top-level steps"
                )
            self._segment_phase = "finalizing"
            final_error = error if error is not None else self._segment_error
            output = self._segment_output
            if output is MISSING and final_error is None:
                try:
                    output = self._workflow_outputs()
                except AutomationError as caught:
                    final_error = caught
                except Exception as caught:
                    final_error = ensure_automation_error(caught)
            result = self._finalize_result(final_error, output)
            self._segment_phase = "finalized"
            self._cancelled.clear()
            return result

    def prepare_finalize(self) -> RuntimeSegmentState:
        """Enter the non-resumable workflow-finalization phase."""

        with self._segment_lock("prepare_finalize"):
            if self._segment_phase == "finalizing":
                return self._export_segment_state()
            if self._segment_phase != "between_top_level_steps":
                raise self._segment_state_error(
                    "runner cannot enter finalization from its current phase",
                    details={"phase": self._segment_phase},
                )
            self._segment_phase = "finalizing"
            return self._export_segment_state()

    def request_segment_cancellation(self) -> None:
        """Mark an initialized segmented run for cooperative cancellation."""

        self._cancelled.set()
        if self._segment_phase == "between_top_level_steps":
            self._segment_error = AutomationError(
                "WORKFLOW.CANCELLED",
                "Workflow execution was cancelled",
                category="workflow",
                phase="execute",
            )
            self._segment_phase = "finalizing"

    def _finalize_result(
        self, error: AutomationError | None, output: Any
    ) -> RunResult:
        cleanup_timeout = (
            _duration(self.descriptor.budgets.get("cleanup_timeout"), 5.0)
            or 5.0
        )
        original_deadline = self._deadline
        self._deadline = time.monotonic() + cleanup_timeout
        try:
            self._run_steps(self.descriptor.finally_steps, cleanup=True)
        except _ReturnFlow:
            pass
        except AutomationError as cleanup_error:
            if error is not None:
                error.add_suppressed(cleanup_error)
            else:
                error = AutomationError(
                    "WORKFLOW.FINALLY_FAILED",
                    "Workflow cleanup failed",
                    phase="cleanup",
                    cause=cleanup_error,
                )
        finally:
            self._deadline = original_deadline
            self._executed = self._attempt_budget.executed
            for name in self._owned:
                self.plugins[name].close()

        status = "succeeded"
        if error is not None:
            if error.code == "ACTION.UNKNOWN_EFFECT" or error.effect == "unknown":
                status = "unknown_effect"
            elif error.code in {
                "WORKFLOW.TIMEOUT",
                "ACTION.TIMEOUT",
                "STEP.TIMEOUT",
                "SCRIPT.TIMEOUT",
            }:
                status = "timed_out"
            elif error.code == "WORKFLOW.CANCELLED":
                status = "cancelled"
            else:
                status = "failed"
        return RunResult(
            status,
            None if output is MISSING else output,
            dict(self.variables),
            error,
            list(self.events),
            dict(self.step_records),
        )

    @contextmanager
    def _segment_lock(self, operation: str) -> Iterator[None]:
        if not self._run_lock.acquire(blocking=False):
            raise self._segment_state_error(
                "WorkflowRunner is already running",
                details={"operation": operation},
            )
        try:
            yield
        finally:
            self._run_lock.release()

    def _set_segment_deadline(self, deadline_epoch_ms: int | None) -> None:
        if deadline_epoch_ms is not None and (
            isinstance(deadline_epoch_ms, bool)
            or not isinstance(deadline_epoch_ms, int)
            or deadline_epoch_ms < 0
        ):
            raise self._segment_state_error(
                "deadlineEpochMs must be a non-negative integer or null"
            )
        self._segment_deadline_epoch_ms = deadline_epoch_ms
        self._deadline = (
            None
            if deadline_epoch_ms is None
            else time.monotonic()
            + max(0.0, deadline_epoch_ms / 1_000 - time.time())
        )

    def _export_segment_state(
        self, *, allow_reserved_attempt: bool = False
    ) -> RuntimeSegmentState:
        if self._deadline_stack or (
            self._pre_reserved_attempts and not allow_reserved_attempt
        ):
            raise self._segment_state_error(
                "runtime state cannot be exported inside a step or attempt"
            )
        try:
            return RuntimeSegmentState(
                RUNTIME_STATE_SCHEMA_VERSION,
                RUNTIME_VERSION,
                canonical_plan_digest(self.descriptor),
                self._segment_phase,
                self._segment_next_index,
                self._attempt_budget.executed,
                self._segment_deadline_epoch_ms,
                _clone_runtime_value(self.variables),
                _clone_runtime_value(self.context.get("steps", {})),
                _clone_runtime_value(self.step_records),
            )
        except (TypeError, ValueError) as exc:
            raise self._segment_state_error(
                "runtime state is not finite JSON data", cause=exc
            ) from exc

    def _normalize_segment_state(
        self, state: RuntimeSegmentState | Mapping[str, Any]
    ) -> RuntimeSegmentState:
        if isinstance(state, RuntimeSegmentState):
            return state
        if not isinstance(state, Mapping):
            raise self._segment_state_error("runtime state must be an object")
        required = {
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
        if set(state) != required:
            raise self._segment_state_error(
                "runtime state fields do not match the supported schema",
                details={
                    "missing": sorted(required - set(state)),
                    "unknown": sorted(set(state) - required),
                },
            )
        return RuntimeSegmentState(
            state["schemaVersion"],
            state["runtimeVersion"],
            state["descriptorDigest"],
            state["phase"],
            state["nextTopLevelIndex"],
            state["executedAttempts"],
            state["deadlineEpochMs"],
            state["variables"],
            state["contextSteps"],
            state["stepRecords"],
        )

    def _validate_imported_segment_state(
        self,
        state: RuntimeSegmentState,
        variables: Any,
        context_steps: Any,
        step_records: Any,
    ) -> None:
        if (
            isinstance(state.schema_version, bool)
            or not isinstance(state.schema_version, int)
            or state.schema_version != RUNTIME_STATE_SCHEMA_VERSION
        ):
            raise self._segment_state_error(
                "runtime state schema version is unsupported",
                details={"schemaVersion": state.schema_version},
            )
        if not isinstance(state.runtime_version, str) or state.runtime_version != RUNTIME_VERSION:
            raise self._segment_state_error(
                "runtime state version does not match this runtime",
                details={
                    "checkpointRuntimeVersion": state.runtime_version,
                    "runtimeVersion": RUNTIME_VERSION,
                },
            )
        expected_digest = canonical_plan_digest(self.descriptor)
        if not isinstance(state.descriptor_digest, str) or state.descriptor_digest != expected_digest:
            raise self._segment_state_error(
                "runtime state descriptor digest does not match",
                details={
                    "checkpointDescriptorDigest": state.descriptor_digest,
                    "descriptorDigest": expected_digest,
                },
            )
        index = state.next_top_level_index
        attempts = state.executed_attempts
        if isinstance(index, bool) or not isinstance(index, int) or not 0 <= index <= len(self.descriptor.steps):
            raise self._segment_state_error("nextTopLevelIndex is out of range")
        if isinstance(attempts, bool) or not isinstance(attempts, int) or attempts < 0:
            raise self._segment_state_error("executedAttempts must be non-negative")
        if not isinstance(variables, dict) or not isinstance(context_steps, dict) or not isinstance(step_records, dict):
            raise self._segment_state_error(
                "variables, contextSteps, and stepRecords must be objects"
            )
        unknown_variables = set(variables) - set(self.descriptor.variables)
        known_steps = {step.id for step in self.descriptor.all_steps()}
        unknown_steps = (set(context_steps) | set(step_records)) - known_steps
        completed_ids = {step.id for step in self.descriptor.steps[:index]}
        if unknown_variables or unknown_steps or not completed_ids.issubset(context_steps):
            raise self._segment_state_error(
                "runtime state does not match the descriptor",
                details={
                    "unknownVariables": sorted(unknown_variables),
                    "unknownSteps": sorted(unknown_steps),
                    "missingCompletedSteps": sorted(completed_ids - set(context_steps)),
                },
            )

    @staticmethod
    def _validate_absolute_deadline(
        value: int | None, name: str
    ) -> int | None:
        if value is not None and (
            isinstance(value, bool) or not isinstance(value, int) or value < 0
        ):
            raise AutomationError(
                "RUNTIME.STATE_INVALID",
                f"{name} must be a non-negative integer or null",
                category="runtime",
                phase="restore",
            )
        return value

    @staticmethod
    def _normalize_durable_binding(
        binding: DurableActionBinding | Mapping[str, Any],
    ) -> DurableActionBinding:
        if isinstance(binding, DurableActionBinding):
            return binding
        if not isinstance(binding, Mapping):
            raise AutomationError(
                "DURABLE.BINDING_MISMATCH",
                "durable action binding must be an object",
                category="durable",
            )
        required = {
            "contract", "providerDigest", "contractDigest",
            "projectionDigest",
        }
        if set(binding) != required or not isinstance(
            binding.get("contract"), Mapping
        ):
            raise AutomationError(
                "DURABLE.BINDING_MISMATCH",
                "durable action binding fields are invalid",
                category="durable",
            )
        digests = [
            binding["providerDigest"], binding["contractDigest"],
            binding["projectionDigest"],
        ]
        if any(not _is_sha256_digest(value) for value in digests):
            raise AutomationError(
                "DURABLE.BINDING_MISMATCH",
                "durable action binding digests are invalid",
                category="durable",
            )
        return DurableActionBinding(
            _clone_runtime_value(binding["contract"]),
            binding["providerDigest"],
            binding["contractDigest"],
            binding["projectionDigest"],
        )

    @staticmethod
    def _segment_state_error(
        message: str,
        *,
        details: Mapping[str, Any] | None = None,
        cause: BaseException | None = None,
    ) -> AutomationError:
        return AutomationError(
            "RUNTIME.STATE_INVALID",
            message,
            category="runtime",
            phase="restore",
            details=details,
            cause=cause,
        )

    def _new_execution_worker(self) -> "WorkflowRunner":
        worker = object.__new__(WorkflowRunner)
        # Workers are populated from an initialized parent in
        # ``_run_parallel_batch``.  Set the new opt-in mode here as a defensive
        # default so an exception during population cannot turn into an
        # unrelated AttributeError.
        worker.durable_action_mode = "deny"
        return worker

    def _prepare_inputs(self, supplied: dict[str, Any]) -> dict[str, Any]:
        extras = set(supplied) - set(self.descriptor.inputs)
        if extras: raise AutomationError("INPUT.UNKNOWN", f"Unknown inputs: {', '.join(sorted(extras))}", category="input")
        result: dict[str, Any] = {}
        for name, definition in self.descriptor.inputs.items():
            if name in supplied: value = supplied[name]
            elif "default" in definition: value = thaw(definition["default"])
            elif definition.get("required", False): raise AutomationError("INPUT.REQUIRED", f"Required input {name!r} is missing", category="input")
            else: continue
            self._validate_schema(value, definition.get("schema"), "INPUT.INVALID", name)
            result[name] = value
        return result

    def _prepare_variables(self) -> dict[str, Any]:
        result: dict[str, Any] = {}
        local = {"inputs": self.context["inputs"], "vars": result, "steps": {}}
        for name, definition in self.descriptor.variables.items():
            if "initial" in definition:
                value = self._evaluate(thaw(definition["initial"]), local)
                self._validate_schema(value, definition.get("schema"), "VARIABLE.INVALID", name)
                result[name] = value
        return result

    def _workflow_outputs(self) -> Any:
        if not self.descriptor.outputs: return None
        result: dict[str, Any] = {}
        for name, definition in self.descriptor.outputs.items():
            value = self._evaluate(thaw(definition["value"]))
            self._validate_schema(value, definition.get("schema"), "OUTPUT.INVALID", name)
            result[name] = value
        return result

    def _check_requirements(self) -> None:
        requirements = thaw(self.descriptor.requires)
        runtime_range = requirements.get("runtime")
        if runtime_range and not _version_matches(RUNTIME_VERSION, runtime_range):
            raise AutomationError(
                "DESCRIPTOR.VERSION_UNSUPPORTED",
                f"Runtime {RUNTIME_VERSION} does not satisfy {runtime_range!r}",
                category="descriptor",
            )
        current_platform = _platform_name()
        allowed_platforms = requirements.get("platforms")
        if allowed_platforms and current_platform not in allowed_platforms:
            raise AutomationError(
                "CAPABILITY.PLATFORM_UNSUPPORTED",
                f"Workflow does not support platform {current_platform!r}",
                category="capability",
            )
        missing_permissions = sorted(
            set(requirements.get("permissions", ())) - self.granted_permissions
        )
        if missing_permissions:
            raise AutomationError(
                "POLICY.DENIED",
                "Workflow permissions were not granted",
                category="policy",
                details={"missing_permissions": missing_permissions},
            )
        for required in requirements.get("capabilities", ()):
            name = required["name"]
            plugin = self.plugins.get(name)
            if plugin is None:
                if required.get("optional", False):
                    continue
                raise AutomationError(
                    "CAPABILITY.MISSING",
                    f"Required capability {name!r} is not registered",
                    category="capability",
                )
            manifest = self._plugin_manifest(
                plugin, timeout_code="WORKFLOW.TIMEOUT"
            )
            if manifest.get("metadata", {}).get("name") != name:
                raise AutomationError(
                    "CAPABILITY.MISSING",
                    f"Registered plugin does not provide {name!r}",
                    category="capability",
                )
            version_range = required.get("version")
            version = manifest.get("metadata", {}).get("version")
            if version_range and (not isinstance(version, str) or not _version_matches(version, version_range)):
                raise AutomationError(
                    "CAPABILITY.VERSION_INCOMPATIBLE",
                    f"Capability {name!r} version {version!r} does not satisfy {version_range!r}",
                    category="capability",
                )
            missing_actions = sorted(
                set(required.get("actions", ()))
                - set(manifest.get("actions", {}))
            )
            if missing_actions:
                raise AutomationError(
                    "CAPABILITY.MISSING",
                    f"Capability {name!r} is missing required actions",
                    category="capability",
                    details={"actions": missing_actions},
                )

    def _validate_schema(
        self,
        value: Any,
        schema: Any,
        code: str,
        name: str,
        *,
        redact: bool = False,
    ) -> None:
        if schema is None or schema is True: return
        if jsonschema is None:
            raise AutomationError(
                "RUNTIME.DEPENDENCY_MISSING",
                "jsonschema is required to enforce value contracts",
                category="runtime",
            )
        try: jsonschema.validate(value, thaw(schema))
        except Exception as exc:
            raise AutomationError(
                code,
                f"{name!r} does not satisfy its schema",
                details={} if redact else {"validation": str(exc)},
                cause=None if redact else exc,
            ) from (None if redact else exc)

    def _evaluate(self, value: Any, context: Mapping[str, Any] | None = None) -> Any:
        context = context or self.context
        if isinstance(value, Mapping): return {key: self._evaluate(item, context) for key, item in value.items()}
        if isinstance(value, (list, tuple)): return [self._evaluate(item, context) for item in value]
        if not isinstance(value, str): return value
        matches = list(_TEMPLATE.finditer(value))
        if not matches: return value
        try:
            if len(matches) == 1 and matches[0].span() == (0, len(value)):
                return evaluate_expression(matches[0].group(1).strip(), context)
            cursor, chunks = 0, []
            for match in matches:
                chunks += [value[cursor:match.start()], str(evaluate_expression(match.group(1).strip(), context))]
                cursor = match.end()
            chunks.append(value[cursor:]); return "".join(chunks)
        except ExpressionError as exc: raise AutomationError("EXPR.EVALUATION_FAILED", str(exc), category="expr", cause=exc) from exc

    def _remaining(self, local_deadline: float | None = None) -> float | None:
        deadlines = [
            item
            for item in (self._deadline, local_deadline, *self._deadline_stack)
            if item is not None
        ]
        return max(0.0, min(deadlines) - time.monotonic()) if deadlines else None

    def _check_control(self, cleanup: bool = False) -> None:
        if self._cancelled.is_set() and not cleanup:
            raise AutomationError(
                "WORKFLOW.CANCELLED",
                "Workflow execution was cancelled",
                phase="execute",
            )
        if self._remaining() == 0: raise AutomationError("WORKFLOW.TIMEOUT", "Workflow deadline exceeded", phase="execute")

    def _check_budget(self, cleanup: bool = False) -> None:
        self._check_control(cleanup)
        if not cleanup and not self._pre_reserved_attempts:
            self._attempt_budget.ensure_available(
                self.descriptor.budgets.get("max_executed_steps")
            )

    def _sleep_interruptibly(
        self,
        seconds: float,
        local_deadline: float | None = None,
        *,
        cleanup: bool = False,
    ) -> None:
        deadline = time.monotonic() + seconds
        while True:
            if self._cancelled.is_set() and not cleanup:
                raise AutomationError(
                    "WORKFLOW.CANCELLED",
                    "Workflow execution was cancelled",
                    phase="execute",
                )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            parent_remaining = self._remaining(local_deadline)
            if parent_remaining == 0:
                return
            wait_for = min(remaining, 0.05)
            if parent_remaining is not None:
                wait_for = min(wait_for, parent_remaining)
            self._cancelled.wait(wait_for)

    def _event(self, event: str, **fields: Any) -> None:
        record = {"event": event, "time": time.time(), **fields}; self.events.append(record)
        if self.event_sink: self.event_sink(record)

    def _run_steps(self, steps: Sequence[CompiledStep], cleanup: bool = False) -> None:
        if not steps:
            return
        max_concurrency = (
            1
            if cleanup
            else int(self.descriptor.budgets.get("max_concurrency", 1))
        )
        pending = {step.id: step for step in steps}
        completed: set[str] = set()
        order = {step.id: index for index, step in enumerate(steps)}

        while pending:
            try:
                self._check_control(cleanup)
            except AutomationError as control_error:
                for step in steps:
                    if step.id in pending:
                        self._mark_scope_terminated(
                            step, control_error.code.lower()
                        )
                raise
            ready = [
                step
                for step in steps
                if step.id in pending
                and all(dependency in completed for dependency in step.depends_on)
            ]
            if not ready:
                raise AutomationError(
                    "RUNTIME.INVALID_PLAN",
                    "Sibling step dependencies cannot be scheduled",
                    category="runtime",
                )

            first = ready[0]
            parallel: list[tuple[CompiledStep, Mapping[str, Any]]] = []
            if max_concurrency > 1 and not cleanup:
                parallel_plugins: set[int] = set()
                available = self._attempt_budget.available(
                    self.descriptor.budgets.get("max_executed_steps")
                )
                candidate_limit = (
                    max_concurrency
                    if available is None
                    else min(max_concurrency, available)
                )
                for step in ready:
                    if len(parallel) >= candidate_limit:
                        break
                    try:
                        contract = self._parallel_action_contract(step)
                    except AutomationError:
                        parallel = []
                        break
                    if contract is None:
                        parallel = []
                        break
                    capability = step.params["uses"].rsplit(".", 1)[0]
                    plugin_identity = id(self.plugins[capability])
                    if plugin_identity in parallel_plugins:
                        break
                    parallel.append((step, contract))
                    parallel_plugins.add(plugin_identity)

            if len(parallel) < 2:
                del pending[first.id]
                self._run_step(first, cleanup)
                completed.add(first.id)
                continue

            for step, _ in parallel:
                del pending[step.id]
            outcomes = self._run_parallel_actions(parallel)
            started_ids = set(outcomes)
            for step, _ in parallel:
                if step.id in started_ids:
                    self.step_records.update(
                        outcomes[step.id].runner.step_records
                    )
            for step, _ in parallel:
                if step.id not in started_ids:
                    pending[step.id] = step
            terminal_order = next(
                (
                    (order[step.id], step.id)
                    for step, _ in parallel
                    if step.id in started_ids
                    and (
                        outcomes[step.id].error is not None
                        or outcomes[step.id].returned is not None
                    )
                ),
                None,
            )
            terminal: _StepOutcome | None = None
            try:
                for step, _ in sorted(
                    (item for item in parallel if item[0].id in started_ids),
                    key=lambda item: order[item[0].id],
                ):
                    outcome = outcomes[step.id]
                    if (
                        terminal_order is None
                        or order[step.id] <= terminal_order[0]
                    ):
                        self._merge_parallel_outcome(step, outcome)
                        completed.add(step.id)
                    else:
                        self._record_parallel_terminal_only(step, outcome)
                    if terminal is None and (
                        outcome.error is not None
                        or outcome.returned is not None
                    ):
                        terminal = outcome
            except AutomationError as merge_error:
                for step, _ in parallel:
                    if step.id in started_ids and step.id not in completed:
                        self.step_records[step.id][
                            "discarded_due_to_context_conflict"
                        ] = True
                for step in steps:
                    if step.id in pending:
                        self._mark_scope_terminated(step, "context_merge")
                raise merge_error
            if terminal is not None:
                assert terminal_order is not None
                terminal_step_id = terminal_order[1]
                pending_ids = set(pending)
                for step in steps:
                    if step.id in pending_ids:
                        self._mark_scope_terminated(step, terminal_step_id)
                if terminal.returned is not None:
                    raise terminal.returned
                assert terminal.error is not None
                raise terminal.error
        self._check_control(cleanup)

    def _run_unwind_steps(self, steps: Sequence[CompiledStep]) -> None:
        if not steps:
            return
        cleanup_timeout = (
            _duration(self.descriptor.budgets.get("cleanup_timeout"), 5.0)
            or 5.0
        )
        previous_deadline = self._deadline
        previous_stack = self._deadline_stack
        self._deadline = time.monotonic() + cleanup_timeout
        self._deadline_stack = []
        try:
            self._run_steps(steps, cleanup=True)
        finally:
            self._deadline = previous_deadline
            self._deadline_stack = previous_stack

    def _parallel_action_contract(
        self, step: CompiledStep
    ) -> Mapping[str, Any] | None:
        """Return a contract only when a step is safe for parallel isolation."""

        if (
            step.type != "action"
            or step.on_error is not None
            or step.finally_steps
            or "if" in step.params
            or "retry" in step.params
            or int(
                thaw(
                    step.params.get(
                        "retry",
                        self.descriptor.defaults.get(
                            "retry", {"max_attempts": 1}
                        ),
                    )
                ).get("max_attempts", 1)
            )
            != 1
        ):
            return None
        contract = self._resolve_action_contract(step)
        if self._effective_action_effect(step, contract=contract) != "read_only":
            return None
        permissions = set(contract.get("permissions", ()))
        plugin = self.plugins[step.params["uses"].rsplit(".", 1)[0]]
        if isinstance(plugin.manifest, Mapping):
            permissions.update(plugin.manifest.get("permissions", ()))
        uses = step.params["uses"]
        capability = uses.rsplit(".", 1)[0]
        if (
            "desktop.input" in permissions
            or capability == "desktop"
            or capability.startswith("desktop.")
        ):
            return None
        return contract

    def _run_parallel_actions(
        self, steps: Sequence[tuple[CompiledStep, Mapping[str, Any]]]
    ) -> dict[str, _StepOutcome]:
        try:
            base_context = _clone_runtime_value(self.context)
            base_variables = _clone_runtime_value(self.variables)
        except Exception as error:
            raise AutomationError(
                "RUNTIME.CONTEXT_CONFLICT",
                "Parallel action context could not be isolated",
                category="runtime",
                cause=error,
            ) from error
        workers: list[tuple[CompiledStep, WorkflowRunner]] = []
        limit = self.descriptor.budgets.get("max_executed_steps")
        available = self._attempt_budget.available(limit)
        if available is not None:
            steps = steps[:available]
        for step, _ in steps:
            worker = self._parallel_worker(base_context, base_variables)
            workers.append((step, worker))

        reserved = self._attempt_budget.reserve_batch(len(workers), limit)
        workers = workers[:reserved]
        if not workers:
            self._attempt_budget.ensure_available(limit)
            raise AssertionError("unreachable")

        futures: dict[str, Future[_StepOutcome]] = {}
        event_lock = threading.Lock()

        def publish(event: Mapping[str, Any]) -> None:
            with event_lock:
                record = dict(event)
                record["time"] = time.time()
                self.events.append(record)
                if self.event_sink:
                    self.event_sink(record)

        submitted = 0
        executor: ThreadPoolExecutor | None = None
        try:
            executor = ThreadPoolExecutor(max_workers=len(workers))
            for (step, worker), (_, contract) in zip(workers, steps):
                worker.event_sink = publish
                futures[step.id] = executor.submit(
                    self._parallel_step_outcome, worker, step, contract
                )
                submitted += 1
            return {
                step.id: futures[step.id].result() for step, _ in workers
            }
        finally:
            if executor is not None:
                executor.shutdown(wait=True)
            unused = reserved - submitted
            if unused:
                self._attempt_budget.release(unused)
                for _, worker in workers[submitted:]:
                    worker._pre_reserved_attempts = 0

    def _parallel_worker(
        self, context: Mapping[str, Any], variables: Mapping[str, Any]
    ) -> "WorkflowRunner":
        try:
            worker = self._new_execution_worker()
            worker.descriptor = self.descriptor
            worker.durable_action_mode = self.durable_action_mode
            worker.allow_scripts = self.allow_scripts
            worker.granted_permissions = self.granted_permissions
            worker.event_sink = None
            worker.plugins = self.plugins
            worker._owned = set()
            worker.events = []
            worker.step_records = {}
            worker.context = _clone_runtime_value(context)
            worker.variables = _clone_runtime_value(variables)
        except Exception as error:
            raise AutomationError(
                "RUNTIME.CONTEXT_CONFLICT",
                "Parallel action context could not be isolated",
                category="runtime",
                cause=error,
            ) from error
        worker.context["vars"] = worker.variables
        worker._deadline = self._deadline
        worker._deadline_stack = list(self._deadline_stack)
        worker._attempt_budget = self._attempt_budget
        worker._cancelled = self._cancelled
        worker._pre_reserved_attempts = 1
        worker._executed = 0
        worker._handler_output = MISSING
        return worker

    @staticmethod
    def _parallel_step_outcome(
        worker: "WorkflowRunner",
        step: CompiledStep,
        contract: Mapping[str, Any],
    ) -> _StepOutcome:
        try:
            worker._run_step(step, action_contract=contract)
        except _ReturnFlow as returned:
            return _StepOutcome(worker, returned=returned)
        except AutomationError as error:
            return _StepOutcome(worker, error=error)
        except Exception as error:  # pragma: no cover - defensive boundary
            return _StepOutcome(worker, error=ensure_automation_error(error))
        return _StepOutcome(worker)

    def _merge_parallel_outcome(
        self, step: CompiledStep, outcome: _StepOutcome
    ) -> None:
        worker = outcome.runner
        try:
            variables_unchanged = _values_equal(
                worker.variables, self.variables
            )
        except Exception as error:
            raise AutomationError(
                "RUNTIME.CONTEXT_CONFLICT",
                f"Read-only step {step.id!r} produced incomparable variables",
                category="runtime",
                details={"step_id": step.id},
                cause=error,
            ) from error
        if not variables_unchanged:
            raise AutomationError(
                "RUNTIME.CONTEXT_CONFLICT",
                f"Read-only step {step.id!r} modified workflow variables",
                category="runtime",
                details={"step_id": step.id},
            )
        original_keys = set(self.context) - {"steps", "vars"}
        worker_keys = set(worker.context) - {"steps", "vars"}
        try:
            context_unchanged = worker_keys == original_keys and all(
                _values_equal(worker.context[key], self.context[key])
                for key in original_keys
            )
        except Exception as error:
            raise AutomationError(
                "RUNTIME.CONTEXT_CONFLICT",
                f"Read-only step {step.id!r} produced incomparable context",
                category="runtime",
                details={"step_id": step.id},
                cause=error,
            ) from error
        if not context_unchanged:
            raise AutomationError(
                "RUNTIME.CONTEXT_CONFLICT",
                f"Read-only step {step.id!r} modified workflow context",
                category="runtime",
                details={"step_id": step.id},
            )
        for step_id, record in worker.context["steps"].items():
            if step_id in self.context["steps"]:
                try:
                    same_record = _values_equal(
                        self.context["steps"][step_id], record
                    )
                except Exception as error:
                    raise AutomationError(
                        "RUNTIME.CONTEXT_CONFLICT",
                        f"Parallel step {step.id!r} produced incomparable step context",
                        category="runtime",
                        details={"step_id": step.id, "conflict": step_id},
                        cause=error,
                    ) from error
                if not same_record:
                    raise AutomationError(
                        "RUNTIME.CONTEXT_CONFLICT",
                        f"Parallel step {step.id!r} rewrote existing step context",
                        category="runtime",
                        details={"step_id": step.id, "conflict": step_id},
                    )
                continue
            if step_id != step.id:
                raise AutomationError(
                    "RUNTIME.CONTEXT_CONFLICT",
                    f"Parallel step {step.id!r} wrote another step context",
                    category="runtime",
                    details={"step_id": step.id, "conflict": step_id},
                )
            self.context["steps"][step_id] = record
        # Worker events are published live by ``event_sink``; only step state
        # is merged here, in descriptor order.

    def _record_parallel_terminal_only(
        self, step: CompiledStep, outcome: _StepOutcome
    ) -> None:
        """Keep diagnostics for in-flight peers without publishing outputs."""

        record = dict(
            outcome.runner.step_records.get(
                step.id, {"status": "failed", "attempts": 0}
            )
        )
        record["discarded_due_to_scope_termination"] = True
        self.step_records[step.id] = record

    def _mark_scope_terminated(
        self, step: CompiledStep, terminal_step_id: str
    ) -> None:
        self.step_records[step.id] = {
            "status": "skipped",
            "reason": "scope_terminated",
            "terminated_by": terminal_step_id,
        }
        self.context["steps"][step.id] = {
            "status": "skipped",
            "output": None,
        }
        self._event(
            "step.skipped",
            step_id=step.id,
            reason="scope_terminated",
            terminated_by=terminal_step_id,
        )

    def _run_step(
        self,
        step: CompiledStep,
        cleanup: bool = False,
        *,
        action_contract: Mapping[str, Any] | None = None,
        action_deadline_epoch_ms: int | None = None,
    ) -> Any:
        self._check_control(cleanup)
        if "if" in step.params and not bool(self._evaluate(thaw(step.params["if"]))):
            self.step_records[step.id] = {"status": "skipped"}
            self.context["steps"][step.id] = {"status": "skipped", "output": None}
            self._event("step.skipped", step_id=step.id)
            self._run_steps(step.finally_steps, cleanup)
            return None
        started = time.monotonic()
        self.step_records[step.id] = {"status": "running", "attempts": 0}; self._event("step.started", step_id=step.id, step_type=step.type)
        timeout = _duration(step.params.get("timeout"), _duration(self.descriptor.defaults.get("timeout")))
        local_deadline = started + timeout if timeout else None
        if action_deadline_epoch_ms is not None:
            frozen_deadline = self._validate_absolute_deadline(
                action_deadline_epoch_ms, "dispatchDeadlineEpochMs"
            )
            assert frozen_deadline is not None
            durable_deadline = time.monotonic() + max(
                0.0, frozen_deadline / 1_000 - time.time()
            )
            local_deadline = (
                durable_deadline
                if local_deadline is None
                else min(local_deadline, durable_deadline)
            )
        if local_deadline is not None:
            self._deadline_stack.append(local_deadline)
        pending: AutomationError | None = None
        result: Any = None
        returned = False
        try:
            result = self._attempt_step(
                step, local_deadline, cleanup, action_contract=action_contract
            )
            self.context["steps"][step.id] = {"status": "succeeded", "output": result}
        except _ReturnFlow:
            returned = True
            raise
        except AutomationError as caught:
            caught.at_step(step.id, step_path=step.path, workflow=self.descriptor.name)
            if step.type == "action" and self.durable_action_mode == "read_only":
                caught = self._redact_durable_action_error(
                    caught, action_contract or {}
                )
                caught.at_step(
                    step.id, step_path=step.path, workflow=self.descriptor.name
                )
            if caught.code in {"WORKFLOW.CANCELLED", "WORKFLOW.TIMEOUT"}:
                pending = caught
            else:
                pending = self._apply_handler(step.on_error, caught)
            if pending is not None:
                status = "unknown_effect" if pending.effect == "unknown" else "timed_out" if pending.code.endswith(".TIMEOUT") else "failed"
                self.step_records[step.id].update(status=status, error=pending.to_dict(), duration_ms=round((time.monotonic() - started) * 1000, 3))
                raise pending
            result = getattr(self, "_handler_output", None); self.context["steps"][step.id] = {"status": "continued", "output": result}
        finally:
            if local_deadline is not None:
                self._deadline_stack.pop()
            try:
                if cleanup:
                    self._run_steps(step.finally_steps, cleanup=True)
                elif pending is not None and pending.code in {
                    "WORKFLOW.CANCELLED",
                    "WORKFLOW.TIMEOUT",
                    "STEP.TIMEOUT",
                    "ACTION.TIMEOUT",
                    "SCRIPT.TIMEOUT",
                }:
                    self._run_unwind_steps(step.finally_steps)
                else:
                    self._run_steps(step.finally_steps)
            except AutomationError as final_error:
                if pending is not None:
                    pending.add_suppressed(final_error)
                else:
                    wrapped = AutomationError(
                        "WORKFLOW.FINALLY_FAILED",
                        f"Finally for step {step.id!r} failed",
                        cause=final_error,
                    )
                    self.step_records[step.id].update(
                        status="failed",
                        error=wrapped.to_dict(),
                        duration_ms=round(
                            (time.monotonic() - started) * 1000, 3
                        ),
                    )
                    raise wrapped from final_error
            if returned:
                self.step_records[step.id].update(
                    status="succeeded",
                    duration_ms=round((time.monotonic() - started) * 1000, 3),
                )
        elapsed = time.monotonic() - started
        self.step_records[step.id].update(status="succeeded", output=result, duration_ms=round(elapsed * 1000, 3)); self._event("step.succeeded", step_id=step.id)
        return result

    def _attempt_step(
        self,
        step: CompiledStep,
        local_deadline: float | None,
        cleanup: bool = False,
        *,
        action_contract: Mapping[str, Any] | None = None,
    ) -> Any:
        retry = thaw(step.params.get("retry", self.descriptor.defaults.get("retry", {"max_attempts": 1})))
        max_attempts = int(retry.get("max_attempts", 1))
        effect = (
            thaw(step.params.get("effect", {})).get("class", "contextual")
            if step.type == "action"
            else "idempotent"
        )
        for attempt in range(1, max_attempts + 1):
            attempt_started = time.monotonic()
            if not cleanup:
                self._check_budget()
            if self._pre_reserved_attempts:
                self._pre_reserved_attempts -= 1
            else:
                self._attempt_budget.reserve(
                    None
                    if cleanup
                    else self.descriptor.budgets.get("max_executed_steps")
                )
            self._executed = self._attempt_budget.executed
            self.step_records[step.id]["attempts"] = attempt; remaining = self._remaining(local_deadline)
            if remaining == 0: raise AutomationError("STEP.TIMEOUT", f"Step {step.id!r} timed out")
            attempt_timeout = _duration(step.params.get("attempt_timeout"), remaining)
            if remaining is not None: attempt_timeout = min(attempt_timeout, remaining) if attempt_timeout is not None else remaining
            attempt_deadline = (
                time.monotonic() + attempt_timeout
                if attempt_timeout is not None
                else None
            )
            try:
                if attempt_deadline is not None:
                    self._deadline_stack.append(attempt_deadline)
                try:
                    contract = action_contract
                    if step.type == "action":
                        if contract is None:
                            contract = self._resolve_action_contract(step)
                        effect = self._effective_action_effect(
                            step, contract=contract
                        )
                    effective_timeout = self._remaining(attempt_deadline)
                    if effective_timeout == 0:
                        raise AutomationError(
                            "STEP.TIMEOUT", f"Step {step.id!r} timed out"
                        )
                    if step.type == "action":
                        assert contract is not None
                        manifest_timeout = _duration(contract.get("timeout"))
                        if manifest_timeout is not None:
                            manifest_remaining = max(
                                0.0,
                                attempt_started
                                + manifest_timeout
                                - time.monotonic(),
                            )
                            if manifest_remaining == 0:
                                raise AutomationError(
                                    "ACTION.TIMEOUT",
                                    "Action deadline expired before dispatch",
                                    category="action",
                                    retryable=True,
                                    effect="not_applied",
                                )
                            effective_timeout = (
                                min(effective_timeout, manifest_remaining)
                                if effective_timeout is not None
                                else manifest_remaining
                            )
                    return self._execute(
                        step, effective_timeout, action_contract=contract
                    )
                finally:
                    if attempt_deadline is not None:
                        self._deadline_stack.pop()
            except _ReturnFlow: raise
            except AutomationError as error:
                error.at_step(step.id, step_path=step.path, attempt=attempt, workflow=self.descriptor.name)
                if (
                    error.code == "ACTION.UNKNOWN_EFFECT"
                    or error.effect == "unknown"
                ):
                    raise
                if attempt >= max_attempts or effect not in {"read_only", "idempotent"} or not self._retry_match(retry, error): raise
                delay = self._retry_delay(retry, attempt); remaining = self._remaining(local_deadline)
                if remaining is not None and delay >= remaining: raise AutomationError("STEP.TIMEOUT", f"Step {step.id!r} timed out during retry", cause=error) from error
                self._event("step.retrying", step_id=step.id, attempt=attempt, delay=delay)
                if delay:
                    self._sleep_interruptibly(
                        delay, local_deadline, cleanup=cleanup
                    )
        raise AssertionError("unreachable")

    def _resolve_action_contract(
        self, step: CompiledStep
    ) -> Mapping[str, Any]:
        uses = step.params["uses"]
        capability = uses.rsplit(".", 1)[0]
        plugin = self.plugins.get(capability)
        if plugin is None:
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"No plugin registered for {capability!r}",
                details={"uses": uses},
            )
        return self._action_contract(plugin, capability, uses)

    def durable_action_binding(
        self, step: CompiledStep
    ) -> DurableActionBinding:
        """Bind a durable action to a validated canonical manifest."""

        if self.durable_action_mode != "read_only" or step.type != "action":
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable actions require explicit read_only mode",
                category="durable",
            )
        if "postcondition" in step.params:
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable read-only actions do not support postconditions",
                category="durable",
                details={"stepId": step.id},
            )
        uses = step.params["uses"]
        capability = uses.rsplit(".", 1)[0]
        plugin = self.plugins.get(capability)
        if plugin is None:
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"No plugin registered for {capability!r}",
                category="capability",
                details={"uses": uses},
            )
        manifest = self._plugin_manifest(plugin)
        if not isinstance(manifest, Mapping):
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable actions require a canonical provider manifest",
                category="durable",
                details={"uses": uses},
            )
        try:
            plugin._validate_manifest(dict(manifest))
        except PluginError as exc:
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action provider manifest is invalid",
                category="durable",
                details={"uses": uses},
            ) from exc
        contract = self._action_contract(plugin, capability, uses)
        if self._effective_action_effect(step, contract=contract) != "read_only":
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action must be effectively read-only",
                category="durable",
                details={"uses": uses},
            )
        errors = contract.get("errors")
        if (
            not isinstance(errors, (list, tuple))
            or not errors
            or any(
                not isinstance(item, Mapping)
                or item.get("effect") != "not_applied"
                for item in errors
            )
        ):
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action errors must be non-empty and not_applied",
                category="durable",
                details={"uses": uses},
            )
        provider_sensitivity = contract.get("sensitivity")
        descriptor_sensitivity = thaw(step.params.get("sensitivity", {}))
        sensitivity_fields = ("input", "output", "error")
        if (
            not isinstance(provider_sensitivity, Mapping)
            or not isinstance(descriptor_sensitivity, Mapping)
            or any(
                provider_sensitivity.get(name) != "public"
                or descriptor_sensitivity.get(name) != "public"
                for name in sensitivity_fields
            )
        ):
            raise AutomationError(
                "DURABLE.SENSITIVE_ACTION",
                "durable action input, output, and error must be explicitly public",
                category="durable",
                details={"uses": uses},
            )
        durability = contract.get("durability")
        provider_fields = (
            durability.get("checkpoint_fields")
            if isinstance(durability, Mapping)
            else None
        )
        if not isinstance(provider_fields, Mapping):
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action provider has no checkpoint field whitelist",
                category="durable",
                details={"uses": uses},
            )
        seen_pointers: set[str] = set()
        for alias, definition in provider_fields.items():
            if not isinstance(alias, str) or not isinstance(definition, Mapping):
                raise AutomationError(
                    "DURABLE.UNSUPPORTED_PLAN",
                    "durable checkpoint field is invalid",
                    category="durable",
                    details={"uses": uses, "field": alias},
                )
            pointer = definition.get("pointer")
            if (
                not _valid_json_pointer(pointer)
                or len(pointer) > 1024
                or len(pointer.split("/")) - 1 > 64
                or pointer in seen_pointers
            ):
                raise AutomationError(
                    "DURABLE.UNSUPPORTED_PLAN",
                    "durable checkpoint pointers must be unique, non-root, and bounded",
                    category="durable",
                    details={"uses": uses, "field": alias},
                )
            seen_pointers.add(pointer)
            if definition.get("missing", "error") == "null":
                try:
                    self._validate_schema(
                        None,
                        definition.get("schema"),
                        "DURABLE.UNSUPPORTED_PLAN",
                        str(alias),
                        redact=True,
                    )
                except AutomationError as exc:
                    raise AutomationError(
                        "DURABLE.UNSUPPORTED_PLAN",
                        "null checkpoint fallback must satisfy its schema",
                        category="durable",
                        details={"uses": uses, "field": alias},
                    ) from exc
        checkpoint = thaw(step.params.get("checkpoint", {}))
        output = checkpoint.get("output")
        if not isinstance(output, Mapping):
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action checkpoint output must be explicit",
                category="durable",
                details={"uses": uses},
            )
        mode = output.get("mode")
        fields = output.get("fields", ())
        if mode == "omit":
            selected: list[str] = []
        elif (
            mode == "project"
            and isinstance(fields, (list, tuple))
            and bool(fields)
            and all(isinstance(item, str) for item in fields)
            and set(fields).issubset(provider_fields)
        ):
            selected = list(fields)
        else:
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action projection is not provider-approved",
                category="durable",
                details={"uses": uses},
            )
        projection = {
            "mode": mode,
            "fields": selected,
            "definitions": {
                alias: _clone_runtime_value(provider_fields[alias])
                for alias in selected
            },
        }
        return DurableActionBinding(
            freeze(_clone_runtime_value(contract)),
            _canonical_digest(manifest),
            _canonical_digest(contract),
            _canonical_digest(projection),
        )

    def durable_action_binding_digest(
        self,
        step: CompiledStep,
        binding: DurableActionBinding | Mapping[str, Any],
    ) -> str:
        """Hash evaluated invocation input without returning or storing it."""

        normalized = self._normalize_durable_binding(binding)
        uses = step.params["uses"]
        capability = uses.rsplit(".", 1)[0]
        plugin = self.plugins.get(capability)
        if plugin is None:
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"No plugin registered for {capability!r}",
                category="capability",
                details={"uses": uses},
            )
        self._enforce_action_policy(step, plugin, normalized.contract)
        action_input = self._evaluate(thaw(step.params["with"]))
        self._validate_schema(
            action_input,
            normalized.contract.get("input_schema"),
            "ACTION.INPUT_INVALID",
            uses,
            redact=True,
        )
        return _canonical_digest(
            {
                "uses": uses,
                "input": action_input,
                "providerDigest": normalized.provider_digest,
                "contractDigest": normalized.contract_digest,
            }
        )

    def durable_action_deadlines(
        self,
        step: CompiledStep,
        binding: DurableActionBinding | Mapping[str, Any],
    ) -> dict[str, int | None]:
        """Freeze absolute workflow/step/attempt/provider deadline bounds."""

        normalized = self._normalize_durable_binding(binding)
        now_ms = int(time.time() * 1_000)

        def deadline(value: Any) -> int | None:
            seconds = _duration(value)
            return None if seconds is None else now_ms + int(seconds * 1_000)

        workflow_deadline = self._segment_deadline_epoch_ms
        step_deadline = deadline(
            step.params.get("timeout", self.descriptor.defaults.get("timeout"))
        )
        bounds = [
            item for item in (workflow_deadline, step_deadline)
            if item is not None
        ]
        attempt_deadline = deadline(step.params.get("attempt_timeout"))
        if bounds:
            attempt_deadline = (
                min(*bounds, attempt_deadline)
                if attempt_deadline is not None
                else min(bounds)
            )
        provider_deadline = deadline(normalized.contract.get("timeout"))
        if attempt_deadline is not None:
            provider_deadline = (
                min(provider_deadline, attempt_deadline)
                if provider_deadline is not None
                else attempt_deadline
            )
        return {
            "workflowDeadlineEpochMs": workflow_deadline,
            "stepDeadlineEpochMs": step_deadline,
            "attemptDeadlineEpochMs": attempt_deadline,
            "providerDeadlineEpochMs": provider_deadline,
            "dispatchDeadlineEpochMs": provider_deadline,
        }

    def _effective_action_effect(
        self, step: CompiledStep, *, contract: Mapping[str, Any] | None = None
    ) -> str:
        declared = thaw(step.params.get("effect", {})).get("class")
        if contract is None:
            contract = self._resolve_action_contract(step)
        provider = thaw(contract.get("effect", {})).get("default_class")
        return _max_effect(provider, declared)

    def _retry_match(self, retry: Mapping[str, Any], error: AutomationError) -> bool:
        if not error.retryable: return False
        on = retry.get("on")
        if not on: return True
        return any(fnmatch.fnmatchcase(error.code, pattern) for pattern in on.get("codes", ())) or error.category in on.get("categories", ())

    def _retry_delay(self, retry: Mapping[str, Any], attempt: int) -> float:
        backoff = retry.get("backoff") or {}; initial = _duration(backoff.get("initial_delay"), 0.0) or 0.0
        delay = initial if backoff.get("strategy", "fixed") == "fixed" else initial * float(backoff.get("multiplier", 2.0)) ** (attempt - 1)
        maximum = _duration(backoff.get("max_delay")); delay = min(delay, maximum) if maximum is not None else delay
        jitter = float(backoff.get("jitter", 0.0)); return delay * random.uniform(1 - jitter, 1 + jitter) if jitter else delay

    def _execute(
        self,
        step: CompiledStep,
        timeout: float | None,
        *,
        action_contract: Mapping[str, Any] | None = None,
    ) -> Any:
        if step.type == "action":
            return self._action(step, timeout, contract=action_contract)
        if step.type == "set":
            snapshot_context = dict(self.context)
            snapshot_context["vars"] = dict(self.variables)
            pending_values = {
                target.partition(".")[2]: self._evaluate(thaw(raw), snapshot_context)
                for target, raw in step.params["assign"].items()
            }
            for name, value in pending_values.items():
                definition = self.descriptor.variables[name]
                self._validate_schema(value, definition.get("schema"), "VARIABLE.INVALID", name)
            self.variables.update(pending_values)
            return dict(self.variables)
        if step.type == "if": self._run_steps(step.then_steps if bool(self._evaluate(thaw(step.params["condition"]))) else step.else_steps); return None
        if step.type == "switch":
            for case in step.cases:
                if bool(self._evaluate(thaw(case.when))): self._run_steps(case.steps); return None
            self._run_steps(step.default_steps); return None
        if step.type == "foreach":
            if int(step.params.get("concurrency", 1)) != 1:
                raise AutomationError(
                    "DESCRIPTOR.UNSUPPORTED_FEATURE",
                    "Concurrent foreach execution is not supported by this runtime",
                    category="descriptor",
                    details={"concurrency": step.params["concurrency"]},
                )
            items = self._evaluate(thaw(step.params["items"])); limit = int(step.params["max_items"])
            if isinstance(items, (str, bytes, Mapping)) or not isinstance(items, Sequence): raise AutomationError("EXPR.TYPE_MISMATCH", "foreach items must be an array")
            if len(items) > limit: raise AutomationError("LOOP.LIMIT_EXCEEDED", "foreach input exceeds max_items", details={"max_items": limit, "actual": len(items)})
            item_name, index_name = step.params["as"], step.params.get("index_as", "index"); old_item, old_index = self.context.get(item_name, MISSING), self.context.get(index_name, MISSING)
            try:
                for index, item in enumerate(items): self.context[item_name] = item; self.context[index_name] = index; self._run_steps(step.steps)
            finally: self._restore(item_name, old_item); self._restore(index_name, old_index)
            return None
        if step.type == "while":
            limit = int(step.params["max_iterations"])
            for _ in range(limit):
                if not bool(self._evaluate(thaw(step.params["condition"]))): return None
                self._run_steps(step.steps)
            if bool(self._evaluate(thaw(step.params["condition"]))): raise AutomationError("LOOP.LIMIT_EXCEEDED", "while condition remains true", details={"max_iterations": limit})
            return None
        if step.type == "block": self._run_steps(step.steps); return None
        if step.type == "script": return self._script(step, timeout)
        if step.type == "fail":
            raw = self._evaluate(thaw(step.params["error"])); raise AutomationError(raw["code"], raw["message"], category=raw.get("category"), retryable=raw.get("retryable", False), effect=raw.get("effect", "none"), details=raw.get("details"))
        if step.type == "return": raise _ReturnFlow(self._evaluate(thaw(step.params.get("value"))))
        raise AutomationError("DESCRIPTOR.UNSUPPORTED_FEATURE", f"Unsupported step type {step.type!r}")

    def _restore(self, name: str, old: Any) -> None:
        if old is MISSING: self.context.pop(name, None)
        else: self.context[name] = old

    def _action(
        self,
        step: CompiledStep,
        timeout: float | None,
        *,
        contract: Mapping[str, Any] | None = None,
    ) -> Any:
        action_deadline = (
            time.monotonic() + timeout if timeout is not None else None
        )
        uses = step.params["uses"]; capability = uses.rsplit(".", 1)[0]; plugin = self.plugins.get(capability)
        if plugin is None: raise AutomationError("CAPABILITY.MISSING", f"No plugin registered for {capability!r}", details={"uses": uses})
        if contract is None:
            contract = self._action_contract(plugin, capability, uses)
        self._enforce_action_policy(step, plugin, contract)
        effective_effect = self._effective_action_effect(
            step, contract=contract
        )
        pre = step.params.get("precondition")
        if pre and not bool(self._evaluate(thaw(pre["condition"]))): raise AutomationError("ACTION.PRECONDITION_FAILED", pre.get("message", "Action precondition failed"), phase="precondition")
        action_input = self._evaluate(thaw(step.params["with"]))
        self._validate_schema(
            action_input,
            contract.get("input_schema"),
            "ACTION.INPUT_INVALID",
            uses,
            redact=self.durable_action_mode == "read_only",
        )
        post = step.params.get("postcondition")
        observation_preflight: tuple[
            ProcessPlugin, str, Mapping[str, Any]
        ] | None = None
        if post is not None and post.get("observe") is not None:
            observation_preflight = self._preflight_observation(step, post)
        invoke_timeout = self._remaining(action_deadline)
        if invoke_timeout == 0:
            raise AutomationError(
                "ACTION.TIMEOUT",
                "Action deadline expired before dispatch",
                category="action",
                retryable=True,
                effect="not_applied",
            )
        result = self._invoke_contract_action(
            plugin,
            uses,
            action_input,
            invoke_timeout,
            contract=contract,
            effective_effect=effective_effect,
        )
        if self.durable_action_mode == "read_only":
            return self._project_durable_action_output(step, contract, result)
        self.context["steps"][step.id] = {"status": "running", "output": result}
        if post is not None:
            if post.get("observe") is None:
                self._postcondition(post)
            else:
                try:
                    self._postcondition(
                        post, prepared_observation=observation_preflight
                    )
                except AutomationError as error:
                    if effective_effect == "read_only":
                        raise
                    raise AutomationError(
                        "ACTION.UNKNOWN_EFFECT",
                        "Action outcome could not be verified after dispatch",
                        category="action",
                        phase="postcondition",
                        retryable=False,
                        effect="unknown",
                        details={
                            "cause": error.to_dict(),
                            "last_observation": error.details.get(
                                "last_observation"
                            ),
                        },
                        cause=error,
                    ) from error
        return result

    def _project_durable_action_output(
        self, step: CompiledStep, contract: Mapping[str, Any], result: Any
    ) -> Any:
        checkpoint = thaw(step.params.get("checkpoint", {}))
        output = checkpoint.get("output", {})
        if output.get("mode") == "omit":
            return None
        fields = output.get("fields")
        provider_fields = thaw(
            contract.get("durability", {}).get("checkpoint_fields", {})
        )
        if output.get("mode") != "project" or not isinstance(fields, list):
            raise AutomationError(
                "DURABLE.UNSUPPORTED_PLAN",
                "durable action output projection is not explicit",
                category="durable",
            )
        projected: dict[str, Any] = {}
        for alias in fields:
            definition = provider_fields.get(alias)
            if not isinstance(definition, Mapping):
                raise AutomationError(
                    "DURABLE.BINDING_MISMATCH",
                    "durable action projection is not provider-approved",
                    category="durable",
                    details={"stepId": step.id, "field": alias},
                )
            found, value = _resolve_json_pointer(result, definition["pointer"])
            if not found:
                missing = definition.get("missing", "error")
                if missing == "omit":
                    continue
                if missing == "null":
                    value = None
                else:
                    raise AutomationError(
                        "ACTION.OUTPUT_INVALID",
                        "durable checkpoint field is missing",
                        category="action",
                        details={"stepId": step.id, "field": alias},
                    )
            self._validate_schema(
                value,
                definition.get("schema"),
                "ACTION.OUTPUT_INVALID",
                f"{step.id}.{alias}",
                redact=True,
            )
            projected[alias] = _clone_runtime_value(value)
        return projected

    def _invoke_contract_action(
        self,
        plugin: ProcessPlugin,
        uses: str,
        action_input: Any,
        timeout: float | None,
        *,
        contract: Mapping[str, Any],
        effective_effect: str,
    ) -> Any:
        try:
            result = plugin.invoke(uses, action_input, timeout=timeout)
        except PluginError as exc:
            if self.durable_action_mode == "read_only":
                declared = any(
                    isinstance(item, Mapping) and item.get("code") == exc.code
                    for item in contract.get("errors", ())
                )
                if declared:
                    raise AutomationError(
                        exc.code,
                        "Durable action failed with a declared provider error",
                        category="action",
                        phase="execute",
                        retryable=False,
                        effect="not_applied",
                    ) from None
                if exc.code == "PLUGIN.HOST_TIMEOUT":
                    raise AutomationError(
                        "ACTION.TIMEOUT",
                        "Durable read-only action timed out",
                        category="action",
                        phase="execute",
                        retryable=False,
                        effect="not_applied",
                    ) from None
                raise AutomationError(
                    "ACTION.UNDECLARED_ERROR",
                    "Durable action returned an undeclared error",
                    category="action",
                    phase="execute",
                    retryable=False,
                    effect="unknown",
                ) from None
            ambiguous = exc.code in {"PLUGIN.HOST_TIMEOUT", "PLUGIN.HOST_EOF", "PLUGIN.HOST_PROTOCOL_ERROR"}
            if exc.dispatched and effective_effect in {"non_idempotent", "contextual"} and ambiguous:
                raise AutomationError("ACTION.UNKNOWN_EFFECT", "Action outcome is unknown after dispatch", category="action", effect="unknown", details={"plugin_error": exc.to_dict()}, cause=exc) from exc
            if exc.code == "PLUGIN.HOST_TIMEOUT":
                raise AutomationError(
                    "ACTION.TIMEOUT",
                    "Action did not complete before its deadline",
                    category="action",
                    retryable=exc.retryable,
                    effect=(
                        "not_applied"
                        if not exc.dispatched or effective_effect in {"read_only", "idempotent"}
                        else "unknown"
                    ),
                    details={"plugin_error": exc.to_dict()},
                    cause=exc,
                ) from exc
            declared_error = next(
                (
                    item
                    for item in contract.get("errors", ())
                    if isinstance(item, Mapping) and item.get("code") == exc.code
                ),
                None,
            )
            error_effect = (
                declared_error.get("effect", "none")
                if isinstance(declared_error, Mapping)
                else "none"
            )
            raise AutomationError(
                exc.code,
                exc.message,
                category="plugin",
                retryable=exc.retryable,
                effect=error_effect,
                details=exc.details,
                cause=exc,
            ) from exc
        self._validate_schema(
            result,
            contract.get("output_schema"),
            "ACTION.OUTPUT_INVALID",
            uses,
            redact=self.durable_action_mode == "read_only",
        )
        return result

    @staticmethod
    def _redact_durable_action_error(
        error: AutomationError, contract: Mapping[str, Any]
    ) -> AutomationError:
        declared_codes = {
            item.get("code")
            for item in contract.get("errors", ())
            if isinstance(item, Mapping)
            and item.get("effect") == "not_applied"
        }
        if error.code in declared_codes:
            return AutomationError(
                error.code,
                "Durable action failed with a declared provider error",
                category="action",
                phase=error.phase or "execute",
                retryable=False,
                effect="not_applied",
            )
        safe_codes = {
            "ACTION.INPUT_INVALID",
            "ACTION.OUTPUT_INVALID",
            "ACTION.PRECONDITION_FAILED",
            "ACTION.TIMEOUT",
            "STEP.TIMEOUT",
            "WORKFLOW.TIMEOUT",
            "WORKFLOW.CANCELLED",
            "POLICY.DENIED",
            "POLICY.CONFIRMATION_REQUIRED",
            "CAPABILITY.MISSING",
            "CAPABILITY.VERSION_INCOMPATIBLE",
            "CAPABILITY.PLATFORM_UNSUPPORTED",
            "DURABLE.BINDING_MISMATCH",
            "DURABLE.UNSUPPORTED_PLAN",
        }
        if error.code in safe_codes:
            return AutomationError(
                error.code,
                "Durable read-only action failed",
                category=error.category,
                phase=error.phase or "execute",
                retryable=False,
                effect=(
                    "none"
                    if error.code == "WORKFLOW.CANCELLED"
                    else "not_applied"
                ),
            )
        return AutomationError(
            "ACTION.UNDECLARED_ERROR",
            "Durable action returned an undeclared error",
            category="action",
            phase="execute",
            retryable=False,
            effect="unknown",
        )

    def _action_contract(
        self, plugin: ProcessPlugin, capability: str, uses: str
    ) -> Mapping[str, Any]:
        if plugin.manifest is None and type(plugin).invoke is not ProcessPlugin.invoke:
            return {}
        manifest = self._plugin_manifest(plugin)
        # Test doubles and trusted in-process adapters may deliberately omit a
        # manifest. Real process plugins are validated by ProcessPlugin.start.
        if not isinstance(manifest, Mapping):
            return {}
        metadata = manifest.get("metadata", {})
        if metadata.get("name") != capability:
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"Plugin {metadata.get('name')!r} does not provide {capability!r}",
                details={"uses": uses},
            )
        action_with_major = uses[len(capability) + 1 :]
        action_name, major_text = action_with_major.rsplit("@", 1)
        contract = manifest.get("actions", {}).get(action_name)
        if not isinstance(contract, Mapping):
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"Capability {capability!r} does not provide action {action_name!r}",
                details={"uses": uses},
            )
        if contract.get("contract_major") != int(major_text):
            raise AutomationError(
                "CAPABILITY.VERSION_INCOMPATIBLE",
                f"Action contract major does not match {uses!r}",
                details={"uses": uses, "available_major": contract.get("contract_major")},
            )
        return contract

    def _plugin_manifest(
        self,
        plugin: ProcessPlugin,
        *,
        timeout_code: str = "ACTION.TIMEOUT",
    ) -> Mapping[str, Any]:
        remaining = self._remaining()
        if remaining == 0:
            raise AutomationError(
                timeout_code,
                "Deadline exceeded during capability handshake",
                category=(
                    "action" if timeout_code == "ACTION.TIMEOUT" else "workflow"
                ),
                phase="execute",
                effect=(
                    "not_applied" if timeout_code == "ACTION.TIMEOUT" else "none"
                ),
            )
        try:
            return plugin.start(timeout=remaining)
        except PluginError as exc:
            if exc.code == "PLUGIN.HOST_TIMEOUT" and self._remaining() == 0:
                raise AutomationError(
                    timeout_code,
                    "Deadline exceeded during capability handshake",
                    category=(
                        "action"
                        if timeout_code == "ACTION.TIMEOUT"
                        else "workflow"
                    ),
                    phase="execute",
                    retryable=exc.retryable,
                    effect=(
                        "not_applied"
                        if timeout_code == "ACTION.TIMEOUT"
                        else "none"
                    ),
                    cause=exc,
                ) from exc
            raise AutomationError(
                exc.code,
                exc.message,
                category="plugin",
                retryable=exc.retryable,
                details=exc.details,
                cause=exc,
            ) from exc

    def _enforce_action_policy(
        self,
        step: CompiledStep,
        plugin: ProcessPlugin,
        contract: Mapping[str, Any],
    ) -> None:
        self._enforce_contract_policy(
            plugin,
            contract,
            declared_risk=thaw(step.params.get("risk", {})),
        )

    def _enforce_contract_policy(
        self,
        plugin: ProcessPlugin,
        contract: Mapping[str, Any],
        *,
        declared_risk: Mapping[str, Any] | None = None,
    ) -> None:
        declared = declared_risk or {}
        default = thaw(contract.get("risk", {}))
        risks = [risk for risk in (default, declared) if risk]
        manifest = plugin.manifest if isinstance(plugin.manifest, Mapping) else {}
        runtime = manifest.get("runtime", {}) if isinstance(manifest, Mapping) else {}
        supported_platforms = runtime.get("platforms") if isinstance(runtime, Mapping) else None
        if supported_platforms and _platform_name() not in supported_platforms:
            raise AutomationError(
                "CAPABILITY.PLATFORM_UNSUPPORTED",
                f"Capability is unavailable on platform {_platform_name()!r}",
                category="capability",
            )
        required_permissions = set(manifest.get("permissions", ()))
        required_permissions.update(contract.get("permissions", ()))
        declared_permissions = set(
            thaw(self.descriptor.requires).get("permissions", ())
        )
        undeclared_permissions = sorted(
            required_permissions - declared_permissions
        )
        if undeclared_permissions:
            raise AutomationError(
                "POLICY.DENIED",
                "Action permissions were not declared by the workflow",
                category="policy",
                details={"undeclared_permissions": undeclared_permissions},
            )
        missing_permissions = sorted(required_permissions - self.granted_permissions)
        if missing_permissions:
            raise AutomationError(
                "POLICY.DENIED",
                "Action permissions were not granted",
                category="policy",
                details={"missing_permissions": missing_permissions},
            )
        if not risks:
            return
        order = {"low": 0, "medium": 1, "high": 2, "critical": 3, "contextual": 4}
        policy = thaw(self.descriptor.policy)
        allowed = policy.get("allowed_risk", {})
        if allowed:
            categories = allowed.get("categories")
            denied_categories = [
                risk.get("category")
                for risk in risks
                if categories and risk.get("category") not in categories
            ]
            if denied_categories:
                raise AutomationError(
                    "POLICY.DENIED",
                    f"Risk category {denied_categories[0]!r} is not allowed",
                    category="policy",
                )
            maximum = allowed.get("max_level")
            highest = max(
                risks, key=lambda risk: order.get(risk.get("level"), 99)
            )
            if (
                maximum in order
                and highest.get("level") in order
                and order[highest["level"]] > order[maximum]
            ):
                raise AutomationError(
                    "POLICY.DENIED",
                    f"Risk level {highest['level']!r} exceeds {maximum!r}",
                    category="policy",
                )
        confirmation = policy.get("confirmation", {})
        required_for = confirmation.get("required_for", {}) if isinstance(confirmation, Mapping) else {}
        categories = set(required_for.get("categories", ()))
        minimum = required_for.get("min_level")
        requires_confirmation = any(
            (categories and risk.get("category") in categories)
            or (minimum in order and risk.get("level") in order and order[risk["level"]] >= order[minimum])
            for risk in risks
        )
        if requires_confirmation:
            raise AutomationError(
                "POLICY.CONFIRMATION_REQUIRED",
                "This action requires a bound confirmation token, which v0 cannot verify",
                category="policy",
            )

    def _preflight_observation(
        self, step: CompiledStep, post: Mapping[str, Any]
    ) -> tuple[ProcessPlugin, str, Mapping[str, Any]]:
        """Validate an observer before the primary action can be dispatched."""
        observe = thaw(post["observe"])
        timeout = _duration(post.get("timeout"))
        deadline = time.monotonic() + timeout if timeout is not None else None
        if deadline is not None:
            self._deadline_stack.append(deadline)
        try:
            prepared = self._prepare_observation(observe)
            if not _references_current_step(observe["with"], step.id):
                _, uses, contract = prepared
                action_input = self._evaluate(observe["with"])
                self._validate_schema(
                    action_input,
                    contract.get("input_schema"),
                    "ACTION.INPUT_INVALID",
                    uses,
                )
            return prepared
        finally:
            if deadline is not None:
                self._deadline_stack.pop()

    def _postcondition(
        self,
        post: Mapping[str, Any],
        *,
        prepared_observation: tuple[
            ProcessPlugin, str, Mapping[str, Any]
        ] | None = None,
    ) -> None:
        timeout = _duration(post.get("timeout"))
        interval = _duration(post.get("poll_interval"), .1) or .1
        started = time.monotonic()
        deadline = started + timeout if timeout is not None else None
        observe = post.get("observe")
        last_observation: Any = MISSING
        if deadline is not None:
            self._deadline_stack.append(deadline)
        try:
            if observe is not None:
                if prepared_observation is None:
                    prepared_observation = self._prepare_observation(
                        thaw(observe)
                    )
            while True:
                if (
                    deadline is not None
                    and
                    last_observation is not MISSING
                    and time.monotonic() >= deadline
                ):
                    raise AutomationError(
                        "ACTION.POSTCONDITION_FAILED",
                        post.get(
                            "message", "Action postcondition failed"
                        ),
                        phase="postcondition",
                        details={"last_observation": last_observation},
                    )
                evaluation_context: Mapping[str, Any] | None = None
                if observe is not None:
                    last_observation = self._observe_postcondition(
                        thaw(observe), prepared=prepared_observation
                    )
                    if self._remaining() == 0:
                        raise AutomationError(
                            "ACTION.TIMEOUT",
                            "Postcondition observation exceeded its deadline",
                            category="action",
                            phase="postcondition",
                            effect="not_applied",
                            details={"last_observation": last_observation},
                        )
                    temporary_context = dict(self.context)
                    temporary_context["observation"] = last_observation
                    evaluation_context = temporary_context
                if bool(
                    self._evaluate(
                        thaw(post["condition"]), evaluation_context
                    )
                ):
                    return
                if deadline is None:
                    details = (
                        {"last_observation": last_observation}
                        if observe is not None
                        else None
                    )
                    raise AutomationError(
                        "ACTION.POSTCONDITION_FAILED",
                        post.get(
                            "message", "Action postcondition failed"
                        ),
                        phase="postcondition",
                        details=details,
                    )
                remaining = self._remaining()
                if remaining == 0:
                    details = (
                        {"last_observation": last_observation}
                        if observe is not None
                        else None
                    )
                    raise AutomationError(
                        "ACTION.POSTCONDITION_FAILED",
                        post.get(
                            "message", "Action postcondition failed"
                        ),
                        phase="postcondition",
                        details=details,
                    )
                sleep_for = interval
                if remaining is not None:
                    sleep_for = min(sleep_for, remaining)
                if sleep_for:
                    self._sleep_interruptibly(sleep_for, deadline)
        except AutomationError as error:
            if observe is not None and "last_observation" not in error.details:
                error.details["last_observation"] = (
                    None
                    if last_observation is MISSING
                    else last_observation
                )
            raise
        finally:
            if deadline is not None:
                self._deadline_stack.pop()

    def _observe_postcondition(
        self,
        observe: Mapping[str, Any],
        *,
        prepared: tuple[ProcessPlugin, str, Mapping[str, Any]] | None = None,
    ) -> Any:
        round_started = time.monotonic()
        if self._remaining() == 0:
            raise AutomationError(
                "ACTION.TIMEOUT",
                "Postcondition deadline expired before observation",
                category="action",
                phase="postcondition",
                effect="not_applied",
            )
        if prepared is None:
            prepared = self._prepare_observation(observe)
        plugin, uses, contract = prepared
        action_input = self._evaluate(thaw(observe["with"]))
        self._validate_schema(
            action_input,
            contract.get("input_schema"),
            "ACTION.INPUT_INVALID",
            uses,
        )
        action_deadline: float | None = None
        manifest_timeout = _duration(contract.get("timeout"))
        if manifest_timeout is not None:
            action_deadline = round_started + manifest_timeout
        invoke_timeout = self._remaining(action_deadline)
        if invoke_timeout == 0:
            raise AutomationError(
                "ACTION.TIMEOUT",
                "Observation deadline expired before dispatch",
                category="action",
                phase="postcondition",
                retryable=True,
                effect="not_applied",
            )
        return self._invoke_contract_action(
            plugin,
            uses,
            action_input,
            invoke_timeout,
            contract=contract,
            effective_effect="read_only",
        )

    def _prepare_observation(
        self, observe: Mapping[str, Any]
    ) -> tuple[ProcessPlugin, str, Mapping[str, Any]]:
        uses = observe["uses"]
        capability = uses.rsplit(".", 1)[0]
        plugin = self.plugins.get(capability)
        if plugin is None:
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"No plugin registered for {capability!r}",
                details={"uses": uses},
            )
        contract = self._action_contract(plugin, capability, uses)
        provider_effect = thaw(contract.get("effect", {})).get(
            "default_class"
        )
        if provider_effect != "read_only":
            raise AutomationError(
                "POLICY.DENIED",
                "Postcondition observation action must be read-only",
                category="policy",
                phase="postcondition",
                details={"uses": uses, "effect": provider_effect},
            )
        unsafe_errors = [
            item.get("code")
            for item in contract.get("errors", ())
            if isinstance(item, Mapping)
            and item.get("effect") != "not_applied"
        ]
        if unsafe_errors:
            raise AutomationError(
                "POLICY.DENIED",
                "Postcondition observation errors must be not_applied",
                category="policy",
                phase="postcondition",
                details={"uses": uses, "unsafe_errors": unsafe_errors},
            )
        self._enforce_contract_policy(plugin, contract)
        return plugin, uses, contract

    def _script(self, step: CompiledStep, timeout: float | None) -> Any:
        if not self.allow_scripts: raise AutomationError("SCRIPT.SANDBOX_DENIED", "Scripts are disabled; pass --allow-scripts", category="script")
        result = execute_python_script(
            self.descriptor,
            step,
            self._evaluate(thaw(step.params.get("inputs", {}))),
            timeout,
        )
        self._validate_schema(
            result, step.params["output_schema"], "SCRIPT.OUTPUT_INVALID", step.id
        )
        return result

    def _apply_handler(self, handler: ErrorHandler | None, error: AutomationError) -> AutomationError | None:
        if handler is None or not self._handler_matches(handler, error): return error
        previous = self.context.get(handler.as_name, MISSING); self.context[handler.as_name] = error.to_dict(); self.context["error"] = error.to_dict()
        try:
            self._run_steps(handler.steps)
            if handler.mode == "rethrow": return error
            value = self._evaluate(thaw(handler.output)) if handler.output is not MISSING else None
            if handler.mode == "return": raise _ReturnFlow(value)
            self._handler_output = value; return None
        finally: self._restore(handler.as_name, previous)

    def _handler_matches(self, handler: ErrorHandler, error: AutomationError) -> bool:
        return (not handler.match_codes or any(fnmatch.fnmatchcase(error.code, item) for item in handler.match_codes)) and (not handler.match_categories or error.category in handler.match_categories) and (not handler.match_effects or error.effect in handler.match_effects)


def _duration(value: Any, default: float | None = None) -> float | None:
    return parse_duration(value, default)


def _values_equal(left: Any, right: Any) -> bool:
    if left is None or isinstance(left, (bool, int, float, str)):
        return type(left) is type(right) and left == right
    if isinstance(left, Mapping):
        return (
            isinstance(right, Mapping)
            and set(left) == set(right)
            and all(_values_equal(left[key], right[key]) for key in left)
        )
    if isinstance(left, (list, tuple)):
        return (
            isinstance(right, (list, tuple))
            and len(left) == len(right)
            and all(
                _values_equal(left_item, right_item)
                for left_item, right_item in zip(left, right)
            )
        )
    raise TypeError(f"unsupported runtime value {type(left).__name__!r}")


def _clone_runtime_value(value: Any) -> Any:
    if value is None or isinstance(value, (bool, int, str)):
        return value
    if isinstance(value, float):
        if not math.isfinite(value):
            raise TypeError("runtime numbers must be finite")
        return value
    if isinstance(value, Mapping):
        if not all(isinstance(key, str) for key in value):
            raise TypeError("runtime object keys must be strings")
        return {key: _clone_runtime_value(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_clone_runtime_value(item) for item in value]
    raise TypeError(f"unsupported runtime value {type(value).__name__!r}")


def _resolve_json_pointer(document: Any, pointer: str) -> tuple[bool, Any]:
    """Resolve a validated, non-root RFC 6901 JSON pointer."""

    if not _valid_json_pointer(pointer):
        return False, None
    current = document
    for raw_token in pointer.split("/")[1:]:
        token = raw_token.replace("~1", "/").replace("~0", "~")
        if isinstance(current, Mapping):
            if token not in current:
                return False, None
            current = current[token]
        elif isinstance(current, (list, tuple)):
            if (
                not token.isascii()
                or not token.isdigit()
                or (token != "0" and token.startswith("0"))
            ):
                return False, None
            index = int(token)
            if index >= len(current):
                return False, None
            current = current[index]
        else:
            return False, None
    return True, current


def _valid_json_pointer(value: Any) -> bool:
    return (
        isinstance(value, str)
        and value.startswith("/")
        and re.fullmatch(r"/(?:[^~/]|~[01])*(?:/(?:[^~/]|~[01])*)*", value)
        is not None
    )


def _canonical_digest(value: Any) -> str:
    payload = json.dumps(
        _clone_runtime_value(value),
        ensure_ascii=False,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return "sha256:" + hashlib.sha256(payload).hexdigest()


def _is_sha256_digest(value: Any) -> bool:
    return isinstance(value, str) and re.fullmatch(
        r"sha256:[0-9a-f]{64}", value
    ) is not None


def canonical_plan_digest(descriptor: WorkflowDescriptor) -> str:
    """Return a stable digest of the canonical compiled descriptor input."""

    payload = json.dumps(
        _clone_runtime_value(descriptor.raw),
        ensure_ascii=False,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return "sha256:" + hashlib.sha256(payload).hexdigest()


def _references_current_step(value: Any, step_id: str) -> bool:
    """Return whether a template may read the current step's live state."""
    if isinstance(value, Mapping):
        return any(
            _references_current_step(item, step_id)
            for item in value.values()
        )
    if isinstance(value, (list, tuple)):
        return any(_references_current_step(item, step_id) for item in value)
    if not isinstance(value, str):
        return False
    for match in _TEMPLATE.finditer(value):
        try:
            tree = ast.parse(match.group(1).strip(), mode="eval")
        except (SyntaxError, ValueError, RecursionError):
            # The compiler reports malformed expressions.  Conservatively
            # defer here so this helper never turns analysis into execution.
            return True
        parents = {
            child: parent
            for parent in ast.walk(tree)
            for child in ast.iter_child_nodes(parent)
        }
        for node in ast.walk(tree):
            parent = parents.get(node)
            if (
                isinstance(parent, (ast.Attribute, ast.Subscript))
                and parent.value is node
            ):
                # Only inspect the outermost access chain.  Otherwise every
                # ``steps.previous...`` also contains a nested bare ``steps``
                # Name and would be misclassified as current-step dependent.
                continue
            path = _expression_access_path(node)
            if not path or path[0] != "steps":
                continue
            if len(path) < 2 or path[1] is None or path[1] == step_id:
                return True
    return False


def _expression_access_path(node: ast.AST) -> list[str | None] | None:
    if isinstance(node, ast.Name):
        return [node.id]
    if isinstance(node, ast.Attribute):
        parent = _expression_access_path(node.value)
        return None if parent is None else [*parent, node.attr]
    if isinstance(node, ast.Subscript):
        parent = _expression_access_path(node.value)
        if parent is None:
            return None
        key = (
            node.slice.value
            if isinstance(node.slice, ast.Constant)
            and isinstance(node.slice.value, str)
            else None
        )
        return [*parent, key]
    return None


def _max_effect(provider: Any, declared: Any) -> str:
    order = {
        "read_only": 0,
        "idempotent": 1,
        "non_idempotent": 2,
        "contextual": 3,
    }
    values = [item for item in (provider, declared) if item in order]
    return max(values, key=lambda item: order[item]) if values else "contextual"


def run_descriptor(descriptor: WorkflowDescriptor, *, inputs: Mapping[str, Any] | None = None, plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None = None, allow_scripts: bool = False, granted_permissions: Sequence[str] | None = None, event_sink: Callable[[Mapping[str, Any]], None] | None = None) -> RunResult:
    return WorkflowRunner(descriptor, plugins=plugins, allow_scripts=allow_scripts, granted_permissions=granted_permissions, event_sink=event_sink).run(inputs)


def _platform_name() -> str:
    if sys.platform.startswith("win"):
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return "linux"


def _semver(
    value: str,
) -> tuple[int, int, int, tuple[int | str, ...] | None]:
    if not isinstance(value, str):
        raise ValueError(f"invalid semantic version: {value!r}")
    match = re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
        r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
        r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?",
        value,
    )
    if not match:
        raise ValueError(f"invalid semantic version: {value!r}")
    major, minor, patch, prerelease_text = match.groups()
    prerelease: tuple[int | str, ...] | None = None
    if prerelease_text is not None:
        identifiers: list[int | str] = []
        for identifier in prerelease_text.split("."):
            if identifier.isdigit():
                if len(identifier) > 1 and identifier.startswith("0"):
                    raise ValueError(
                        f"invalid semantic version: {value!r}"
                    )
                identifiers.append(int(identifier))
            else:
                identifiers.append(identifier)
        prerelease = tuple(identifiers)
    return int(major), int(minor), int(patch), prerelease


def _compare_semver(
    left: tuple[int, int, int, tuple[int | str, ...] | None],
    right: tuple[int, int, int, tuple[int | str, ...] | None],
) -> int:
    if left[:3] != right[:3]:
        return -1 if left[:3] < right[:3] else 1
    left_pre, right_pre = left[3], right[3]
    if left_pre is None or right_pre is None:
        if left_pre is right_pre:
            return 0
        return 1 if left_pre is None else -1
    for left_item, right_item in zip(left_pre, right_pre):
        if left_item == right_item:
            continue
        if isinstance(left_item, int) and isinstance(right_item, str):
            return -1
        if isinstance(left_item, str) and isinstance(right_item, int):
            return 1
        return -1 if left_item < right_item else 1
    if len(left_pre) == len(right_pre):
        return 0
    return -1 if len(left_pre) < len(right_pre) else 1


def _version_matches(version: str, constraint: str) -> bool:
    try:
        actual = _semver(version)
        tokens = constraint.split()
        comparators: list[
            tuple[
                str,
                tuple[int, int, int, tuple[int | str, ...] | None],
            ]
        ] = []
        for token in tokens:
            if token.startswith("^"):
                floor = _semver(token[1:])
                if floor[0] != 0:
                    ceiling = (floor[0] + 1, 0, 0, None)
                elif floor[1] != 0:
                    ceiling = (0, floor[1] + 1, 0, None)
                else:
                    ceiling = (0, 0, floor[2] + 1, None)
                comparators.extend(((">=", floor), ("<", ceiling)))
                continue
            match = re.fullmatch(r"(>=|<=|>|<|=)?(.+)", token)
            if not match:
                return False
            operator, wanted_text = match.groups()
            wanted = _semver(wanted_text)
            comparators.append((operator or "=", wanted))
        if not comparators:
            return False
        # SemVer ranges do not implicitly opt into prerelease providers.  A
        # comparator must name a prerelease on the same major/minor/patch.
        if actual[3] is not None and not any(
            wanted[3] is not None and wanted[:3] == actual[:3]
            for _, wanted in comparators
        ):
            return False
        for operator, wanted in comparators:
            comparison = _compare_semver(actual, wanted)
            if operator == ">=" and comparison < 0:
                return False
            if operator == "<=" and comparison > 0:
                return False
            if operator == ">" and comparison <= 0:
                return False
            if operator == "<" and comparison >= 0:
                return False
            if operator == "=" and comparison != 0:
                return False
        return True
    except (TypeError, ValueError):
        return False


__all__ = [
    "DurableActionBinding",
    "RUNTIME_STATE_SCHEMA_VERSION",
    "RUNTIME_VERSION",
    "RuntimeSegmentState",
    "SegmentResult",
    "WorkflowRunner",
    "canonical_plan_digest",
    "run_descriptor",
    "RunResult",
]
