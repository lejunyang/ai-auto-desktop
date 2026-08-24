"""Contract tests for the persistent run lifecycle service."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from ai_auto_desktop.journal import (
    DesiredState,
    JournalStore,
    OwnerLease,
    RunStatus,
)
from ai_auto_desktop.run_service import (
    DispatchState,
    RunService,
    RunServiceError,
)


PLAIN_DESCRIPTOR = {
    "inputs": {"name": {"schema": {"type": "string"}}},
    "outputs": {},
}


class RunServiceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.path = Path(self.temporary.name) / "journal.sqlite3"
        self.store = JournalStore(self.path)
        self.addCleanup(self.store.close)
        self.service = RunService(self.store)

    def create(self, run_id: str = "run-1") -> object:
        return self.service.create(
            run_id=run_id,
            workflow_name="service-contract",
            workflow_version="1.0.0",
            plan_digest="sha256:fixture",
            inputs={"name": "Ada"},
            descriptor=PLAIN_DESCRIPTOR,
        )

    def start(
        self,
        run_id: str = "run-1",
        *,
        owner_id: str = "worker-a",
        now: float = 100,
        ttl_seconds: float = 10_000_000_000,
    ) -> OwnerLease:
        lease = self.store.claim_owner(
            run_id, owner_id=owner_id, ttl_seconds=ttl_seconds, now=now
        )
        self.store.set_status_with_event(
            run_id,
            expected=RunStatus.PENDING,
            status=RunStatus.RUNNING,
            owner_id=lease.owner_id,
            token=lease.token,
            now=now + 1,
            event_type="run.started",
            event_payload={},
        )
        return lease

    def test_create_get_list_and_events_use_persistent_records(self) -> None:
        created = self.create()
        self.create("run-2")

        self.assertEqual(self.service.get("run-1"), created)
        self.assertEqual(
            {run.run_id for run in self.service.list()}, {"run-1", "run-2"}
        )
        self.assertEqual(
            [event.event_type for event in self.service.events("run-1")],
            ["run.created"],
        )
        self.assertEqual(self.service.events("run-1", after_seq=1), [])

    def test_service_errors_are_structured_and_do_not_expose_lease_tokens(self) -> None:
        with self.assertRaises(RunServiceError) as missing:
            self.service.get("missing")
        self.assertEqual(missing.exception.code, "RUN.NOT_FOUND")
        self.assertEqual(missing.exception.category, "run")
        self.assertEqual(missing.exception.details["operation"], "get")
        self.assertEqual(missing.exception.details["runId"], "missing")

        self.create()
        lease = self.start()
        with self.assertRaises(RunServiceError) as fenced:
            self.service.runner_safe_point(
                "run-1", owner_id=lease.owner_id, token="wrong-token", now=102
            )
        self.assertEqual(fenced.exception.code, "RUN.LEASE_LOST")
        self.assertNotIn(lease.token, str(fenced.exception.to_dict()))

    def test_create_passes_descriptor_sensitivity_and_fails_closed(self) -> None:
        sensitive = {
            "inputs": {
                "password": {
                    "schema": {"type": "string"},
                    "sensitive": True,
                }
            },
            "outputs": {},
        }
        with mock.patch.object(
            self.store,
            "create_run_with_event",
            wraps=self.store.create_run_with_event,
        ) as create:
            with self.assertRaises(RunServiceError) as rejected:
                self.service.create(
                    run_id="secret",
                    workflow_name="secret",
                    inputs={"password": "do-not-store"},
                    descriptor=sensitive,
                )
        self.assertEqual(rejected.exception.code, "RUN.SENSITIVE_DATA")
        self.assertIs(create.call_args.kwargs["sensitive"], True)
        self.assertEqual(self.store.list_runs(), [])

        # Unknown descriptor definition shapes are also non-durable.  This is
        # declaration enforcement; the service does not guess secrets by value.
        with self.assertRaises(RunServiceError) as malformed:
            self.service.create(
                run_id="unknown-shape",
                workflow_name="unknown-shape",
                inputs={},
                descriptor={"inputs": None, "outputs": {}},
            )
        self.assertEqual(malformed.exception.code, "RUN.SENSITIVE_DATA")
        self.assertEqual(self.store.list_runs(), [])

    def test_pause_and_resume_take_effect_only_at_runner_safe_points(self) -> None:
        self.create()
        lease = self.start()

        requested = self.service.request_pause("run-1")
        self.assertEqual(requested.status, RunStatus.RUNNING)
        self.assertEqual(requested.desired_state, DesiredState.PAUSE)
        paused = self.service.runner_safe_point(
            "run-1", owner_id=lease.owner_id, token=lease.token, now=102
        )
        self.assertEqual(paused.status, RunStatus.PAUSED)
        self.assertEqual(paused.desired_state, DesiredState.PAUSE)
        self.assertIsNone(paused.owner_id)

        requested = self.service.request_resume("run-1")
        resume_lease = self.store.claim_owner(
            "run-1", owner_id="worker-resume", ttl_seconds=100, now=103
        )
        self.assertEqual(requested.status, RunStatus.PAUSED)
        self.assertEqual(requested.desired_state, DesiredState.RUN)
        running = self.service.runner_safe_point(
            "run-1",
            owner_id=resume_lease.owner_id,
            token=resume_lease.token,
            now=104,
        )
        self.assertEqual(running.status, RunStatus.RUNNING)
        self.assertEqual(running.desired_state, DesiredState.RUN)
        self.assertEqual(
            [event.event_type for event in self.service.events("run-1")],
            [
                "run.created",
                "run.started",
                "run.pause_requested",
                "run.paused",
                "run.resume_requested",
                "run.resumed",
            ],
        )

    def test_repeated_control_requests_are_read_only_idempotent(self) -> None:
        self.create()
        self.start()

        first = self.service.request_pause("run-1")
        second = self.service.request_pause("run-1")
        self.assertEqual(second, first)
        self.assertEqual(
            [
                event.event_type
                for event in self.service.events("run-1")
                if event.event_type == "run.pause_requested"
            ],
            ["run.pause_requested"],
        )

        resumed = self.service.request_resume("run-1")
        self.assertEqual(self.service.request_resume("run-1"), resumed)
        self.assertEqual(
            [
                event.event_type
                for event in self.service.events("run-1")
                if event.event_type == "run.resume_requested"
            ],
            ["run.resume_requested"],
        )

        cancelled = self.service.request_cancel("run-1")
        self.assertEqual(self.service.request_cancel("run-1"), cancelled)
        self.assertEqual(
            [
                event.event_type
                for event in self.service.events("run-1")
                if event.event_type == "run.cancel_requested"
            ],
            ["run.cancel_requested"],
        )

    def test_cancel_intent_is_sticky_and_only_runner_commits_terminal_state(self) -> None:
        self.create()
        lease = self.start()

        requested = self.service.request_cancel("run-1")
        self.assertEqual(requested.status, RunStatus.RUNNING)
        self.assertEqual(requested.desired_state, DesiredState.CANCEL)
        with self.assertRaises(RunServiceError) as resume:
            self.service.request_resume("run-1")
        self.assertEqual(resume.exception.code, "RUN.CANCEL_PENDING")
        with self.assertRaises(RunServiceError) as pause:
            self.service.request_pause("run-1")
        self.assertEqual(pause.exception.code, "RUN.CANCEL_PENDING")

        cancelled = self.service.runner_safe_point(
            "run-1", owner_id=lease.owner_id, token=lease.token, now=102
        )
        self.assertEqual(cancelled.status, RunStatus.CANCELLED)
        self.assertEqual(cancelled.error["code"], "RUN.CANCELLED")
        self.assertIsNone(cancelled.owner_id)

    def test_ambiguous_post_dispatch_cancel_is_unknown_effect(self) -> None:
        self.create()
        lease = self.start()
        self.service.request_cancel("run-1")

        terminal = self.service.runner_safe_point(
            "run-1",
            owner_id=lease.owner_id,
            token=lease.token,
            now=102,
            dispatch_state=DispatchState.EFFECT_UNKNOWN,
        )
        self.assertEqual(terminal.status, RunStatus.UNKNOWN_EFFECT)
        self.assertEqual(terminal.error["code"], "RUN.UNKNOWN_EFFECT")
        self.assertEqual(terminal.error["effect"], "unknown")
        self.assertIn(
            "cancellation cannot be proven", terminal.error["message"]
        )
        self.assertEqual(
            self.service.events("run-1")[-1].event_type,
            "run.effect_unknown",
        )

    def test_terminal_runs_fail_closed_for_every_control_operation(self) -> None:
        self.create()
        lease = self.start()
        self.store.set_status_with_event(
            "run-1",
            expected=RunStatus.RUNNING,
            status=RunStatus.SUCCEEDED,
            owner_id=lease.owner_id,
            token=lease.token,
            now=102,
            output={"ok": True},
            event_type="run.succeeded",
            event_payload={},
        )

        operations = (
            self.service.request_pause,
            self.service.request_resume,
            self.service.request_cancel,
            lambda run_id: self.service.runner_safe_point(
                run_id, owner_id=lease.owner_id, token=lease.token, now=103
            ),
        )
        for operation in operations:
            with self.subTest(operation=operation), self.assertRaises(
                RunServiceError
            ) as failed:
                operation("run-1")
            self.assertEqual(failed.exception.code, "RUN.TERMINAL")
        self.assertEqual(self.service.get("run-1").status, RunStatus.SUCCEEDED)

    def test_expired_and_stolen_leases_are_fenced_at_safe_point(self) -> None:
        self.create()
        stale = self.start(ttl_seconds=5)
        self.service.request_pause("run-1")

        current = self.store.claim_owner(
            "run-1", owner_id="worker-b", ttl_seconds=10, now=105
        )
        before = [event.to_dict() for event in self.service.events("run-1")]
        with self.assertRaises(RunServiceError) as expired:
            self.service.runner_safe_point(
                "run-1",
                owner_id=stale.owner_id,
                token=stale.token,
                now=106,
            )
        self.assertEqual(expired.exception.code, "RUN.LEASE_LOST")
        self.assertEqual(self.service.get("run-1").status, RunStatus.RUNNING)
        self.assertEqual(
            [event.to_dict() for event in self.service.events("run-1")], before
        )

        paused = self.service.runner_safe_point(
            "run-1",
            owner_id=current.owner_id,
            token=current.token,
            now=106,
        )
        self.assertEqual(paused.status, RunStatus.PAUSED)

    def test_operator_control_is_independent_of_live_runner_lease(self) -> None:
        self.create()
        lease = self.start()

        requested = self.service.request_pause("run-1")
        self.assertEqual(requested.desired_state, DesiredState.PAUSE)
        self.assertEqual(requested.owner_id, lease.owner_id)
        self.assertEqual(
            self.service.events("run-1")[-1].event_type,
            "run.pause_requested",
        )

        paused = self.service.runner_safe_point(
            "run-1", owner_id=lease.owner_id, token=lease.token, now=106
        )
        self.assertEqual(paused.status, RunStatus.PAUSED)
        self.assertIsNone(paused.owner_id)

    def test_concurrent_duplicate_pause_writes_one_request_event(self) -> None:
        self.create()
        self.start()

        def pause(_: int) -> DesiredState:
            with JournalStore(self.path) as store:
                return RunService(store).request_pause("run-1").desired_state

        with ThreadPoolExecutor(max_workers=8) as executor:
            states = list(executor.map(pause, range(24)))
        self.assertEqual(states, [DesiredState.PAUSE] * 24)
        self.assertEqual(
            [
                event.event_type
                for event in self.service.events("run-1")
                if event.event_type == "run.pause_requested"
            ],
            ["run.pause_requested"],
        )

    def test_concurrent_pause_and_cancel_converge_to_sticky_cancel(self) -> None:
        self.create()
        self.start()

        def control(index: int) -> str:
            with JournalStore(self.path) as store:
                service = RunService(store)
                try:
                    run = (
                        service.request_cancel("run-1")
                        if index % 2
                        else service.request_pause("run-1")
                    )
                    return run.desired_state.value
                except RunServiceError as exc:
                    return exc.code

        with ThreadPoolExecutor(max_workers=8) as executor:
            outcomes = list(executor.map(control, range(24)))
        self.assertIn(DesiredState.CANCEL.value, outcomes)
        self.assertEqual(
            set(outcomes) - {"pause", "cancel"}, {"RUN.CANCEL_PENDING"}
            if "RUN.CANCEL_PENDING" in outcomes
            else set(),
        )
        self.assertEqual(
            self.service.get("run-1").desired_state, DesiredState.CANCEL
        )
        events = [event.event_type for event in self.service.events("run-1")]
        self.assertEqual(events.count("run.cancel_requested"), 1)
        self.assertLessEqual(events.count("run.pause_requested"), 1)

    def test_resume_racing_pause_application_requires_a_fresh_owner(self) -> None:
        self.create()
        lease = self.start()
        self.service.request_pause("run-1")
        original = self.store.set_status_with_event

        def pause_then_resume(*args: object, **kwargs: object) -> object:
            transitioned, event = original(*args, **kwargs)
            if kwargs.get("status") is RunStatus.PAUSED:
                with JournalStore(self.path) as store:
                    RunService(store).request_resume("run-1")
                transitioned = self.store.get_run("run-1")
            return transitioned, event

        with mock.patch.object(
            self.store, "set_status_with_event", side_effect=pause_then_resume
        ) as transition:
            paused = self.service.runner_safe_point(
                "run-1", owner_id=lease.owner_id, token=lease.token, now=102
            )
        self.assertEqual(transition.call_count, 1)
        self.assertEqual(paused.status, RunStatus.PAUSED)
        self.assertEqual(paused.desired_state, DesiredState.RUN)
        self.assertIsNone(paused.owner_id)

        resumed_by = self.store.claim_owner(
            "run-1", owner_id="worker-b", ttl_seconds=100, now=103
        )
        running = self.service.runner_safe_point(
            "run-1",
            owner_id=resumed_by.owner_id,
            token=resumed_by.token,
            now=104,
        )
        self.assertEqual(running.status, RunStatus.RUNNING)


if __name__ == "__main__":
    unittest.main()
