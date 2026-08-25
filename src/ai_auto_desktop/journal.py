"""Crash-safe SQLite storage for workflow run lifecycle state.

The journal deliberately has no dependency on the runtime or descriptor model.
Callers cross the durable boundary with JSON-compatible values and must opt out
of persistence when a descriptor declares sensitive inputs or outputs.
"""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass, field as dataclass_field
from datetime import datetime, timezone
from enum import StrEnum
import hashlib
import json
import math
import os
from pathlib import Path
import re
import secrets
import sqlite3
import time
from typing import Any, Iterator, Mapping
import uuid


SCHEMA_VERSION = 1
DEFAULT_BUSY_TIMEOUT_MS = 5_000
EVENT_TYPE_PATTERN = re.compile(r"^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$")
_UNSET = object()


class RunStatus(StrEnum):
    """Persisted run states."""

    PENDING = "pending"
    RUNNING = "running"
    PAUSED = "paused"
    SUCCEEDED = "succeeded"
    FAILED = "failed"
    TIMED_OUT = "timed_out"
    CANCELLED = "cancelled"
    UNKNOWN_EFFECT = "unknown_effect"


class DesiredState(StrEnum):
    """Operator intent used by a runner at safe interruption points."""

    RUN = "run"
    PAUSE = "pause"
    CANCEL = "cancel"


TERMINAL_RUN_STATUSES = frozenset(
    {
        RunStatus.SUCCEEDED,
        RunStatus.FAILED,
        RunStatus.TIMED_OUT,
        RunStatus.CANCELLED,
        RunStatus.UNKNOWN_EFFECT,
    }
)
ALLOWED_STATUS_TRANSITIONS = {
    RunStatus.PENDING: frozenset(
        {RunStatus.RUNNING, RunStatus.FAILED, RunStatus.TIMED_OUT, RunStatus.CANCELLED}
    ),
    RunStatus.RUNNING: frozenset(
        {
            RunStatus.PAUSED,
            RunStatus.SUCCEEDED,
            RunStatus.FAILED,
            RunStatus.TIMED_OUT,
            RunStatus.CANCELLED,
            RunStatus.UNKNOWN_EFFECT,
        }
    ),
    RunStatus.PAUSED: frozenset(
        {
            RunStatus.RUNNING,
            RunStatus.FAILED,
            RunStatus.TIMED_OUT,
            RunStatus.CANCELLED,
            RunStatus.UNKNOWN_EFFECT,
        }
    ),
}
ALLOWED_DESIRED_STATE_TRANSITIONS = {
    DesiredState.RUN: frozenset({DesiredState.PAUSE, DesiredState.CANCEL}),
    DesiredState.PAUSE: frozenset({DesiredState.RUN, DesiredState.CANCEL}),
    DesiredState.CANCEL: frozenset(),
}


class JournalError(RuntimeError):
    """Base class for lifecycle persistence errors."""


class RunNotFoundError(JournalError):
    """The requested run does not exist."""


class JournalConflictError(JournalError):
    """A compare-and-set operation observed different state."""


class InvalidStateTransitionError(JournalError):
    """A requested run transition violates lifecycle invariants."""


class LeaseConflictError(JournalConflictError):
    """A live lease is owned by another claimant."""


class LeaseLostError(JournalConflictError):
    """The supplied lease token no longer owns the run."""


class SensitiveDataError(JournalError):
    """Sensitive values were offered to the durable journal."""


@dataclass(frozen=True, slots=True)
class RunRecord:
    run_id: str
    workflow_name: str
    workflow_version: str | None
    plan_digest: str | None
    status: RunStatus
    desired_state: DesiredState
    inputs: Any
    output: Any | None
    error: Any | None
    checkpoint: Any | None
    owner_id: str | None
    lease_expires_at: float | None
    created_at: str
    updated_at: str
    finished_at: str | None

    @property
    def terminal(self) -> bool:
        return self.status in TERMINAL_RUN_STATUSES

    def to_dict(self) -> dict[str, Any]:
        """Return the versioned transport shape described by run.schema.json."""

        workflow: dict[str, str] = {"name": self.workflow_name}
        if self.workflow_version is not None:
            workflow["version"] = self.workflow_version
        if self.plan_digest is not None:
            workflow["planDigest"] = self.plan_digest
        lease = None
        if self.owner_id is not None:
            lease = {
                "ownerId": self.owner_id,
                "expiresAt": self.lease_expires_at,
            }
        return {
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Run",
            "runId": self.run_id,
            "workflow": workflow,
            "status": self.status.value,
            "desiredState": self.desired_state.value,
            "inputs": self.inputs,
            "output": self.output,
            "error": self.error,
            "checkpoint": self.checkpoint,
            "ownerLease": lease,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "finishedAt": self.finished_at,
        }


@dataclass(frozen=True, slots=True)
class EventRecord:
    run_id: str
    seq: int
    event_type: str
    payload: Any
    created_at: str

    def to_dict(self) -> dict[str, Any]:
        """Return the versioned transport shape described by event.schema.json."""

        return {
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "RunEvent",
            "runId": self.run_id,
            "seq": self.seq,
            "type": self.event_type,
            "payload": self.payload,
            "createdAt": self.created_at,
        }


@dataclass(frozen=True, slots=True)
class OwnerLease:
    run_id: str
    owner_id: str
    token: str = dataclass_field(repr=False)
    expires_at: float


