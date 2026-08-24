"""Persistent run lifecycle orchestration on top of :mod:`journal`.

The journal owns storage invariants.  This module owns the public control
semantics that sit above them:

* pause, resume, and cancel requests change only ``desired_state``;
* a request never claims that the runner has observed it;
* only a live, fenced owner may apply intent at a runner safe point; and
* an unconfirmed post-dispatch effect is terminal ``unknown_effect``, never
  the more reassuring but incorrect ``cancelled``.

Repeated control requests are idempotent while a run is non-terminal.  Once a
run is terminal every control request fails closed, including a repeated
cancel request, because the durable row is immutable.
"""

from __future__ import annotations

from enum import StrEnum
import sqlite3
from typing import Any, Callable, NoReturn, TypeVar

from .errors import AutomationError
from .journal import (
    DesiredState,
    EventRecord,
    InvalidStateTransitionError,
    JournalConflictError,
    JournalError,
    JournalStore,
    LeaseConflictError,
    LeaseLostError,
    RunNotFoundError,
    RunRecord,
    RunStatus,
    SensitiveDataError,
    durable_descriptor_eligible,
)


MAX_CONTROL_CAS_ATTEMPTS = 16
MAX_SAFE_POINT_RECONCILIATIONS = 16


class DispatchState(StrEnum):
    """What is known about the most recent dispatch at a safe point."""

    BEFORE_DISPATCH = "before_dispatch"
    EFFECT_CONFIRMED = "effect_confirmed"
    EFFECT_UNKNOWN = "effect_unknown"


class RunServiceError(AutomationError):
    """Stable, machine-readable failure from :class:`RunService`."""


_T = TypeVar("_T")


