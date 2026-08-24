"""Contract tests for the crash-safe lifecycle journal."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

import jsonschema

from ai_auto_desktop.journal import (
    DesiredState,
    InvalidStateTransitionError,
    JournalConflictError,
    JournalError,
    JournalStore,
    LeaseConflictError,
    LeaseLostError,
    RunNotFoundError,
    RunStatus,
    SensitiveDataError,
    durable_descriptor_eligible,
)


PROJECT_ROOT = Path(__file__).resolve().parents[1]
RUNTIME_SCHEMAS = PROJECT_ROOT / "schemas" / "runtime" / "v1alpha1"


class JournalTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.path = Path(self.temporary.name) / "journal.sqlite3"
        self.store = JournalStore(self.path)
        self.addCleanup(self.store.close)

    def create(self, run_id: str = "run-1") -> object:
        return self.store.create_run(
            run_id=run_id,
            workflow_name="journal-contract",
            workflow_version="1.0.0",
            plan_digest="sha256:fixture",
            inputs={"name": "Ada"},
            descriptor={
                "inputs": {"name": {"schema": {"type": "string"}}},
                "outputs": {},
            },
        )

    def claim(
        self, run_id: str = "run-1", *, owner_id: str = "worker-a", now: float = 1
    ) -> object:
        return self.store.claim_owner(
            run_id, owner_id=owner_id, ttl_seconds=1_000_000_000, now=now
        )

    def test_configures_durability_and_schema_version(self) -> None:
        connection = sqlite3.connect(self.path)
        self.addCleanup(connection.close)
        self.assertEqual(connection.execute("PRAGMA journal_mode").fetchone()[0], "wal")
        self.assertEqual(connection.execute("PRAGMA synchronous").fetchone()[0], 2)
        self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 1)
        self.assertEqual(self.store._connection.execute("PRAGMA foreign_keys").fetchone()[0], 1)
        self.assertEqual(self.store._connection.execute("PRAGMA busy_timeout").fetchone()[0], 5_000)

    def test_refuses_newer_schema_without_modifying_it(self) -> None:
        other = Path(self.temporary.name) / "future.sqlite3"
        connection = sqlite3.connect(other)
        connection.execute("PRAGMA user_version = 99")
        connection.close()
        with self.assertRaises(JournalError):
            JournalStore(other)
        connection = sqlite3.connect(other)
        self.addCleanup(connection.close)
        self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 99)

    def test_concurrent_first_open_migrates_exactly_once(self) -> None:
        concurrent_path = Path(self.temporary.name) / "concurrent.sqlite3"

        def open_and_close(_: int) -> int:
            with JournalStore(concurrent_path) as store:
                return store._connection.execute("PRAGMA user_version").fetchone()[0]

        with ThreadPoolExecutor(max_workers=6) as executor:
            versions = list(executor.map(open_and_close, range(12)))
        self.assertEqual(versions, [1] * 12)

    def test_create_get_and_list_runs_preserve_json(self) -> None:
        created = self.create()
        self.assertEqual(created.status, RunStatus.PENDING)
        self.assertEqual(created.desired_state, DesiredState.RUN)
        self.assertEqual(created.inputs, {"name": "Ada"})
        self.assertFalse(created.terminal)

        second = self.store.create_run(
            run_id="run-2", workflow_name="second", inputs=[1, 2],
            descriptor={"inputs": {}, "outputs": {}},
        )
        self.assertEqual(self.store.get_run("run-2"), second)
        self.assertEqual({run.run_id for run in self.store.list_runs()}, {"run-1", "run-2"})
        self.assertEqual(
            [run.run_id for run in self.store.list_runs(status=RunStatus.PENDING)],
            [run.run_id for run in self.store.list_runs()],
        )
        with self.assertRaises(JournalConflictError):
            self.create()
        with self.assertRaises(RunNotFoundError):
            self.store.get_run("missing")

    def test_rejects_non_json_values_before_crossing_durable_boundary(self) -> None:
        with self.assertRaises(ValueError):
            self.store.create_run(
                run_id="bad", workflow_name="bad-json", inputs={"bad": object()},
                descriptor={"inputs": {}, "outputs": {}},
            )
        self.create()
        lease = self.claim()
        with self.assertRaises(ValueError):
            self.store.append_event(
                "run-1", "step.started", float("nan"),
                owner_id=lease.owner_id, token=lease.token, now=2,
            )
        self.assertEqual(self.store.list_events("run-1"), [])

    def test_sensitive_input_or_output_definition_is_not_durable(self) -> None:
        plain = {"inputs": {}, "outputs": {}}
        sensitive_input = {"inputs": {"token": {"sensitive": True}}}
        sensitive_output = {"outputs": {"secret": {"sensitive": True}}}
        self.assertTrue(durable_descriptor_eligible(plain))
        self.assertFalse(durable_descriptor_eligible(sensitive_input))
        self.assertFalse(durable_descriptor_eligible(sensitive_output))
        for descriptor in (sensitive_input, sensitive_output):
            with self.subTest(descriptor=descriptor), self.assertRaises(SensitiveDataError):
                self.store.create_run(
                    run_id="secret",
                    workflow_name="sensitive",
                    inputs={},
                    descriptor=descriptor,
                )
        self.assertEqual(self.store.list_runs(), [])

    def test_desired_state_is_compare_and_set_and_terminal_runs_reject_it(self) -> None:
        self.create()
        lease = self.claim()
        paused = self.store.compare_and_set_desired_state(
            "run-1", expected=DesiredState.RUN, desired=DesiredState.PAUSE,
            owner_id=lease.owner_id, token=lease.token, now=2,
        )
        self.assertEqual(paused.desired_state, DesiredState.PAUSE)
        with self.assertRaises(JournalConflictError):
            self.store.compare_and_set_desired_state(
                "run-1", expected=DesiredState.RUN, desired=DesiredState.CANCEL,
                owner_id=lease.owner_id, token=lease.token, now=2,
            )
        self.store.set_status(
            "run-1", expected=RunStatus.PENDING, status=RunStatus.RUNNING,
            owner_id=lease.owner_id, token=lease.token, now=2,
        )
        self.store.set_status(
            "run-1", expected=RunStatus.RUNNING, status=RunStatus.SUCCEEDED,
            owner_id=lease.owner_id, token=lease.token, now=3, output={"ok": True}
        )
        for desired in DesiredState:
            with self.subTest(desired=desired), self.assertRaises(InvalidStateTransitionError):
                self.store.compare_and_set_desired_state(
                    "run-1", expected=DesiredState.PAUSE, desired=desired,
                    owner_id=lease.owner_id, token=lease.token, now=4,
                )

    def test_terminal_status_is_irreversible_and_releases_owner(self) -> None:
        self.create()
        lease = self.claim()
        self.store.set_status(
            "run-1", expected=RunStatus.PENDING, status=RunStatus.RUNNING,
            owner_id=lease.owner_id, token=lease.token, now=2,
        )
        terminal = self.store.set_status(
            "run-1",
            expected=RunStatus.RUNNING,
            status=RunStatus.FAILED,
            owner_id=lease.owner_id,
            token=lease.token,
            now=3,
            error={"code": "FIXTURE"},
        )
        self.assertTrue(terminal.terminal)
        self.assertIsNotNone(terminal.finished_at)
        self.assertIsNone(terminal.owner_id)
        self.assertEqual(terminal.error, {"code": "FIXTURE"})
        with self.assertRaises(InvalidStateTransitionError):
            self.store.set_status(
                "run-1", expected=RunStatus.FAILED, status=RunStatus.RUNNING,
                owner_id=lease.owner_id, token=lease.token, now=4,
            )
        with self.assertRaises(InvalidStateTransitionError):
            self.store.release_owner("run-1", owner_id="worker-a", token=lease.token)
        with self.assertRaises(InvalidStateTransitionError):
            self.store.claim_owner("run-1", owner_id="worker-b", ttl_seconds=10)

        # The database invariant also protects against a future caller that
        # bypasses the Python CAS method.
        with self.assertRaises(sqlite3.IntegrityError):
            self.store._connection.execute(
                "UPDATE runs SET status = 'running', finished_at = NULL WHERE run_id = ?",
                ("run-1",),
            )

    def test_pause_transition_atomically_releases_owner_lease(self) -> None:
        self.create()
        lease = self.claim()
        self.store.set_status(
            "run-1", expected=RunStatus.PENDING, status=RunStatus.RUNNING,
            owner_id=lease.owner_id, token=lease.token, now=2,
        )
        paused, event = self.store.set_status_with_event(
            "run-1", expected=RunStatus.RUNNING, status=RunStatus.PAUSED,
            owner_id=lease.owner_id, token=lease.token, now=3,
            event_type="run.paused", event_payload={},
        )
        self.assertEqual(paused.status, RunStatus.PAUSED)
        self.assertIsNone(paused.owner_id)
        self.assertIsNone(paused.lease_expires_at)
        self.assertEqual(event.seq, 1)
        with self.assertRaises(LeaseLostError):
            self.store.append_event(
                "run-1", "run.safe_point", {}, owner_id=lease.owner_id,
                token=lease.token, now=4,
            )

    def test_owner_lease_requires_token_and_can_only_be_stolen_after_expiry(self) -> None:
        self.create()
        first = self.store.claim_owner("run-1", owner_id="worker-a", ttl_seconds=5, now=100)
        self.assertGreaterEqual(len(first.token), 32)
        with self.assertRaises(LeaseConflictError):
            self.store.claim_owner("run-1", owner_id="worker-b", ttl_seconds=5, now=104)
        with self.assertRaises(LeaseLostError):
            self.store.heartbeat_owner(
                "run-1", owner_id="worker-a", token="wrong", ttl_seconds=5, now=101
            )
        renewed = self.store.heartbeat_owner(
            "run-1", owner_id="worker-a", token=first.token, ttl_seconds=8, now=102
        )
        self.assertEqual(renewed.expires_at, 110)
        second = self.store.claim_owner("run-1", owner_id="worker-b", ttl_seconds=5, now=110)
        self.assertNotEqual(second.token, first.token)
        with self.assertRaises(LeaseLostError):
            self.store.release_owner("run-1", owner_id="worker-a", token=first.token)
        released = self.store.release_owner(
            "run-1", owner_id="worker-b", token=second.token, now=111
        )
        self.assertIsNone(released.owner_id)

    def test_expired_owner_is_fenced_from_every_runner_mutation(self) -> None:
        self.create()
        stale = self.store.claim_owner(
            "run-1", owner_id="worker-a", ttl_seconds=5, now=100
        )
        current = self.store.claim_owner(
            "run-1", owner_id="worker-b", ttl_seconds=10, now=105
        )
        attempts = (
            lambda: self.store.set_status(
                "run-1", expected=RunStatus.PENDING, status=RunStatus.RUNNING,
                owner_id=stale.owner_id, token=stale.token, now=106,
            ),
            lambda: self.store.append_event(
                "run-1", "step.started", {}, owner_id=stale.owner_id,
                token=stale.token, now=106,
            ),
            lambda: self.store.save_checkpoint(
                "run-1", {}, owner_id=stale.owner_id, token=stale.token, now=106
            ),
            lambda: self.store.append_event_with_checkpoint(
                "run-1", "checkpoint.saved", {}, {}, owner_id=stale.owner_id,
                token=stale.token, now=106,
            ),
        )
        for attempt in attempts:
            with self.subTest(attempt=attempt), self.assertRaises(LeaseLostError):
                attempt()
        self.assertEqual(self.store.list_events("run-1"), [])
        self.assertIsNone(self.store.get_run("run-1").checkpoint)
        self.store.save_checkpoint(
            "run-1", {"owner": "b"}, owner_id=current.owner_id,
            token=current.token, now=106,
        )
        requested = self.store.compare_and_set_desired_state(
            "run-1", expected=DesiredState.RUN, desired=DesiredState.PAUSE
        )
        self.assertEqual(requested.desired_state, DesiredState.PAUSE)

    def test_public_run_records_never_expose_bearer_token(self) -> None:
        self.create()
        lease = self.claim()
        run = self.store.get_run("run-1")
        self.assertNotIn("token", repr(run))
        self.assertNotIn(lease.token, repr(run))
        self.assertNotIn("token", run.to_dict()["ownerLease"])
        raw = self.store._connection.execute(
            "SELECT lease_token_hash FROM runs WHERE run_id = 'run-1'"
        ).fetchone()[0]
        self.assertNotEqual(raw, lease.token)
        self.assertEqual(len(raw), 64)

    def test_store_instance_has_explicit_single_thread_contract(self) -> None:
        self.create()
        with ThreadPoolExecutor(max_workers=1) as executor:
            future = executor.submit(self.store.get_run, "run-1")
            with self.assertRaises(sqlite3.ProgrammingError):
                future.result()

    def test_status_transition_matrix_and_result_shape_are_enforced(self) -> None:
        self.create()
        lease = self.claim()
        invalid = (
            (RunStatus.PENDING, RunStatus.PAUSED, None, None),
            (RunStatus.PENDING, RunStatus.PENDING, None, None),
            (RunStatus.PENDING, RunStatus.SUCCEEDED, None, None),
            (RunStatus.PENDING, RunStatus.FAILED, None, None),
        )
        for expected, status, output, error in invalid:
            with self.subTest(status=status), self.assertRaises(InvalidStateTransitionError):
                self.store.set_status(
                    "run-1", expected=expected, status=status,
                    owner_id=lease.owner_id, token=lease.token, now=2,
                    output=output, error=error,
                )
        self.assertEqual(self.store.get_run("run-1").status, RunStatus.PENDING)

    def test_create_and_status_event_combinations_are_atomic(self) -> None:
        run, created = self.store.create_run_with_event(
            run_id="atomic", workflow_name="atomic", inputs={},
            descriptor={"inputs": {}, "outputs": {}},
            event_type="run.created", event_payload={},
        )
        self.assertEqual((run.run_id, created.seq), ("atomic", 1))
        lease = self.store.claim_owner("atomic", owner_id="worker", ttl_seconds=10, now=1)
        transitioned, event = self.store.set_status_with_event(
            "atomic", expected=RunStatus.PENDING, status=RunStatus.RUNNING,
            owner_id=lease.owner_id, token=lease.token, now=2,
            event_type="run.started", event_payload={},
            checkpoint={"phase": "between_top_level_steps"},
        )
        self.assertEqual(transitioned.status, RunStatus.RUNNING)
        self.assertEqual(
            transitioned.checkpoint, {"phase": "between_top_level_steps"}
        )
        self.assertEqual(event.seq, 2)
        with self.assertRaises(InvalidStateTransitionError):
            self.store.set_status_with_event(
                "atomic", expected=RunStatus.RUNNING, status=RunStatus.SUCCEEDED,
                owner_id=lease.owner_id, token=lease.token, now=3,
                event_type="run.finished", event_payload={}, error={"bad": True},
            )
        self.assertEqual([item.seq for item in self.store.list_events("atomic")], [1, 2])
        self.assertEqual(
            self.store.get_run("atomic").checkpoint,
            {"phase": "between_top_level_steps"},
        )

    def test_desired_state_and_control_event_are_atomic(self) -> None:
        self.create()
        lease = self.claim()
        updated, event = self.store.compare_and_set_desired_state_with_event(
            "run-1", expected=DesiredState.RUN, desired=DesiredState.PAUSE,
            event_type="control.pause_requested", event_payload={"source": "user"},
        )
        self.assertEqual(updated.desired_state, DesiredState.PAUSE)
        self.assertEqual(updated.owner_id, lease.owner_id)
        self.assertEqual(event.seq, 1)
        with self.assertRaises(JournalConflictError):
            self.store.compare_and_set_desired_state_with_event(
                "run-1", expected=DesiredState.RUN, desired=DesiredState.CANCEL,
                event_type="control.cancel_requested", event_payload={},
            )
        self.assertEqual([item.seq for item in self.store.list_events("run-1")], [1])

    def test_sensitive_flag_and_strict_json_round_trip_fail_closed(self) -> None:
        self.create()
        lease = self.claim()
        with self.assertRaises(SensitiveDataError):
            self.store.append_event(
                "run-1", "step.started", {}, owner_id=lease.owner_id,
                token=lease.token, now=2, sensitive=True,
            )
        with self.assertRaises(SensitiveDataError):
            self.store.save_checkpoint(
                "run-1", {}, owner_id=lease.owner_id, token=lease.token,
                now=2, sensitive=True,
            )
        with self.assertRaises(ValueError):
            self.store.append_event(
                "run-1", "step.started", (1, 2), owner_id=lease.owner_id,
                token=lease.token, now=2,
            )
        with self.assertRaises(SensitiveDataError):
            self.store.set_status(
                "run-1", expected=RunStatus.PENDING, status=RunStatus.RUNNING,
                owner_id=lease.owner_id, token=lease.token, now=2, sensitive=True,
            )

    def test_length_and_finite_lease_boundaries_fail_closed(self) -> None:
        descriptor = {"inputs": {}, "outputs": {}}
        with self.assertRaises(ValueError):
            self.store.create_run(
                run_id="x" * 257, workflow_name="valid", inputs={},
                descriptor=descriptor,
            )
        self.create()
        with self.assertRaises(ValueError):
            self.store.claim_owner(
                "run-1", owner_id="x" * 257, ttl_seconds=1
            )
        with self.assertRaises(ValueError):
            self.store.claim_owner(
                "run-1", owner_id="owner", ttl_seconds=1e308, now=1e308
            )

    def test_terminal_run_rejects_checkpoint_and_new_events(self) -> None:
        self.create()
        lease = self.claim()
        self.store.set_status(
            "run-1", expected=RunStatus.PENDING, status=RunStatus.CANCELLED,
            owner_id=lease.owner_id, token=lease.token, now=2,
            error={"code": "WORKFLOW.CANCELLED"},
        )
        with self.assertRaises(InvalidStateTransitionError):
            self.store.save_checkpoint(
                "run-1", {}, owner_id=lease.owner_id, token=lease.token, now=3
            )
        with self.assertRaises(InvalidStateTransitionError):
            self.store.append_event(
                "run-1", "run.changed", {}, owner_id=lease.owner_id,
                token=lease.token, now=3,
            )

    def test_events_have_per_run_contiguous_sequence(self) -> None:
        self.create()
        self.store.create_run(
            run_id="run-2", workflow_name="two", inputs={},
            descriptor={"inputs": {}, "outputs": {}},
        )
        lease1 = self.claim()
        lease2 = self.claim("run-2", owner_id="worker-b")
        first = self.store.append_event(
            "run-1", "run.created", {"n": 1},
            owner_id=lease1.owner_id, token=lease1.token, now=2,
        )
        second = self.store.append_event(
            "run-1", "step.started", {"n": 2},
            owner_id=lease1.owner_id, token=lease1.token, now=2,
        )
        other = self.store.append_event(
            "run-2", "run.created", {}, owner_id=lease2.owner_id,
            token=lease2.token, now=2,
        )
        self.assertEqual((first.seq, second.seq, other.seq), (1, 2, 1))
        self.assertEqual(
            [event.seq for event in self.store.list_events("run-1")], [1, 2]
        )
        self.assertEqual(
            [event.seq for event in self.store.list_events("run-1", after_seq=1)], [2]
        )
        with self.assertRaises(RunNotFoundError):
            self.store.append_event(
                "missing", "run.created", {}, owner_id=lease1.owner_id,
                token=lease1.token, now=2,
            )
        with self.assertRaises(sqlite3.IntegrityError):
            self.store._connection.execute(
                """
                INSERT INTO events (run_id, seq, event_type, payload_json, created_at)
                VALUES (?, ?, ?, ?, ?)
                """,
                ("run-1", 4, "step.finished", "{}", first.created_at),
            )

    def test_event_sequence_remains_contiguous_across_connections(self) -> None:
        self.create()
        lease = self.claim()

        def append(index: int) -> int:
            with JournalStore(self.path) as store:
                return store.append_event(
                    "run-1", "step.finished", {"i": index},
                    owner_id=lease.owner_id, token=lease.token, now=2,
                ).seq

        with ThreadPoolExecutor(max_workers=8) as executor:
            seqs = list(executor.map(append, range(40)))
        self.assertEqual(sorted(seqs), list(range(1, 41)))
        self.assertEqual(
            [event.seq for event in self.store.list_events("run-1")], list(range(1, 41))
        )

    def test_event_and_checkpoint_commit_atomically(self) -> None:
        self.create()
        lease = self.claim()
        event = self.store.append_event_with_checkpoint(
            "run-1", "checkpoint.saved", {"step": "a"}, {"next": "b"},
            owner_id=lease.owner_id, token=lease.token, now=2,
        )
        self.assertEqual(event.seq, 1)
        self.assertEqual(self.store.get_run("run-1").checkpoint, {"next": "b"})

        with self.assertRaises(ValueError):
            self.store.append_event_with_checkpoint(
                "run-1", "checkpoint.saved", {"step": "b"}, object(),
                owner_id=lease.owner_id, token=lease.token, now=2,
            )
        self.assertEqual([item.seq for item in self.store.list_events("run-1")], [1])
        self.store.save_checkpoint(
            "run-1", {"next": None}, owner_id=lease.owner_id,
            token=lease.token, now=2,
        )
        self.assertEqual(self.store.get_run("run-1").checkpoint, {"next": None})

    def test_runtime_json_schemas_are_valid_and_canonical_copies_match(self) -> None:
        valid_documents = {
            "run.schema.json": {
                "apiVersion": "ai-auto-desktop.dev/v1alpha1",
                "kind": "Run",
                "runId": "run-1",
                "workflow": {"name": "fixture"},
                "status": "pending",
                "desiredState": "run",
                "inputs": {},
                "createdAt": "2026-08-25T00:00:00Z",
                "updatedAt": "2026-08-25T00:00:00Z",
                "finishedAt": None,
            },
            "event.schema.json": {
                "apiVersion": "ai-auto-desktop.dev/v1alpha1",
                "kind": "RunEvent",
                "runId": "run-1",
                "seq": 1,
                "type": "run.created",
                "payload": {},
                "createdAt": "2026-08-25T00:00:00Z",
            },
        }
        for filename, document in valid_documents.items():
            with self.subTest(filename=filename):
                canonical = RUNTIME_SCHEMAS / filename
                packaged = (
                    PROJECT_ROOT / "src" / "ai_auto_desktop" / "schemas"
                    / "runtime" / "v1alpha1" / filename
                )
                self.assertEqual(canonical.read_bytes(), packaged.read_bytes())
                schema = json.loads(canonical.read_text(encoding="utf-8"))
                jsonschema.Draft202012Validator.check_schema(schema)
                jsonschema.Draft202012Validator(schema).validate(document)

        self.create()
        lease = self.store.claim_owner(
            "run-1", owner_id="worker-a", ttl_seconds=10, now=1
        )
        run_validator = jsonschema.Draft202012Validator(
            json.loads((RUNTIME_SCHEMAS / "run.schema.json").read_text())
        )
        run_validator.validate(self.store.get_run("run-1").to_dict())
        event_validator = jsonschema.Draft202012Validator(
            json.loads((RUNTIME_SCHEMAS / "event.schema.json").read_text())
        )
        event_validator.validate(
            self.store.append_event(
                "run-1", "lease.claimed", {"owner": lease.owner_id},
                owner_id=lease.owner_id, token=lease.token, now=2,
            ).to_dict()
        )


if __name__ == "__main__":
    unittest.main()