def durable_descriptor_eligible(descriptor: object) -> bool:
    """Return whether descriptor inputs and outputs are safe to persist.

    This conservative first slice refuses the complete run when any declared
    input or output is marked sensitive.  A future secret broker may replace
    sensitive values with durable references before calling this module.
    """

    for section_name in ("inputs", "outputs"):
        if isinstance(descriptor, Mapping):
            section = descriptor.get(section_name, {})
        else:
            section = getattr(descriptor, section_name, None)
        if not isinstance(section, Mapping):
            return False
        for definition in section.values():
            if not isinstance(definition, Mapping):
                return False
            sensitive = definition.get("sensitive", False)
            if not isinstance(sensitive, bool) or sensitive:
                return False
    return True


def assert_durable_descriptor_eligible(descriptor: object) -> None:
    """Reject descriptors whose declared values may not enter SQLite."""

    if not durable_descriptor_eligible(descriptor):
        raise SensitiveDataError(
            "durable runs do not accept descriptors with sensitive inputs or outputs"
        )


class JournalStore:
    """SQLite-backed lifecycle journal.

    A store owns one SQLite connection and therefore belongs to the thread
    that constructed it.  Concurrent workers must open their own store for the
    same database path; WAL plus ``BEGIN IMMEDIATE`` serializes their writes.
    """

    def __init__(
        self,
        path: str | os.PathLike[str],
        *,
        busy_timeout_ms: int = DEFAULT_BUSY_TIMEOUT_MS,
    ) -> None:
        if (
            isinstance(busy_timeout_ms, bool)
            or not isinstance(busy_timeout_ms, int)
            or busy_timeout_ms < 0
        ):
            raise ValueError("busy_timeout_ms must be a non-negative integer")
        self.path = Path(path)
        if self.path != Path(":memory:"):
            self.path.parent.mkdir(parents=True, exist_ok=True)
        self.busy_timeout_ms = int(busy_timeout_ms)
        self._connection = sqlite3.connect(
            os.fspath(self.path),
            isolation_level=None,
            timeout=self.busy_timeout_ms / 1_000,
            check_same_thread=True,
        )
        self._connection.row_factory = sqlite3.Row
        try:
            self._configure()
            self._migrate()
        except BaseException:
            self._connection.close()
            raise

    def __enter__(self) -> "JournalStore":
        return self

    def __exit__(self, *args: object) -> None:
        self.close()

    def close(self) -> None:
        self._connection.close()

    def _configure(self) -> None:
        self._connection.execute(f"PRAGMA busy_timeout = {self.busy_timeout_ms}")
        self._connection.execute("PRAGMA foreign_keys = ON")
        self._connection.execute("PRAGMA synchronous = FULL")
        journal_mode = self._connection.execute("PRAGMA journal_mode = WAL").fetchone()[0]
        # SQLite cannot enable WAL for an in-memory database.  File-backed
        # journals must never silently fall back to a weaker mode.
        if self.path != Path(":memory:") and str(journal_mode).lower() != "wal":
            self.close()
            raise JournalError(f"SQLite refused WAL mode: {journal_mode!r}")
        if self._connection.execute("PRAGMA foreign_keys").fetchone()[0] != 1:
            self.close()
            raise JournalError("SQLite foreign key enforcement is unavailable")
        if self._connection.execute("PRAGMA synchronous").fetchone()[0] != 2:
            self.close()
            raise JournalError("SQLite synchronous=FULL is unavailable")

    def _migrate(self) -> None:
        # Recheck the version after obtaining the writer lock.  This makes
        # first-open initialization safe when several processes race to open
        # the same new journal.
        with self._transaction():
            current = int(
                self._connection.execute("PRAGMA user_version").fetchone()[0]
            )
            if current > SCHEMA_VERSION:
                raise JournalError(
                    f"journal schema {current} is newer than supported "
                    f"{SCHEMA_VERSION}"
                )
            if current == SCHEMA_VERSION:
                return
            self._connection.execute(
                """
                CREATE TABLE runs (
                    run_id TEXT PRIMARY KEY,
                    workflow_name TEXT NOT NULL,
                    workflow_version TEXT,
                    plan_digest TEXT,
                    status TEXT NOT NULL CHECK (status IN (
                        'pending', 'running', 'paused', 'succeeded', 'failed',
                        'timed_out', 'cancelled', 'unknown_effect'
                    )),
                    desired_state TEXT NOT NULL CHECK (desired_state IN (
                        'run', 'pause', 'cancel'
                    )),
                    inputs_json TEXT NOT NULL,
                    output_json TEXT,
                    error_json TEXT,
                    checkpoint_json TEXT,
                    owner_id TEXT,
                    lease_token_hash TEXT,
                    lease_expires_at REAL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    finished_at TEXT,
                    CHECK ((owner_id IS NULL) = (lease_token_hash IS NULL)),
                    CHECK ((owner_id IS NULL) = (lease_expires_at IS NULL)),
                    CHECK (length(run_id) BETWEEN 1 AND 256),
                    CHECK (length(workflow_name) BETWEEN 1 AND 192),
                    CHECK (workflow_version IS NULL OR length(workflow_version) BETWEEN 1 AND 128),
                    CHECK (plan_digest IS NULL OR length(plan_digest) BETWEEN 1 AND 256),
                    CHECK (owner_id IS NULL OR length(owner_id) BETWEEN 1 AND 256),
                    CHECK (
                        (status NOT IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
                         AND output_json IS NULL AND error_json IS NULL)
                        OR
                        (status = 'succeeded' AND error_json IS NULL)
                        OR
                        (status IN ('failed','timed_out','cancelled','unknown_effect')
                         AND output_json IS NULL AND error_json IS NOT NULL)
                    ),
                    CHECK (
                        (status IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
                         AND finished_at IS NOT NULL)
                        OR
                        (status NOT IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
                         AND finished_at IS NULL)
                    )
                )
                """
            )
            self._connection.execute(
                """
                CREATE TABLE events (
                    run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                    seq INTEGER NOT NULL CHECK (seq >= 1),
                    event_type TEXT NOT NULL CHECK (length(event_type) BETWEEN 1 AND 192),
                    payload_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (run_id, seq)
                ) WITHOUT ROWID
                """
            )
            self._connection.execute(
                "CREATE INDEX runs_created_at_idx "
                "ON runs(created_at DESC, run_id DESC)"
            )
            self._connection.execute(
                "CREATE INDEX runs_status_created_at_idx "
                "ON runs(status, created_at DESC, run_id DESC)"
            )
            self._connection.execute(
                """
                CREATE TRIGGER runs_status_transition_valid
                BEFORE UPDATE OF status ON runs
                WHEN NEW.status != OLD.status AND NOT (
                    (OLD.status = 'pending' AND NEW.status IN (
                        'running','failed','timed_out','cancelled'
                    )) OR
                    (OLD.status = 'running' AND NEW.status IN (
                        'paused','succeeded','failed','timed_out','cancelled','unknown_effect'
                    )) OR
                    (OLD.status = 'paused' AND NEW.status IN (
                        'running','failed','timed_out','cancelled','unknown_effect'
                    ))
                )
                BEGIN
                    SELECT RAISE(ABORT, 'invalid run status transition');
                END
                """
            )
            self._connection.execute(
                """
                CREATE TRIGGER runs_cancel_intent_absorbing
                BEFORE UPDATE OF desired_state ON runs
                WHEN OLD.desired_state = 'cancel' AND NEW.desired_state != 'cancel'
                BEGIN
                    SELECT RAISE(ABORT, 'cancel desired state is absorbing');
                END
                """
            )
            self._connection.execute(
                """
                CREATE TRIGGER runs_terminal_row_immutable
                BEFORE UPDATE ON runs
                WHEN OLD.status IN (
                    'succeeded','failed','timed_out','cancelled','unknown_effect'
                )
                BEGIN
                    SELECT RAISE(ABORT, 'terminal run is immutable');
                END
                """
            )
            self._connection.execute(
                """
                CREATE TRIGGER events_reject_terminal_run
                BEFORE INSERT ON events
                WHEN EXISTS (
                    SELECT 1 FROM runs
                    WHERE run_id = NEW.run_id
                      AND status IN (
                        'succeeded','failed','timed_out','cancelled','unknown_effect'
                      )
                )
                BEGIN
                    SELECT RAISE(ABORT, 'terminal run cannot accept events');
                END
                """
            )
            self._connection.execute(
                """
                CREATE TRIGGER events_contiguous_sequence
                BEFORE INSERT ON events
                WHEN NEW.seq != COALESCE(
                    (SELECT MAX(seq) + 1 FROM events WHERE run_id = NEW.run_id),
                    1
                )
                BEGIN
                    SELECT RAISE(ABORT, 'event sequence must be contiguous');
                END
                """
            )
            self._connection.execute(f"PRAGMA user_version = {SCHEMA_VERSION}")

    @contextmanager
    def _transaction(self) -> Iterator[None]:
        self._connection.execute("BEGIN IMMEDIATE")
        try:
            yield
        except BaseException:
            self._connection.rollback()
            raise
        else:
            self._connection.commit()

    def create_run(
        self,
        *,
        workflow_name: str,
        inputs: Any,
        descriptor: object,
        run_id: str | None = None,
        workflow_version: str | None = None,
        plan_digest: str | None = None,
        status: RunStatus | str = RunStatus.PENDING,
        desired_state: DesiredState | str = DesiredState.RUN,
        sensitive: bool = False,
    ) -> RunRecord:
        """Create one run after an explicit metadata-based sensitivity check.

        The caller remains responsible for propagating runtime taint via
        ``sensitive=True``; this module cannot infer secrets from arbitrary
        JSON values.
        """

        _reject_sensitive(sensitive)
        assert_durable_descriptor_eligible(descriptor)
        run_id = run_id or uuid.uuid4().hex
        _require_nonempty_string(run_id, "run_id")
        _require_nonempty_string(workflow_name, "workflow_name")
        if len(run_id) > 256:
            raise ValueError("run_id must not exceed 256 characters")
        if len(workflow_name) > 192:
            raise ValueError("workflow_name must not exceed 192 characters")
        _validate_optional_string(workflow_version, "workflow_version", 128)
        _validate_optional_string(plan_digest, "plan_digest", 256)
        status = _run_status(status)
        desired_state = _desired_state(desired_state)
        if status in TERMINAL_RUN_STATUSES:
            raise InvalidStateTransitionError(
                "a run cannot be created directly in a terminal state"
            )
        now = _utc_now()
        inputs_json = _encode_json(inputs, "inputs")
        try:
            self._connection.execute(
                """
                INSERT INTO runs (
                    run_id, workflow_name, workflow_version, plan_digest,
                    status, desired_state, inputs_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    run_id,
                    workflow_name,
                    workflow_version,
                    plan_digest,
                    status.value,
                    desired_state.value,
                    inputs_json,
                    now,
                    now,
                ),
            )
        except sqlite3.IntegrityError as exc:
            raise JournalConflictError(f"run already exists: {run_id}") from exc
        return self.get_run(run_id)

    def create_run_with_event(
        self,
        *,
        workflow_name: str,
        inputs: Any,
        descriptor: object,
        event_type: str,
        event_payload: Any,
        run_id: str | None = None,
        workflow_version: str | None = None,
        plan_digest: str | None = None,
        status: RunStatus | str = RunStatus.PENDING,
        desired_state: DesiredState | str = DesiredState.RUN,
        sensitive: bool = False,
    ) -> tuple[RunRecord, EventRecord]:
        """Atomically create a run and its first, non-sensitive event."""

        _reject_sensitive(sensitive)
        assert_durable_descriptor_eligible(descriptor)
        effective_run_id = run_id or uuid.uuid4().hex
        _require_nonempty_string(effective_run_id, "run_id")
        _require_nonempty_string(workflow_name, "workflow_name")
        if len(effective_run_id) > 256:
            raise ValueError("run_id must not exceed 256 characters")
        if len(workflow_name) > 192:
            raise ValueError("workflow_name must not exceed 192 characters")
        _validate_optional_string(workflow_version, "workflow_version", 128)
        _validate_optional_string(plan_digest, "plan_digest", 256)
        status_value = _run_status(status)
        desired_value = _desired_state(desired_state)
        if status_value in TERMINAL_RUN_STATUSES:
            raise InvalidStateTransitionError(
                "a run cannot be created directly in a terminal state"
            )
        inputs_json = _encode_json(inputs, "inputs")
        with self._transaction():
            created_at = _utc_now()
            try:
                self._connection.execute(
                    """
                    INSERT INTO runs (
                        run_id, workflow_name, workflow_version, plan_digest,
                        status, desired_state, inputs_json, created_at, updated_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (effective_run_id, workflow_name, workflow_version, plan_digest,
                     status_value.value, desired_value.value, inputs_json,
                     created_at, created_at),
                )
            except sqlite3.IntegrityError as exc:
                raise JournalConflictError(
                    f"run already exists: {effective_run_id}"
                ) from exc
            event = self._append_event_locked(
                effective_run_id, event_type, event_payload
            )
        return self.get_run(effective_run_id), event

    def get_run(self, run_id: str) -> RunRecord:
        row = self._connection.execute(
            "SELECT * FROM runs WHERE run_id = ?", (run_id,)
        ).fetchone()
        if row is None:
            raise RunNotFoundError(f"run not found: {run_id}")
        return _run_from_row(row)

    def list_runs(
        self,
        *,
        status: RunStatus | str | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> list[RunRecord]:
        if isinstance(limit, bool) or limit < 1 or limit > 10_000:
            raise ValueError("limit must be between 1 and 10000")
        if isinstance(offset, bool) or offset < 0:
            raise ValueError("offset must be non-negative")
        parameters: list[Any] = []
        where = ""
        if status is not None:
            where = "WHERE status = ?"
            parameters.append(_run_status(status).value)
        parameters.extend((limit, offset))
        rows = self._connection.execute(
            f"""SELECT * FROM runs {where}
            ORDER BY created_at DESC, run_id DESC LIMIT ? OFFSET ?""",
            parameters,
        ).fetchall()
        return [_run_from_row(row) for row in rows]

    def compare_and_set_desired_state(
        self,
        run_id: str,
        *,
        expected: DesiredState | str,
        desired: DesiredState | str,
        owner_id: str | None = None,
        token: str | None = None,
        now: float | None = None,
    ) -> RunRecord:
        """Atomically change operator intent when the expected value matches.

        This is a control-plane mutation and never requires or inspects the
        runner's bearer lease.  The optional lease arguments are retained for
        call-site compatibility and intentionally ignored.  ``cancel`` is
        absorbing.
        """

        expected_value = _desired_state(expected)
        desired_value = _desired_state(desired)
        _validate_desired_state_transition(expected_value, desired_value)
        with self._transaction():
            self._compare_and_set_desired_state_locked(
                run_id, expected=expected_value, desired=desired_value,
            )
        return self.get_run(run_id)

    def compare_and_set_desired_state_with_event(
        self,
        run_id: str,
        *,
        expected: DesiredState | str,
        desired: DesiredState | str,
        event_type: str,
        event_payload: Any,
        owner_id: str | None = None,
        token: str | None = None,
        now: float | None = None,
        sensitive: bool = False,
    ) -> tuple[RunRecord, EventRecord]:
        """Atomically change desired state and append its control event."""

        _reject_sensitive(sensitive)
        expected_value = _desired_state(expected)
        desired_value = _desired_state(desired)
        _validate_desired_state_transition(expected_value, desired_value)
        with self._transaction():
            self._compare_and_set_desired_state_locked(
                run_id, expected=expected_value, desired=desired_value,
            )
            event = self._append_event_locked(run_id, event_type, event_payload)
        return self.get_run(run_id), event

    def _compare_and_set_desired_state_locked(
        self,
        run_id: str,
        *,
        expected: DesiredState,
        desired: DesiredState,
    ) -> None:
        cursor = self._connection.execute(
            """
            UPDATE runs SET desired_state = ?, updated_at = ?
            WHERE run_id = ? AND desired_state = ?
              AND status NOT IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
            """,
            (desired.value, _utc_now(), run_id, expected.value),
        )
        if cursor.rowcount != 1:
            run = self.get_run(run_id)
            if run.terminal:
                raise InvalidStateTransitionError(
                    f"terminal run {run_id} cannot change desired state"
                )
            raise JournalConflictError(
                f"desired state changed concurrently: expected {expected.value}, "
                f"found {run.desired_state.value}"
            )

    def set_status(
        self,
        run_id: str,
        *,
        expected: RunStatus | str,
        status: RunStatus | str,
        owner_id: str,
        token: str,
        now: float | None = None,
        output: Any | None = None,
        error: Any | None = None,
        sensitive: bool = False,
    ) -> RunRecord:
        """CAS status under a live lease; reject caller-marked sensitive data."""

        _reject_sensitive(sensitive)
        expected_value = _run_status(expected)
        status_value = _run_status(status)
        now_value = _timestamp(now)
        with self._transaction():
            self._set_status_locked(
                run_id, expected=expected_value, status=status_value,
                owner_id=owner_id, token=token, now=now_value, output=output,
                error=error,
            )
        return self.get_run(run_id)

    def set_status_with_event(
        self,
        run_id: str,
        *,
        expected: RunStatus | str,
        status: RunStatus | str,
        owner_id: str,
        token: str,
        event_type: str,
        event_payload: Any,
        now: float | None = None,
        output: Any | None = None,
        error: Any | None = None,
        checkpoint: Any = _UNSET,
        expected_desired_state: DesiredState | str | None = None,
        sensitive: bool = False,
    ) -> tuple[RunRecord, EventRecord]:
        """Atomically append an event and perform an owner-fenced status CAS."""

        _reject_sensitive(sensitive)
        expected_value = _run_status(expected)
        status_value = _run_status(status)
        expected_desired_value = (
            None
            if expected_desired_state is None
            else _desired_state(expected_desired_state)
        )
        now_value = _timestamp(now)
        checkpoint_json = (
            None if checkpoint is _UNSET else _encode_json(checkpoint, "checkpoint")
        )
        with self._transaction():
            self._require_live_lease_locked(
                run_id, owner_id=owner_id, token=token, now=now_value
            )
            self._require_expected_run_state_locked(
                run_id, status=expected_value,
                desired_state=expected_desired_value,
            )
            event = self._append_event_locked(run_id, event_type, event_payload)
            if checkpoint is not _UNSET:
                cursor = self._connection.execute(
                    """
                    UPDATE runs SET checkpoint_json = ?, updated_at = ?
                    WHERE run_id = ? AND owner_id = ? AND lease_token_hash = ?
                      AND lease_expires_at > ?
                    """,
                    (checkpoint_json, _utc_now(), run_id, owner_id,
                     _lease_token_hash(token), now_value),
                )
                if cursor.rowcount != 1:
                    self._raise_lease_lost(run_id)
            self._set_status_locked(
                run_id, expected=expected_value, status=status_value,
                owner_id=owner_id, token=token, now=now_value, output=output,
                error=error,
            )
        return self.get_run(run_id), event

    def _set_status_locked(
        self,
        run_id: str,
        *,
        expected: RunStatus,
        status: RunStatus,
        owner_id: str,
        token: str,
        now: float,
        output: Any | None,
        error: Any | None,
    ) -> None:
        _validate_status_transition(expected, status, output=output, error=error)
        output_json = _encode_optional_json(output, "output")
        error_json = _encode_optional_json(error, "error")
        releases_lease = status == RunStatus.PAUSED or status in TERMINAL_RUN_STATUSES
        finished_at = _utc_now() if status in TERMINAL_RUN_STATUSES else None
        cursor = self._connection.execute(
            """
            UPDATE runs
            SET status = ?, output_json = ?, error_json = ?, finished_at = ?,
                updated_at = ?,
                owner_id = CASE WHEN ? THEN NULL ELSE owner_id END,
                lease_token_hash = CASE WHEN ? THEN NULL ELSE lease_token_hash END,
                lease_expires_at = CASE WHEN ? THEN NULL ELSE lease_expires_at END
            WHERE run_id = ? AND status = ? AND owner_id = ? AND lease_token_hash = ?
              AND lease_expires_at > ?
              AND status NOT IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
            """,
            (
                status.value, output_json, error_json, finished_at, _utc_now(),
                releases_lease, releases_lease, releases_lease, run_id,
                expected.value, owner_id,
                _lease_token_hash(token), now,
            ),
        )
        if cursor.rowcount != 1:
            self._raise_status_or_lease_conflict(
                run_id, expected=expected, owner_id=owner_id, token=token, now=now,
            )

    def claim_owner(
        self,
        run_id: str,
        *,
        owner_id: str,
        ttl_seconds: float,
        now: float | None = None,
    ) -> OwnerLease:
        """Acquire an unowned or expired run lease and return its secret token."""

        _require_nonempty_string(owner_id, "owner_id")
        if len(owner_id) > 256:
            raise ValueError("owner_id must not exceed 256 characters")
        now_value = _timestamp(now)
        ttl = _positive_seconds(ttl_seconds)
        token = secrets.token_urlsafe(32)
        expires_at = now_value + ttl
        if not math.isfinite(expires_at):
            raise ValueError("lease expiry must be finite")
        updated_at = _utc_now()
        cursor = self._connection.execute(
            """
            UPDATE runs
            SET owner_id = ?, lease_token_hash = ?, lease_expires_at = ?, updated_at = ?
            WHERE run_id = ?
              AND status NOT IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
              AND (owner_id IS NULL OR lease_expires_at <= ?)
            """,
            (owner_id, _lease_token_hash(token), expires_at, updated_at, run_id, now_value),
        )
        if cursor.rowcount != 1:
            run = self.get_run(run_id)
            if run.terminal:
                raise InvalidStateTransitionError(
                    f"terminal run {run_id} cannot be claimed"
                )
            raise LeaseConflictError(
                f"run {run_id} has a live lease owned by {run.owner_id!r}"
            )
        return OwnerLease(run_id, owner_id, token, expires_at)

    def heartbeat_owner(
        self,
        run_id: str,
        *,
        owner_id: str,
        token: str,
        ttl_seconds: float,
        now: float | None = None,
    ) -> OwnerLease:
        """Extend a live lease only when both owner and token still match."""

        now_value = _timestamp(now)
        ttl = _positive_seconds(ttl_seconds)
        expires_at = now_value + ttl
        if not math.isfinite(expires_at):
            raise ValueError("lease expiry must be finite")
        updated_at = _utc_now()
        cursor = self._connection.execute(
            """
            UPDATE runs SET lease_expires_at = ?, updated_at = ?
            WHERE run_id = ? AND owner_id = ? AND lease_token_hash = ?
              AND lease_expires_at > ?
              AND status NOT IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
            """,
            (expires_at, updated_at, run_id, owner_id, _lease_token_hash(token), now_value),
        )
        if cursor.rowcount != 1:
            self._raise_lease_lost(run_id)
        return OwnerLease(run_id, owner_id, token, expires_at)

    def release_owner(
        self,
        run_id: str,
        *,
        owner_id: str,
        token: str,
        now: float | None = None,
    ) -> RunRecord:
        """Release a lease only when its owner identity and token match."""

        cursor = self._connection.execute(
            """
            UPDATE runs
            SET owner_id = NULL, lease_token_hash = NULL, lease_expires_at = NULL,
                updated_at = ?
            WHERE run_id = ? AND owner_id = ? AND lease_token_hash = ?
              AND lease_expires_at > ?
            """,
            (_utc_now(), run_id, owner_id, _lease_token_hash(token), _timestamp(now)),
        )
        if cursor.rowcount != 1:
            self._raise_lease_lost(run_id)
        return self.get_run(run_id)

    def ensure_live_lease(
        self,
        run_id: str,
        *,
        owner_id: str,
        token: str,
        now: float | None = None,
    ) -> RunRecord:
        """Return the run only when the supplied owner lease is still live."""

        now_value = _timestamp(now)
        with self._transaction():
            self._require_live_lease_locked(
                run_id, owner_id=owner_id, token=token, now=now_value
            )
        return self.get_run(run_id)

    def _raise_lease_lost(self, run_id: str) -> None:
        run = self.get_run(run_id)
        if run.terminal:
            raise InvalidStateTransitionError(
                f"terminal run {run_id} is immutable ({run.status.value})"
            )
        raise LeaseLostError(f"owner lease is no longer held for run {run_id}")

    def append_event(
        self,
        run_id: str,
        event_type: str,
        payload: Any,
        *,
        owner_id: str,
        token: str,
        now: float | None = None,
        sensitive: bool = False,
    ) -> EventRecord:
        """Append a non-sensitive event under a live owner lease.

        ``sensitive`` is explicit taint supplied by the trusted caller; the
        journal deliberately does not guess whether arbitrary payload text is
        secret.
        """

        _reject_sensitive(sensitive)
        with self._transaction():
            self._require_live_lease_locked(
                run_id, owner_id=owner_id, token=token, now=_timestamp(now)
            )
            return self._append_event_locked(run_id, event_type, payload)

    def append_event_with_checkpoint(
        self,
        run_id: str,
        event_type: str,
        payload: Any,
        checkpoint: Any,
        *,
        owner_id: str,
        token: str,
        now: float | None = None,
        expected_status: RunStatus | str | None = None,
        expected_desired_state: DesiredState | str | None = None,
        sensitive: bool = False,
    ) -> EventRecord:
        """Atomically append one event and replace the run checkpoint."""

        _reject_sensitive(sensitive)
        expected_status_value = (
            None if expected_status is None else _run_status(expected_status)
        )
        expected_desired_value = (
            None
            if expected_desired_state is None
            else _desired_state(expected_desired_state)
        )
        checkpoint_json = _encode_json(checkpoint, "checkpoint")
        now_value = _timestamp(now)
        with self._transaction():
            self._require_live_lease_locked(
                run_id, owner_id=owner_id, token=token, now=now_value
            )
            self._require_expected_run_state_locked(
                run_id, status=expected_status_value,
                desired_state=expected_desired_value,
            )
            event = self._append_event_locked(run_id, event_type, payload)
            cursor = self._connection.execute(
                """
                UPDATE runs SET checkpoint_json = ?, updated_at = ?
                WHERE run_id = ? AND owner_id = ? AND lease_token_hash = ?
                  AND lease_expires_at > ?
                  AND status NOT IN (
                    'succeeded','failed','timed_out','cancelled','unknown_effect'
                  )
                """,
                (checkpoint_json, _utc_now(), run_id, owner_id,
                 _lease_token_hash(token), now_value),
            )
            if cursor.rowcount != 1:
                self._raise_lease_lost(run_id)
            return event

    def _append_event_locked(
        self, run_id: str, event_type: str, payload: Any
    ) -> EventRecord:
        _require_nonempty_string(event_type, "event_type")
        if len(event_type) > 192:
            raise ValueError("event_type must not exceed 192 characters")
        if EVENT_TYPE_PATTERN.fullmatch(event_type) is None:
            raise ValueError(
                "event_type must be a lowercase dot, dash, or underscore qualified name"
            )
        payload_json = _encode_json(payload, "event payload")
        created_at = _utc_now()
        try:
            row = self._connection.execute(
                """
                INSERT INTO events (run_id, seq, event_type, payload_json, created_at)
                SELECT ?, COALESCE(MAX(seq), 0) + 1, ?, ?, ?
                FROM events
                WHERE run_id = ? AND EXISTS (SELECT 1 FROM runs WHERE run_id = ?)
                RETURNING seq
                """,
                (run_id, event_type, payload_json, created_at, run_id, run_id),
            ).fetchone()
        except sqlite3.IntegrityError as exc:
            if not self._run_exists(run_id):
                raise RunNotFoundError(f"run not found: {run_id}") from exc
            raise
        if row is None:
            raise RunNotFoundError(f"run not found: {run_id}")
        return EventRecord(
            run_id, int(row["seq"]), event_type, json.loads(payload_json), created_at
        )

    def list_events(
        self,
        run_id: str,
        *,
        after_seq: int = 0,
        limit: int = 1_000,
    ) -> list[EventRecord]:
        if isinstance(after_seq, bool) or after_seq < 0:
            raise ValueError("after_seq must be non-negative")
        if isinstance(limit, bool) or limit < 1 or limit > 10_000:
            raise ValueError("limit must be between 1 and 10000")
        if not self._run_exists(run_id):
            raise RunNotFoundError(f"run not found: {run_id}")
        rows = self._connection.execute(
            """
            SELECT run_id, seq, event_type, payload_json, created_at
            FROM events WHERE run_id = ? AND seq > ?
            ORDER BY seq ASC LIMIT ?
            """,
            (run_id, after_seq, limit),
        ).fetchall()
        return [
            EventRecord(
                row["run_id"], int(row["seq"]), row["event_type"],
                json.loads(row["payload_json"]), row["created_at"],
            )
            for row in rows
        ]

    def save_checkpoint(
        self,
        run_id: str,
        checkpoint: Any,
        *,
        owner_id: str,
        token: str,
        now: float | None = None,
        sensitive: bool = False,
    ) -> RunRecord:
        _reject_sensitive(sensitive)
        checkpoint_json = _encode_json(checkpoint, "checkpoint")
        now_value = _timestamp(now)
        cursor = self._connection.execute(
            """
            UPDATE runs SET checkpoint_json = ?, updated_at = ?
            WHERE run_id = ? AND owner_id = ? AND lease_token_hash = ?
              AND lease_expires_at > ?
              AND status NOT IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
            """,
            (checkpoint_json, _utc_now(), run_id, owner_id,
             _lease_token_hash(token), now_value),
        )
        if cursor.rowcount != 1:
            self._raise_lease_lost(run_id)
        return self.get_run(run_id)

    def _require_live_lease_locked(
        self, run_id: str, *, owner_id: str, token: str, now: float
    ) -> None:
        row = self._connection.execute(
            """
            SELECT status FROM runs
            WHERE run_id = ? AND owner_id = ? AND lease_token_hash = ?
              AND lease_expires_at > ?
              AND status NOT IN ('succeeded','failed','timed_out','cancelled','unknown_effect')
            """,
            (run_id, owner_id, _lease_token_hash(token), now),
        ).fetchone()
        if row is None:
            self._raise_lease_lost(run_id)

    def _require_expected_run_state_locked(
        self,
        run_id: str,
        *,
        status: RunStatus | None,
        desired_state: DesiredState | None,
    ) -> None:
        row = self._connection.execute(
            "SELECT status, desired_state FROM runs WHERE run_id = ?", (run_id,)
        ).fetchone()
        if row is None:
            raise RunNotFoundError(f"run not found: {run_id}")
        if status is not None and row["status"] != status.value:
            raise JournalConflictError(
                f"run status changed concurrently: expected {status.value}, "
                f"found {row['status']}"
            )
        if (
            desired_state is not None
            and row["desired_state"] != desired_state.value
        ):
            raise JournalConflictError(
                "desired state changed concurrently: expected "
                f"{desired_state.value}, found {row['desired_state']}"
            )

    def _raise_status_or_lease_conflict(
        self,
        run_id: str,
        *,
        expected: RunStatus,
        owner_id: str,
        token: str,
        now: float,
    ) -> None:
        row = self._connection.execute(
            "SELECT * FROM runs WHERE run_id = ?", (run_id,)
        ).fetchone()
        if row is None:
            raise RunNotFoundError(f"run not found: {run_id}")
        run = _run_from_row(row)
        if run.terminal:
            raise InvalidStateTransitionError(
                f"terminal run {run_id} is immutable ({run.status.value})"
            )
        if (
            row["owner_id"] != owner_id
            or row["lease_token_hash"] != _lease_token_hash(token)
            or row["lease_expires_at"] is None
            or float(row["lease_expires_at"]) <= now
        ):
            raise LeaseLostError(f"owner lease is no longer held for run {run_id}")
        raise JournalConflictError(
            f"run status changed concurrently: expected {expected.value}, "
            f"found {run.status.value}"
        )

    def _run_exists(self, run_id: str) -> bool:
        return (
            self._connection.execute(
                "SELECT 1 FROM runs WHERE run_id = ?", (run_id,)
            ).fetchone()
            is not None
        )


def _run_from_row(row: sqlite3.Row) -> RunRecord:
    return RunRecord(
        run_id=row["run_id"],
        workflow_name=row["workflow_name"],
        workflow_version=row["workflow_version"],
        plan_digest=row["plan_digest"],
        status=RunStatus(row["status"]),
        desired_state=DesiredState(row["desired_state"]),
        inputs=json.loads(row["inputs_json"]),
        output=_decode_optional_json(row["output_json"]),
        error=_decode_optional_json(row["error_json"]),
        checkpoint=_decode_optional_json(row["checkpoint_json"]),
        owner_id=row["owner_id"],
        lease_expires_at=row["lease_expires_at"],
        created_at=row["created_at"],
        updated_at=row["updated_at"],
        finished_at=row["finished_at"],
    )


def _encode_json(value: Any, field: str) -> str:
    try:
        _validate_json_value(value)
        encoded = json.dumps(
            value, ensure_ascii=False, allow_nan=False, separators=(",", ":"),
            sort_keys=True,
        )
        return encoded
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field} must be finite JSON data") from exc


def _validate_json_value(value: Any) -> None:
    """Reject Python-only values that ``json.dumps`` would silently coerce."""

    if value is None or type(value) in (str, bool, int):
        return
    if type(value) is float:
        if not math.isfinite(value):
            raise ValueError
        return
    if type(value) is list:
        for item in value:
            _validate_json_value(item)
        return
    if type(value) is dict:
        for key, item in value.items():
            if type(key) is not str:
                raise ValueError
            _validate_json_value(item)
        return
    raise ValueError


def _encode_optional_json(value: Any | None, field: str) -> str | None:
    return None if value is None else _encode_json(value, field)


def _decode_optional_json(value: str | None) -> Any | None:
    return None if value is None else json.loads(value)


def _require_nonempty_string(value: str, field: str) -> None:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{field} must be a non-empty string")


def _validate_optional_string(value: str | None, field: str, maximum: int) -> None:
    if value is None:
        return
    _require_nonempty_string(value, field)
    if len(value) > maximum:
        raise ValueError(f"{field} must not exceed {maximum} characters")


def _validate_status_transition(
    expected: RunStatus, status: RunStatus, *, output: Any | None, error: Any | None
) -> None:
    if status not in ALLOWED_STATUS_TRANSITIONS.get(expected, frozenset()):
        raise InvalidStateTransitionError(
            f"invalid run status transition: {expected.value} -> {status.value}"
        )
    if status not in TERMINAL_RUN_STATUSES and (output is not None or error is not None):
        raise InvalidStateTransitionError(
            "output and error may only be committed with a terminal status"
        )
    if status == RunStatus.SUCCEEDED and error is not None:
        raise InvalidStateTransitionError("a succeeded run cannot contain an error")
    if status != RunStatus.SUCCEEDED and status in TERMINAL_RUN_STATUSES:
        if output is not None or error is None:
            raise InvalidStateTransitionError(
                f"{status.value} requires an error and cannot contain output"
            )


def _validate_desired_state_transition(
    expected: DesiredState, desired: DesiredState
) -> None:
    if desired not in ALLOWED_DESIRED_STATE_TRANSITIONS[expected]:
        raise InvalidStateTransitionError(
            f"invalid desired state transition: {expected.value} -> {desired.value}"
        )


def _reject_sensitive(sensitive: bool) -> None:
    if not isinstance(sensitive, bool):
        raise ValueError("sensitive must be a boolean")
    if sensitive:
        raise SensitiveDataError("sensitive values cannot be written to the journal")


def _lease_token_hash(token: str) -> str:
    _require_nonempty_string(token, "token")
    return hashlib.sha256(token.encode("utf-8")).hexdigest()


def _run_status(value: RunStatus | str) -> RunStatus:
    try:
        return RunStatus(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"invalid run status: {value!r}") from exc


def _desired_state(value: DesiredState | str) -> DesiredState:
    try:
        return DesiredState(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"invalid desired state: {value!r}") from exc


def _positive_seconds(value: float) -> float:
    if isinstance(value, bool):
        raise ValueError("ttl_seconds must be a positive finite number")
    result = float(value)
    if not math.isfinite(result) or result <= 0:
        raise ValueError("ttl_seconds must be a positive finite number")
    return result


def _timestamp(value: float | None) -> float:
    result = time.time() if value is None else float(value)
    if not math.isfinite(result):
        raise ValueError("now must be finite")
    return result


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="microseconds")


__all__ = [
    "DEFAULT_BUSY_TIMEOUT_MS",
    "ALLOWED_STATUS_TRANSITIONS",
    "ALLOWED_DESIRED_STATE_TRANSITIONS",
    "SCHEMA_VERSION",
    "DesiredState",
    "EventRecord",
    "InvalidStateTransitionError",
    "JournalConflictError",
    "JournalError",
    "JournalStore",
    "LeaseConflictError",
    "LeaseLostError",
    "OwnerLease",
    "RunNotFoundError",
    "RunRecord",
    "RunStatus",
    "SensitiveDataError",
    "TERMINAL_RUN_STATUSES",
    "assert_durable_descriptor_eligible",
    "durable_descriptor_eligible",
]