class RunService:
    """Query and control durable workflow runs.

    The service deliberately does not acquire owner leases.  A scheduler may
    use its own claimant policy, then pass the resulting owner ID and bearer
    token to :meth:`runner_safe_point`.  The token is passed straight through
    to the journal and is never included in events or service errors.

    This is also the trusted persistence entry point.  It derives the
    journal's ``sensitive`` flag from the descriptor's declared input/output
    definitions and rejects unknown definition shapes.  That is declaration
    enforcement, not content inspection or complete runtime taint tracking.
    """

    def __init__(self, journal: JournalStore) -> None:
        if not isinstance(journal, JournalStore):
            raise TypeError("journal must be a JournalStore")
        self._journal = journal

    def create(
        self,
        *,
        workflow_name: str,
        inputs: Any,
        descriptor: object,
        run_id: str | None = None,
        workflow_version: str | None = None,
        plan_digest: str | None = None,
    ) -> RunRecord:
        """Create a pending run and its ``run.created`` event atomically."""

        workflow: dict[str, str] = {"name": workflow_name}
        if workflow_version is not None:
            workflow["version"] = workflow_version
        if plan_digest is not None:
            workflow["planDigest"] = plan_digest
        sensitive = not durable_descriptor_eligible(descriptor)
        run, _ = self._call(
            "create",
            run_id,
            lambda: self._journal.create_run_with_event(
                run_id=run_id,
                workflow_name=workflow_name,
                workflow_version=workflow_version,
                plan_digest=plan_digest,
                inputs=inputs,
                descriptor=descriptor,
                event_type="run.created",
                event_payload={
                    "workflow": workflow,
                    "status": RunStatus.PENDING.value,
                    "desiredState": DesiredState.RUN.value,
                },
                sensitive=sensitive,
            ),
        )
        return run

    def get(self, run_id: str) -> RunRecord:
        """Return the current durable snapshot of one run."""

        return self._call("get", run_id, lambda: self._journal.get_run(run_id))

    def list(
        self,
        *,
        status: RunStatus | str | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> list[RunRecord]:
        """List run snapshots in journal order."""

        return self._call(
            "list",
            None,
            lambda: self._journal.list_runs(
                status=status, limit=limit, offset=offset
            ),
        )

    def events(
        self,
        run_id: str,
        *,
        after_seq: int = 0,
        limit: int = 1_000,
    ) -> list[EventRecord]:
        """List immutable run events after ``after_seq``."""

        return self._call(
            "events",
            run_id,
            lambda: self._journal.list_events(
                run_id, after_seq=after_seq, limit=limit
            ),
        )

    def request_pause(self, run_id: str) -> RunRecord:
        """Request a pause without claiming it has taken effect.

        Calling this again while ``desired_state`` is already ``pause`` is a
        read-only success.  A sticky cancel request cannot be overwritten.
        Operator intent is independent of the runner's bearer lease.
        """

        return self._request_desired_state(
            run_id,
            DesiredState.PAUSE,
            operation="request_pause",
            event_type="run.pause_requested",
        )

    def request_resume(self, run_id: str) -> RunRecord:
        """Request execution, leaving a currently paused status unchanged.

        The runner changes ``paused`` to ``running`` only after claiming a new
        owner lease and presenting it at a safe point.
        """

        return self._request_desired_state(
            run_id,
            DesiredState.RUN,
            operation="request_resume",
            event_type="run.resume_requested",
        )

    def request_cancel(self, run_id: str) -> RunRecord:
        """Set sticky cancellation intent without fabricating completion.

        Repetition is a read-only success until a runner applies cancellation.
        After that, terminal immutability takes precedence and the request is
        rejected with ``RUN.TERMINAL``.
        """

        return self._request_desired_state(
            run_id,
            DesiredState.CANCEL,
            operation="request_cancel",
            event_type="run.cancel_requested",
        )

    def runner_safe_point(
        self,
        run_id: str,
        *,
        owner_id: str,
        token: str,
        now: float | None = None,
        dispatch_state: DispatchState | str = DispatchState.BEFORE_DISPATCH,
    ) -> RunRecord:
        """Apply current control intent using a live owner lease.

        ``running <-> paused`` and terminal transitions are committed together
        with their event through the journal's combination transaction.  Even
        an otherwise no-op safe point appends a fenced ``run.safe_point``
        event; this prevents a stale owner from being told it may continue.

        ``dispatch_state=effect_unknown`` is fail-closed.  It records
        ``unknown_effect`` regardless of control intent, and in particular
        prevents a concurrent cancel request from being reported as a clean
        cancellation after an ambiguous dispatch.
        """

        state = self._dispatch_state(dispatch_state, run_id=run_id)
        for _ in range(MAX_SAFE_POINT_RECONCILIATIONS):
            run = self.get(run_id)
            self._require_nonterminal(run, operation="runner_safe_point")
            if run.status not in {RunStatus.RUNNING, RunStatus.PAUSED}:
                raise self._error(
                    "RUN.NOT_AT_SAFE_POINT",
                    f"run {run_id} is {run.status.value}, not running or paused",
                    operation="runner_safe_point",
                    run=run,
                )

            if state is DispatchState.EFFECT_UNKNOWN:
                terminal = self._terminal_at_safe_point(
                    run,
                    owner_id=owner_id,
                    token=token,
                    now=now,
                    status=RunStatus.UNKNOWN_EFFECT,
                    event_type="run.effect_unknown",
                    dispatch_state=state,
                )
                if terminal is not None:
                    return terminal
                continue

            if run.desired_state is DesiredState.CANCEL:
                terminal = self._terminal_at_safe_point(
                    run,
                    owner_id=owner_id,
                    token=token,
                    now=now,
                    status=RunStatus.CANCELLED,
                    event_type="run.cancelled",
                    dispatch_state=state,
                )
                if terminal is not None:
                    return terminal
                continue

            target = (
                RunStatus.PAUSED
                if run.desired_state is DesiredState.PAUSE
                else RunStatus.RUNNING
            )
            if run.status is target:
                current = self._append_noop_safe_point(
                    run,
                    owner_id=owner_id,
                    token=token,
                    now=now,
                    dispatch_state=state,
                )
            else:
                event_type = (
                    "run.paused"
                    if target is RunStatus.PAUSED
                    else "run.resumed"
                )
                try:
                    current, _ = self._journal.set_status_with_event(
                        run_id,
                        expected=run.status,
                        status=target,
                        owner_id=owner_id,
                        token=token,
                        now=now,
                        event_type=event_type,
                        event_payload=self._event_payload(
                            run, target=target, dispatch_state=state
                        ),
                        sensitive=False,
                    )
                except LeaseLostError as exc:
                    self._raise_mapped(
                        exc, operation="runner_safe_point", run_id=run_id
                    )
                except (JournalConflictError, InvalidStateTransitionError):
                    # Another call with the same owner token may have applied
                    # the state first.  Re-read before deciding whether it is
                    # an idempotent outcome, a new operator intent, or terminal.
                    continue
                except (JournalError, ValueError, sqlite3.Error) as exc:
                    self._raise_mapped(
                        exc, operation="runner_safe_point", run_id=run_id
                    )

            # A control CAS may race the status transaction.  Reconcile before
            # returning so a cancel observed during this call is never ignored.
            if current.terminal:
                self._require_nonterminal(
                    current, operation="runner_safe_point"
                )
            expected_desired = (
                DesiredState.PAUSE
                if target is RunStatus.PAUSED
                else DesiredState.RUN
            )
            if (
                current.status is target
                and current.desired_state is expected_desired
            ):
                return current
            if target is RunStatus.PAUSED and current.status is RunStatus.PAUSED:
                # Pausing releases the lease in the same journal transaction.
                # A resume/cancel request may win immediately afterwards, but
                # this owner is now fenced and must stop.  A newly claimed
                # runner applies that newer intent.
                return current

        raise self._error(
            "RUN.CONTROL_RACE",
            f"control state for run {run_id} did not stabilize at a safe point",
            operation="runner_safe_point",
            run_id=run_id,
            retryable=True,
        )

    def _request_desired_state(
        self,
        run_id: str,
        desired: DesiredState,
        *,
        operation: str,
        event_type: str,
    ) -> RunRecord:
        for _ in range(MAX_CONTROL_CAS_ATTEMPTS):
            run = self.get(run_id)
            self._require_nonterminal(run, operation=operation)
            if run.desired_state is desired:
                return run
            if run.desired_state is DesiredState.CANCEL:
                raise self._error(
                    "RUN.CANCEL_PENDING",
                    f"run {run_id} already has a sticky cancel request",
                    operation=operation,
                    run=run,
                    details={"requestedDesiredState": desired.value},
                )
            try:
                updated, _ = self._journal.compare_and_set_desired_state_with_event(
                    run_id,
                    expected=run.desired_state,
                    desired=desired,
                    event_type=event_type,
                    event_payload={
                        "fromDesiredState": run.desired_state.value,
                        "toDesiredState": desired.value,
                    },
                    sensitive=False,
                )
                return updated
            except JournalConflictError:
                # Same-target requests become idempotent after the re-read.
                # Cancel retries from either run or pause and therefore wins
                # over a racing non-terminal control request.
                continue
            except InvalidStateTransitionError:
                # The run may have become terminal after our initial read.
                continue
            except (JournalError, ValueError, sqlite3.Error) as exc:
                self._raise_mapped(exc, operation=operation, run_id=run_id)
        raise self._error(
            "RUN.CONTROL_RACE",
            f"control state for run {run_id} changed too frequently",
            operation=operation,
            run_id=run_id,
            retryable=True,
        )

    def _append_noop_safe_point(
        self,
        run: RunRecord,
        *,
        owner_id: str,
        token: str,
        now: float | None,
        dispatch_state: DispatchState,
    ) -> RunRecord:
        try:
            self._journal.append_event(
                run.run_id,
                "run.safe_point",
                self._event_payload(
                    run, target=run.status, dispatch_state=dispatch_state
                ),
                owner_id=owner_id,
                token=token,
                now=now,
                sensitive=False,
            )
            return self._journal.get_run(run.run_id)
        except LeaseLostError as exc:
            self._raise_mapped(
                exc, operation="runner_safe_point", run_id=run.run_id
            )
        except InvalidStateTransitionError as exc:
            current = self.get(run.run_id)
            self._require_nonterminal(current, operation="runner_safe_point")
            self._raise_mapped(
                exc, operation="runner_safe_point", run_id=run.run_id
            )
        except (JournalError, ValueError, sqlite3.Error) as exc:
            self._raise_mapped(
                exc, operation="runner_safe_point", run_id=run.run_id
            )

    def _terminal_at_safe_point(
        self,
        run: RunRecord,
        *,
        owner_id: str,
        token: str,
        now: float | None,
        status: RunStatus,
        event_type: str,
        dispatch_state: DispatchState,
    ) -> RunRecord | None:
        unknown = status is RunStatus.UNKNOWN_EFFECT
        code = "RUN.UNKNOWN_EFFECT" if unknown else "RUN.CANCELLED"
        message = (
            "dispatch outcome is unknown; cancellation cannot be proven"
            if unknown and run.desired_state is DesiredState.CANCEL
            else "dispatch outcome is unknown at runner safe point"
            if unknown
            else "run cancelled at runner safe point"
        )
        error = AutomationError(
            code,
            message,
            category="run",
            phase="execute",
            retryable=False,
            effect="unknown" if unknown else "none",
            workflow=run.workflow_name,
            details={
                "runId": run.run_id,
                "desiredState": run.desired_state.value,
                "dispatchState": dispatch_state.value,
            },
        ).to_dict()
        try:
            transitioned, _ = self._journal.set_status_with_event(
                run.run_id,
                expected=run.status,
                status=status,
                owner_id=owner_id,
                token=token,
                now=now,
                error=error,
                event_type=event_type,
                event_payload=self._event_payload(
                    run, target=status, dispatch_state=dispatch_state
                ),
                sensitive=False,
            )
            return transitioned
        except LeaseLostError as exc:
            self._raise_mapped(
                exc, operation="runner_safe_point", run_id=run.run_id
            )
        except JournalConflictError:
            current = self.get(run.run_id)
            self._require_nonterminal(current, operation="runner_safe_point")
            return None
        except InvalidStateTransitionError as exc:
            current = self.get(run.run_id)
            self._require_nonterminal(current, operation="runner_safe_point")
            self._raise_mapped(
                exc, operation="runner_safe_point", run_id=run.run_id
            )
        except (JournalError, ValueError, sqlite3.Error) as exc:
            self._raise_mapped(
                exc, operation="runner_safe_point", run_id=run.run_id
            )

    @staticmethod
    def _event_payload(
        run: RunRecord, *, target: RunStatus, dispatch_state: DispatchState
    ) -> dict[str, str]:
        return {
            "fromStatus": run.status.value,
            "toStatus": target.value,
            "desiredState": run.desired_state.value,
            "dispatchState": dispatch_state.value,
        }

    def _dispatch_state(
        self, value: DispatchState | str, *, run_id: str
    ) -> DispatchState:
        try:
            return DispatchState(value)
        except (TypeError, ValueError) as exc:
            raise self._error(
                "RUN.INVALID_ARGUMENT",
                f"invalid dispatch state: {value!r}",
                operation="runner_safe_point",
                run_id=run_id,
                details={
                    "allowed": [item.value for item in DispatchState],
                },
                cause=exc,
            ) from exc

    def _require_nonterminal(self, run: RunRecord, *, operation: str) -> None:
        if run.terminal:
            raise self._error(
                "RUN.TERMINAL",
                f"terminal run {run.run_id} is immutable ({run.status.value})",
                operation=operation,
                run=run,
            )

    def _call(
        self, operation: str, run_id: str | None, callback: Callable[[], _T]
    ) -> _T:
        try:
            return callback()
        except RunServiceError:
            raise
        except (JournalError, ValueError, TypeError, sqlite3.Error) as exc:
            self._raise_mapped(exc, operation=operation, run_id=run_id)

    def _raise_mapped(
        self, exc: BaseException, *, operation: str, run_id: str | None
    ) -> NoReturn:
        if isinstance(exc, RunNotFoundError):
            code, message, retryable = (
                "RUN.NOT_FOUND",
                f"run not found: {run_id}",
                False,
            )
        elif isinstance(exc, LeaseLostError):
            code, message, retryable = (
                "RUN.LEASE_LOST",
                f"owner lease is no longer valid for run {run_id}",
                False,
            )
        elif isinstance(exc, LeaseConflictError):
            code, message, retryable = (
                "RUN.LEASE_CONFLICT",
                f"run {run_id} has another live owner",
                True,
            )
        elif isinstance(exc, InvalidStateTransitionError):
            code, message, retryable = (
                "RUN.INVALID_STATE",
                str(exc),
                False,
            )
        elif isinstance(exc, SensitiveDataError):
            code, message, retryable = (
                "RUN.SENSITIVE_DATA",
                str(exc),
                False,
            )
        elif isinstance(exc, JournalConflictError):
            code, message, retryable = "RUN.CONFLICT", str(exc), True
        elif isinstance(exc, (ValueError, TypeError)):
            code, message, retryable = (
                "RUN.INVALID_ARGUMENT",
                str(exc),
                False,
            )
        else:
            code, message, retryable = (
                "RUN.STORAGE_FAILURE",
                "durable run storage failed",
                True,
            )
        raise self._error(
            code,
            message,
            operation=operation,
            run_id=run_id,
            retryable=retryable,
            cause=exc,
        ) from exc

    @staticmethod
    def _error(
        code: str,
        message: str,
        *,
        operation: str,
        run: RunRecord | None = None,
        run_id: str | None = None,
        retryable: bool = False,
        details: dict[str, Any] | None = None,
        cause: BaseException | None = None,
    ) -> RunServiceError:
        merged: dict[str, Any] = {"operation": operation}
        effective_run_id = run.run_id if run is not None else run_id
        if effective_run_id is not None:
            merged["runId"] = effective_run_id
        if run is not None:
            merged.update(
                {
                    "status": run.status.value,
                    "desiredState": run.desired_state.value,
                }
            )
        merged.update(details or {})
        return RunServiceError(
            code,
            message,
            category="run",
            phase="control",
            retryable=retryable,
            effect="none",
            workflow=run.workflow_name if run is not None else None,
            details=merged,
            cause=cause,
        )


__all__ = [
    "DispatchState",
    "MAX_CONTROL_CAS_ATTEMPTS",
    "MAX_SAFE_POINT_RECONCILIATIONS",
    "RunService",
    "RunServiceError",
]
