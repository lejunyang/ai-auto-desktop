"""Contracts for the opt-in top-level runtime segment API."""

from __future__ import annotations

import time
import unittest
from unittest import mock

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.errors import AutomationError
from ai_auto_desktop.runtime import (
    RUNTIME_STATE_SCHEMA_VERSION,
    RUNTIME_VERSION,
    WorkflowRunner,
    canonical_plan_digest,
)


def descriptor(
    *steps: dict[str, object],
    max_steps: int = 20,
    max_duration: str = "5s",
    finally_steps: list[dict[str, object]] | None = None,
) -> object:
    raw: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "segment-contract", "version": "1.0.0"},
        "inputs": {
            "value": {"schema": {"type": "integer"}, "default": 1}
        },
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
            "cleanup_timeout": "1s",
            "max_concurrency": 1,
        },
        "steps": list(steps),
    }
    if finally_steps is not None:
        raw["finally"] = finally_steps
    return compile_descriptor(raw)


def set_count(step_id: str, value: object) -> dict[str, object]:
    return {
        "id": step_id,
        "type": "set",
        "assign": {"vars.count": value},
    }


class RuntimeSegmentTests(unittest.TestCase):
    def test_top_level_chain_exports_versioned_state_and_resumes(self) -> None:
        plan = descriptor(
            set_count("first", "${{ inputs.value }}"),
            set_count("second", "${{ vars.count + 1 }}"),
        )
        first = WorkflowRunner(plan)
        initial = first.initialize({"value": 4})
        self.assertEqual(initial.schema_version, RUNTIME_STATE_SCHEMA_VERSION)
        self.assertEqual(initial.runtime_version, RUNTIME_VERSION)
        self.assertEqual(initial.descriptor_digest, canonical_plan_digest(plan))

        entered = first.prepare_segment()
        self.assertEqual(entered.step_id, "first")
        self.assertEqual(entered.state.phase, "in_top_level_step")
        progressed = first.run_segment()
        self.assertEqual(progressed.state.phase, "between_top_level_steps")
        self.assertEqual(progressed.state.next_top_level_index, 1)
        self.assertEqual(progressed.state.executed_attempts, 1)

        resumed = WorkflowRunner(plan)
        restored = resumed.import_state(progressed.state.to_dict(), inputs={"value": 4})
        self.assertEqual(restored.variables, {"count": 4})
        self.assertEqual(resumed.context["vars"], resumed.variables)
        completed = resumed.run_segment()
        self.assertTrue(completed.terminal_ready)
        self.assertEqual(completed.state.executed_attempts, 2)
        result = resumed.finalize()
        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.output, {"count": 5})

    def test_digest_is_canonical_and_mismatch_is_rejected(self) -> None:
        first = descriptor(set_count("first", 1))
        same = descriptor(set_count("first", 1))
        changed = descriptor(set_count("first", 2))
        self.assertEqual(canonical_plan_digest(first), canonical_plan_digest(same))
        self.assertNotEqual(canonical_plan_digest(first), canonical_plan_digest(changed))
        checkpoint = WorkflowRunner(first).initialize().to_dict()
        with self.assertRaises(AutomationError) as rejected:
            WorkflowRunner(changed).import_state(checkpoint)
        self.assertEqual(rejected.exception.code, "RUNTIME.STATE_INVALID")
        self.assertIn("DescriptorDigest", str(rejected.exception.details))

    def test_import_requires_exact_schema_and_safe_boundary(self) -> None:
        plan = descriptor(set_count("first", 1))
        state = WorkflowRunner(plan).initialize().to_dict()
        malformed = dict(state)
        malformed["extra"] = True
        for candidate in (malformed, {**state, "schemaVersion": 99}):
            with self.subTest(candidate=candidate), self.assertRaises(
                AutomationError
            ) as rejected:
                WorkflowRunner(plan).import_state(candidate)
            self.assertEqual(rejected.exception.code, "RUNTIME.STATE_INVALID")

        for phase in ("in_top_level_step", "finalizing", "finalized"):
            with self.subTest(phase=phase), self.assertRaises(AutomationError):
                WorkflowRunner(plan).import_state({**state, "phase": phase})

    def test_deadline_is_absolute_and_never_reset_on_import(self) -> None:
        plan = descriptor(set_count("first", 1))
        deadline = int((time.time() + 30) * 1_000)
        state = WorkflowRunner(plan).initialize(deadline_epoch_ms=deadline)
        with mock.patch("ai_auto_desktop.runtime.time.time", return_value=time.time() + 10):
            runner = WorkflowRunner(plan)
            restored = runner.import_state(state.to_dict())
        self.assertEqual(restored.deadline_epoch_ms, deadline)
        self.assertEqual(runner.export_state().deadline_epoch_ms, deadline)

        expired = {**state.to_dict(), "deadlineEpochMs": 1}
        with self.assertRaises(AutomationError) as timed_out:
            WorkflowRunner(plan).import_state(expired)
        self.assertEqual(timed_out.exception.code, "WORKFLOW.TIMEOUT")
        recovered = WorkflowRunner(plan).import_state(expired, allow_expired=True)
        self.assertEqual(recovered.deadline_epoch_ms, 1)

    def test_attempt_budget_survives_resume(self) -> None:
        plan = descriptor(
            set_count("first", 1),
            set_count("second", 2),
            max_steps=1,
        )
        runner = WorkflowRunner(plan)
        runner.initialize()
        checkpoint = runner.run_segment().state
        resumed = WorkflowRunner(plan)
        resumed.import_state(checkpoint.to_dict())
        terminal = resumed.run_segment()
        self.assertTrue(terminal.terminal_ready)
        result = resumed.finalize()
        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "WORKFLOW.STEP_LIMIT")

    def test_return_failure_and_finalize_once(self) -> None:
        returning = descriptor(
            {"id": "stop", "type": "return", "value": 42},
            set_count("never", 9),
        )
        runner = WorkflowRunner(returning)
        runner.initialize()
        self.assertTrue(runner.run_segment().terminal_ready)
        result = runner.finalize()
        self.assertEqual((result.status, result.output), ("succeeded", 42))
        with self.assertRaises(AutomationError):
            runner.finalize()

        failing = descriptor(
            {
                "id": "fail",
                "type": "fail",
                "error": {"code": "TEST.FAIL", "message": "boom"},
            },
            set_count("never", 9),
            finally_steps=[set_count("cleanup", 7)],
        )
        runner = WorkflowRunner(failing)
        runner.initialize()
        self.assertTrue(runner.run_segment().terminal_ready)
        result = runner.finalize()
        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "TEST.FAIL")
        self.assertEqual(result.variables["count"], 7)

    def test_prepare_state_is_not_resumable_and_does_not_execute(self) -> None:
        plan = descriptor(set_count("effectful", 8))
        runner = WorkflowRunner(plan)
        runner.initialize()
        unsafe = runner.prepare_segment().state
        self.assertEqual(runner.variables["count"], 0)
        with self.assertRaises(AutomationError) as rejected:
            WorkflowRunner(plan).import_state(unsafe.to_dict())
        self.assertEqual(rejected.exception.code, "RUNTIME.STATE_INVALID")


if __name__ == "__main__":
    unittest.main()
