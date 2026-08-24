"""End-to-end contracts for durable segmented workflow execution."""

from __future__ import annotations

from pathlib import Path
import tempfile
import time
import unittest
from unittest import mock

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.durable import DurableExecutor
from ai_auto_desktop.errors import AutomationError
from ai_auto_desktop.journal import DesiredState, JournalStore, RunStatus
from ai_auto_desktop.run_service import RunService
from ai_auto_desktop.runtime import WorkflowRunner, canonical_plan_digest


def workflow(
    *steps: dict[str, object],
    max_duration: str = "5s",
    max_concurrency: int = 1,
    max_steps: int = 30,
    finally_steps: list[dict[str, object]] | None = None,
) -> object:
    raw: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "durable-contract", "version": "1.0.0"},
        "variables": {
            "count": {
                "schema": {"type": "integer"},
                "mutable": True,
                "initial": 0,
            }
        },
        "outputs": {"count": {"value": "${{ vars.count }}"}},
        "budgets": {
            "max_duration": max_duration,
            "max_executed_steps": max_steps,
            "max_concurrency": max_concurrency,
            "cleanup_timeout": "1s",
        },
        "steps": list(steps),
    }
    if finally_steps is not None:
        raw["finally"] = finally_steps
    return compile_descriptor(raw)


def assign(step_id: str, value: object, **extra: object) -> dict[str, object]:
    return {
        "id": step_id,
        "type": "set",
        "assign": {"vars.count": value},
        **extra,
    }


class DurableExecutionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.path = Path(self.temporary.name) / "journal.sqlite3"
        self.store = JournalStore(self.path)
        self.addCleanup(self.store.close)
        self.executor = DurableExecutor(self.store, owner_id="worker-a")

    def test_start_executes_and_persists_terminal_result(self) -> None:
        plan = workflow(assign("first", 2), assign("second", 3))
        outcome = self.executor.start(plan, run_id="run-1")

        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(outcome.run.output, {"count": 3})
        self.assertIsNone(outcome.run.owner_id)
        self.assertEqual(outcome.run.plan_digest, canonical_plan_digest(plan))
        self.assertEqual(outcome.result.status, "succeeded")
        event_types = [event.event_type for event in self.store.list_events("run-1")]
        self.assertEqual(event_types[0:2], ["run.created", "run.started"])
        self.assertEqual(event_types[-1], "run.finished")

    def test_run_service_exposes_durable_start_entrypoint(self) -> None:
        plan = workflow(assign("only", 6))
        outcome = RunService(self.store).start(
            plan, run_id="service-start", owner_id="service-worker"
        )
        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(outcome.run.output, {"count": 6})

    def test_plan_eligibility_fails_closed_before_run_creation(self) -> None:
        parallel = workflow(
            assign("left", 1, depends_on=[]),
            assign("right", 2, depends_on=[]),
            max_concurrency=2,
        )
        with self.assertRaises(AutomationError) as rejected:
            self.executor.start(parallel, run_id="parallel")
        self.assertEqual(rejected.exception.code, "DURABLE.UNSUPPORTED_PLAN")
        self.assertEqual(self.store.list_runs(), [])

    def test_action_and_script_plans_are_not_durable_in_v0(self) -> None:
        action_plan = workflow(
            {
                "id": "observe", "type": "action",
                "uses": "fixture.ocr@1", "with": {},
                "effect": {"class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
            }
        )
        script_plan = workflow(
            {
                "id": "script", "type": "script",
                "runtime": "python", "source": "print(1)",
                "output_schema": {},
            }
        )
        for index, plan in enumerate((action_plan, script_plan)):
            with self.subTest(plan=plan), self.assertRaises(AutomationError) as rejected:
                self.executor.start(plan, run_id=f"unsafe-{index}")
            self.assertEqual(rejected.exception.code, "DURABLE.UNSUPPORTED_PLAN")
        self.assertEqual(self.store.list_runs(), [])

    def test_segment_enter_cancel_race_prevents_next_segment_dispatch(self) -> None:
        plan = workflow(assign("first", 1), assign("never", 2))
        service = RunService(self.store)
        original = self.store.append_event_with_checkpoint
        enter_calls = 0

        def cancel_before_second_enter(
            run_id: str, event_type: str, *args: object, **kwargs: object
        ) -> object:
            nonlocal enter_calls
            if event_type == "run.segment_entered":
                enter_calls += 1
                if enter_calls == 2:
                    service.request_cancel(run_id)
            if event_type == "run.segment_entered":
                self.assertEqual(kwargs["expected_status"], RunStatus.RUNNING)
                self.assertEqual(
                    kwargs["expected_desired_state"], DesiredState.RUN
                )
            return original(run_id, event_type, *args, **kwargs)

        with mock.patch.object(
            self.store,
            "append_event_with_checkpoint",
            side_effect=cancel_before_second_enter,
        ):
            outcome = self.executor.start(plan, run_id="cancel-race")
        self.assertEqual(outcome.run.status, RunStatus.CANCELLED)
        self.assertEqual(outcome.result.variables["count"], 1)
        self.assertNotIn("never", outcome.result.steps)

    def test_segment_enter_pause_race_prevents_next_segment_dispatch(self) -> None:
        plan = workflow(assign("first", 1), assign("never", 2))
        service = RunService(self.store)
        original = self.store.append_event_with_checkpoint
        enter_calls = 0

        def pause_before_second_enter(
            run_id: str, event_type: str, *args: object, **kwargs: object
        ) -> object:
            nonlocal enter_calls
            if event_type == "run.segment_entered":
                enter_calls += 1
                if enter_calls == 2:
                    service.request_pause(run_id)
            return original(run_id, event_type, *args, **kwargs)

        with mock.patch.object(
            self.store,
            "append_event_with_checkpoint",
            side_effect=pause_before_second_enter,
        ):
            outcome = self.executor.start(plan, run_id="pause-race")
        self.assertEqual(outcome.run.status, RunStatus.PAUSED)
        self.assertEqual(outcome.run.checkpoint["nextTopLevelIndex"], 1)
        self.assertNotIn("never", outcome.run.checkpoint["stepRecords"])

    def test_resume_while_pause_is_requested_commits_paused_without_dispatch(self) -> None:
        plan = workflow(assign("never", 4))
        service = RunService(self.store)
        service.create(
            run_id="pause-before-resume", workflow_name=plan.name, inputs={},
            descriptor=plan, plan_digest=canonical_plan_digest(plan),
        )
        lease = self.store.claim_owner(
            "pause-before-resume", owner_id="dead", ttl_seconds=1, now=1
        )
        state = WorkflowRunner(plan).initialize()
        self.store.set_status_with_event(
            "pause-before-resume", expected=RunStatus.PENDING,
            status=RunStatus.RUNNING, owner_id=lease.owner_id, token=lease.token,
            now=1.5, event_type="run.started", event_payload={},
            checkpoint={"checkpointSchemaVersion": 1, **state.to_dict()},
        )
        service.request_pause("pause-before-resume")

        runner_factory = mock.Mock(side_effect=AssertionError("zero dispatch"))
        paused = DurableExecutor(
            self.store, owner_id="recovery", runner_factory=runner_factory
        ).resume("pause-before-resume", plan)
        self.assertEqual(paused.run.status, RunStatus.PAUSED)
        self.assertIsNone(paused.run.owner_id)
        runner_factory.assert_not_called()

    def test_pause_then_resume_uses_checkpoint_and_new_lease(self) -> None:
        plan = workflow(assign("first", 1), assign("second", 2))
        service = RunService(self.store)
        original = WorkflowRunner.run_segment
        calls = 0

        def pause_after_first(runner: WorkflowRunner) -> object:
            nonlocal calls
            result = original(runner)
            calls += 1
            if calls == 1:
                service.request_pause("paused")
            return result

        with mock.patch.object(WorkflowRunner, "run_segment", pause_after_first):
            paused = self.executor.start(plan, run_id="paused")
        self.assertEqual(paused.run.status, RunStatus.PAUSED)
        self.assertIsNone(paused.run.owner_id)
        self.assertEqual(paused.run.checkpoint["nextTopLevelIndex"], 1)

        service.request_resume("paused")
        resumed = DurableExecutor(
            self.store, owner_id="worker-b"
        ).resume("paused", plan)
        self.assertEqual(resumed.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(resumed.run.output, {"count": 2})
        completed = [
            event.payload["stepId"]
            for event in self.store.list_events("paused")
            if event.event_type == "run.segment_completed"
        ]
        self.assertEqual(completed, ["first", "second"])

    def test_unsafe_recovery_is_unknown_effect_without_dispatch(self) -> None:
        plan = workflow(assign("effectful", 9))
        service = RunService(self.store)
        service.create(
            run_id="crashed", workflow_name=plan.name, inputs={},
            descriptor=plan, plan_digest=canonical_plan_digest(plan),
        )
        lease = self.store.claim_owner(
            "crashed", owner_id="dead", ttl_seconds=1, now=1
        )
        runner = WorkflowRunner(plan)
        runner.initialize()
        unsafe = runner.prepare_segment().state
        self.store.set_status_with_event(
            "crashed", expected=RunStatus.PENDING, status=RunStatus.RUNNING,
            owner_id=lease.owner_id, token=lease.token, now=1.5,
            event_type="run.segment_entered", event_payload={},
            checkpoint={"checkpointSchemaVersion": 1, **unsafe.to_dict()},
        )

        factory = mock.Mock(side_effect=AssertionError("must not create runner"))
        outcome = DurableExecutor(
            self.store, owner_id="recovery", runner_factory=factory
        ).resume("crashed", plan)
        self.assertEqual(outcome.run.status, RunStatus.UNKNOWN_EFFECT)
        self.assertEqual(outcome.run.error["code"], "DURABLE.UNSAFE_RECOVERY")
        factory.assert_not_called()

    def test_resume_rejects_digest_mismatch_before_dispatch(self) -> None:
        original = workflow(assign("first", 1))
        changed = workflow(assign("first", 2))
        service = RunService(self.store)
        service.create(
            run_id="mismatch", workflow_name=original.name, inputs={},
            descriptor=original, plan_digest=canonical_plan_digest(original),
        )
        with self.assertRaises(AutomationError) as rejected:
            self.executor.resume("mismatch", changed)
        self.assertEqual(rejected.exception.code, "DURABLE.PLAN_MISMATCH")
        self.assertEqual(self.store.get_run("mismatch").status, RunStatus.PENDING)

    def test_expired_deadline_is_not_reset_and_finally_runs(self) -> None:
        plan = workflow(
            assign("work", 1),
            max_duration="1s",
            finally_steps=[assign("cleanup", 7)],
        )
        service = RunService(self.store)
        service.create(
            run_id="expired", workflow_name=plan.name, inputs={},
            descriptor=plan, plan_digest=canonical_plan_digest(plan),
        )
        lease = self.store.claim_owner(
            "expired", owner_id="dead", ttl_seconds=1, now=1
        )
        runner = WorkflowRunner(plan)
        state = runner.initialize(
            deadline_epoch_ms=int((time.time() + 60) * 1_000)
        )
        state_payload = state.to_dict()
        state_payload["deadlineEpochMs"] = 1
        self.store.set_status_with_event(
            "expired", expected=RunStatus.PENDING, status=RunStatus.RUNNING,
            owner_id=lease.owner_id, token=lease.token, now=1.5,
            event_type="run.started", event_payload={},
            checkpoint={"checkpointSchemaVersion": 1, **state_payload},
        )
        outcome = DurableExecutor(
            self.store, owner_id="recovery"
        ).resume("expired", plan)
        self.assertEqual(outcome.run.status, RunStatus.TIMED_OUT)
        self.assertEqual(outcome.result.variables["count"], 7)
        self.assertEqual(
            outcome.run.checkpoint["deadlineEpochMs"], 1
        )


if __name__ == "__main__":
    unittest.main()
