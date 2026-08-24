"""Execution contracts for sibling-scoped DAG scheduling."""

from __future__ import annotations

from copy import deepcopy
import threading
import time
import unittest

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.plugin import PluginError, ProcessPlugin
from ai_auto_desktop.runtime import WorkflowRunner, run_descriptor


def workflow(
    *steps: dict[str, object],
    max_concurrency: int = 4,
    max_steps: int = 100,
    finally_steps: list[dict[str, object]] | None = None,
) -> dict[str, object]:
    value: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "dag-runtime-contract"},
        "budgets": {
            "max_duration": "2s",
            "max_executed_steps": max_steps,
            "cleanup_timeout": "1s",
            "max_concurrency": max_concurrency,
        },
        "steps": list(steps),
    }
    if finally_steps is not None:
        value["finally"] = finally_steps
    return value


def action(
    step_id: str,
    *,
    depends_on: list[str] | None = None,
    delay: float = 0.05,
    effect: str = "read_only",
    fail: bool = False,
    uses: str | None = None,
) -> dict[str, object]:
    value: dict[str, object] = {
        "id": step_id,
        "type": "action",
        "uses": uses or f"{step_id}.run@1",
        "with": {"delay": delay, "fail": fail},
        "effect": {"class": effect},
        "risk": {"category": "observe", "level": "low"},
    }
    if depends_on is not None:
        value["depends_on"] = depends_on
    return value


def manifest(name: str, *, effect: str = "read_only") -> dict[str, object]:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": name, "version": "1.0.0"},
        "actions": {
            "run": {
                "contract_major": 1,
                "effect": {"default_class": effect},
                "risk": {"category": "observe", "level": "low"},
                "input_schema": {"type": "object"},
                "output_schema": {"type": "object"},
                "errors": [
                    {
                        "code": "TEST.FAIL",
                        "retryable": False,
                        "effect": "not_applied",
                    }
                ],
            }
        },
    }


class TimedPlugin(ProcessPlugin):
    def __init__(
        self,
        name: str,
        state: dict[str, object],
        *,
        effect: str = "read_only",
    ) -> None:
        super().__init__(["python", "-c", "pass"], name=name)
        self.manifest_value = manifest(name, effect=effect)
        self.manifest = deepcopy(self.manifest_value)
        self.state = state

    def start(self, timeout: float | None = None) -> dict[str, object]:
        self.manifest = deepcopy(self.manifest_value)
        return self.manifest

    def invoke(
        self, action_name: str, args: object, timeout: float | None = None
    ) -> object:
        assert isinstance(args, dict)
        lock = self.state["lock"]
        assert isinstance(lock, type(threading.Lock()))
        with lock:
            active = int(self.state.get("active", 0)) + 1
            self.state["active"] = active
            self.state["peak"] = max(int(self.state.get("peak", 0)), active)
            starts = self.state.setdefault("starts", {})
            assert isinstance(starts, dict)
            starts[self.name] = time.monotonic()
        try:
            delay = float(args["delay"])
            if timeout is not None and delay > timeout:
                time.sleep(timeout)
                raise PluginError(
                    "PLUGIN.HOST_TIMEOUT",
                    "planned timeout",
                    details={"dispatched": True},
                    retryable=True,
                )
            time.sleep(delay)
            if args.get("fail"):
                raise PluginError(
                    "TEST.FAIL", "planned failure", retryable=True
                )
            return {"name": self.name}
        finally:
            with lock:
                self.state["active"] = int(self.state["active"]) - 1
                finishes = self.state.setdefault("finishes", {})
                assert isinstance(finishes, dict)
                finishes[self.name] = time.monotonic()


def plugins_for(
    names: list[str],
    state: dict[str, object],
    *,
    effects: dict[str, str] | None = None,
) -> dict[str, TimedPlugin]:
    effects = effects or {}
    return {
        name: TimedPlugin(name, state, effect=effects.get(name, "read_only"))
        for name in names
    }


class DagRuntimeTests(unittest.TestCase):
    def state(self) -> dict[str, object]:
        return {"lock": threading.Lock(), "active": 0, "peak": 0}

    def test_independent_read_only_actions_overlap_and_join_reads_outputs(self) -> None:
        state = self.state()
        raw = workflow(
            action("left", depends_on=[], delay=0.18),
            action("right", depends_on=[], delay=0.18),
            {
                "id": "join",
                "type": "return",
                "depends_on": ["left", "right"],
                "value": [
                    "${{ steps.left.output.name }}",
                    "${{ steps.right.output.name }}",
                ],
            },
            max_concurrency=2,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(["left", "right"], state),
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.output, ["left", "right"])
        self.assertEqual(state["peak"], 2)
        starts = state["starts"]
        finishes = state["finishes"]
        assert isinstance(starts, dict) and isinstance(finishes, dict)
        self.assertLess(starts["left"], finishes["right"])
        self.assertLess(starts["right"], finishes["left"])
        self.assertEqual(
            [event["step_id"] for event in result.events].count("left"), 2
        )
        self.assertEqual(
            [event["step_id"] for event in result.events].count("right"), 2
        )
        self.assertEqual([event["step_id"] for event in result.events][-1], "join")
        self.assertEqual(
            [event["time"] for event in result.events],
            sorted(event["time"] for event in result.events),
        )

    def test_max_concurrency_is_a_global_bound(self) -> None:
        state = self.state()
        names = ["one", "two", "three", "four"]
        raw = workflow(
            *(action(name, depends_on=[], delay=0.08) for name in names),
            max_concurrency=2,
        )

        result = run_descriptor(
            compile_descriptor(raw), plugins=plugins_for(names, state)
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(state["peak"], 2)

    def test_default_concurrency_keeps_legacy_chain_serial(self) -> None:
        state = self.state()
        raw = workflow(
            action("first", delay=0.03),
            action("second", delay=0.03),
            max_concurrency=1,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(["first", "second"], state),
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(state["peak"], 1)
        starts = state["starts"]
        finishes = state["finishes"]
        assert isinstance(starts, dict) and isinstance(finishes, dict)
        self.assertGreaterEqual(starts["second"], finishes["first"])

    def test_forward_dependency_waits_for_its_prerequisite(self) -> None:
        state = self.state()
        raw = workflow(
            action("sink", depends_on=["source"], delay=0.01),
            action("source", depends_on=[], delay=0.06),
            max_concurrency=2,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(["source", "sink"], state),
        )

        self.assertTrue(result.ok, result.to_dict())
        starts = state["starts"]
        finishes = state["finishes"]
        assert isinstance(starts, dict) and isinstance(finishes, dict)
        self.assertGreaterEqual(starts["sink"], finishes["source"])

    def test_skipped_dependency_runs_finally_before_releasing_dependent(self) -> None:
        state = self.state()
        raw = workflow(
            {
                "id": "guarded",
                "type": "block",
                "depends_on": [],
                "if": "${{ False }}",
                "steps": [{"id": "body", "type": "return"}],
                "finally": [action("cleanup", depends_on=[], delay=0.06)],
            },
            action("dependent", depends_on=["guarded"], delay=0.01),
            max_concurrency=4,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(["cleanup", "dependent"], state),
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.steps["guarded"]["status"], "skipped")
        starts = state["starts"]
        finishes = state["finishes"]
        assert isinstance(starts, dict) and isinstance(finishes, dict)
        self.assertGreaterEqual(starts["dependent"], finishes["cleanup"])

    def test_failure_does_not_start_new_steps_after_inflight_batch(self) -> None:
        state = self.state()
        raw = workflow(
            action("failure", depends_on=[], delay=0.02, fail=True),
            action("inflight", depends_on=[], delay=0.08),
            action("later", depends_on=[], delay=0.01),
            max_concurrency=2,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(["failure", "inflight", "later"], state),
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "TEST.FAIL")
        self.assertIn("inflight", result.steps)
        self.assertEqual(result.steps["inflight"]["status"], "succeeded")
        self.assertIs(
            result.steps["inflight"]["discarded_due_to_scope_termination"],
            True,
        )
        self.assertEqual(result.steps["later"]["status"], "skipped")
        self.assertEqual(result.steps["later"]["reason"], "scope_terminated")
        starts = state["starts"]
        assert isinstance(starts, dict)
        self.assertNotIn("later", starts)

    def test_later_failure_does_not_discard_earlier_success(self) -> None:
        state = self.state()
        raw = workflow(
            action("success", depends_on=[], delay=0.05),
            action("failure", depends_on=[], delay=0.01, fail=True),
            max_concurrency=2,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(["success", "failure"], state),
        )

        self.assertEqual(result.error.code, "TEST.FAIL")
        self.assertEqual(result.steps["success"]["status"], "succeeded")
        self.assertEqual(result.steps["failure"]["status"], "failed")

    def test_non_read_only_action_is_an_exclusive_global_barrier(self) -> None:
        state = self.state()
        raw = workflow(
            action("before", depends_on=[], delay=0.06),
            action("writer", depends_on=[], delay=0.04, effect="idempotent"),
            action("after", depends_on=[], delay=0.06),
            max_concurrency=3,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(
                ["before", "writer", "after"],
                state,
                effects={"writer": "idempotent"},
            ),
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(state["peak"], 1)
        starts = state["starts"]
        finishes = state["finishes"]
        assert isinstance(starts, dict) and isinstance(finishes, dict)
        self.assertGreaterEqual(starts["writer"], finishes["before"])
        self.assertGreaterEqual(starts["after"], finishes["writer"])

    def test_exclusive_step_blocks_all_ready_actions_in_its_batch(self) -> None:
        state = self.state()
        raw = workflow(
            action("first", depends_on=[], delay=0.05),
            action("writer", depends_on=[], delay=0.04, effect="idempotent"),
            action("third", depends_on=[], delay=0.05),
            max_concurrency=3,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(
                ["first", "writer", "third"],
                state,
                effects={"writer": "idempotent"},
            ),
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(state["peak"], 1)

    def test_same_process_plugin_is_not_scheduled_into_two_parallel_slots(self) -> None:
        state = self.state()
        shared = TimedPlugin("shared", state)
        raw = workflow(
            action("first", depends_on=[], delay=0.06, uses="shared.run@1"),
            action("second", depends_on=[], delay=0.06, uses="shared.run@1"),
            max_concurrency=2,
        )

        result = run_descriptor(
            compile_descriptor(raw), plugins={"shared": shared}
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(state["peak"], 1)

    def test_cleanup_still_runs_after_step_budget_is_exhausted(self) -> None:
        raw = workflow(
            {
                "id": "first",
                "type": "set",
                "assign": {"vars.cleaned": False},
            },
            {
                "id": "blocked",
                "type": "return",
            },
            max_steps=1,
            finally_steps=[
                {
                    "id": "cleanup",
                    "type": "set",
                    "assign": {"vars.cleaned": True},
                }
            ],
        )
        raw["variables"] = {
            "cleaned": {
                "schema": {"type": "boolean"},
                "mutable": True,
                "initial": False,
            }
        }

        result = run_descriptor(compile_descriptor(raw))

        self.assertEqual(result.error.code, "WORKFLOW.STEP_LIMIT")
        self.assertIs(result.variables["cleaned"], True)
        self.assertEqual(result.steps["cleanup"]["status"], "succeeded")

    def test_parallel_attempt_budget_is_reserved_before_dispatch(self) -> None:
        state = self.state()
        raw = workflow(
            action("first", depends_on=[], delay=0.03),
            action("second", depends_on=[], delay=0.03),
            max_concurrency=2,
            max_steps=1,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(["first", "second"], state),
        )

        self.assertEqual(result.error.code, "WORKFLOW.STEP_LIMIT")
        starts = state["starts"]
        assert isinstance(starts, dict)
        self.assertEqual(set(starts), {"first"})

    def test_cancel_during_retry_backoff_stops_before_next_attempt(self) -> None:
        state = self.state()
        step = action("retrying", depends_on=[], delay=0, fail=True)
        step["retry"] = {
            "max_attempts": 3,
            "on": {"codes": ["TEST.FAIL"]},
            "backoff": {"strategy": "fixed", "initial_delay": "500ms"},
        }
        runner = WorkflowRunner(
            compile_descriptor(workflow(step, max_concurrency=1)),
            plugins=plugins_for(["retrying"], state),
        )
        holder: list[object] = []
        thread = threading.Thread(target=lambda: holder.append(runner.run()))

        thread.start()
        deadline = time.monotonic() + 1
        while time.monotonic() < deadline:
            starts = state.get("starts", {})
            if isinstance(starts, dict) and "retrying" in starts:
                break
            time.sleep(0.005)
        runner.cancel()
        thread.join(timeout=1)

        self.assertFalse(thread.is_alive())
        result = holder[0]
        self.assertEqual(result.status, "cancelled")
        self.assertEqual(result.steps["retrying"]["attempts"], 1)

    def test_cancel_before_run_is_not_lost(self) -> None:
        runner = WorkflowRunner(
            compile_descriptor(
                workflow({"id": "finish", "type": "return"})
            )
        )

        runner.cancel()
        result = runner.run()

        self.assertEqual(result.status, "cancelled")
        self.assertEqual(result.steps["finish"]["status"], "skipped")

    def test_cancel_while_last_action_runs_changes_terminal_status(self) -> None:
        state = self.state()
        runner = WorkflowRunner(
            compile_descriptor(
                workflow(
                    action("only", depends_on=[], delay=0.15),
                    max_concurrency=1,
                )
            ),
            plugins=plugins_for(["only"], state),
        )
        holder: list[object] = []
        thread = threading.Thread(target=lambda: holder.append(runner.run()))

        thread.start()
        deadline = time.monotonic() + 1
        while time.monotonic() < deadline:
            starts = state.get("starts", {})
            if isinstance(starts, dict) and "only" in starts:
                break
            time.sleep(0.005)
        runner.cancel()
        thread.join(timeout=1)

        self.assertFalse(thread.is_alive())
        result = holder[0]
        self.assertEqual(result.status, "cancelled")

    def test_step_finally_runs_after_cancellation_with_cleanup_budget(self) -> None:
        state = self.state()
        raw = workflow(
            {
                "id": "parent",
                "type": "block",
                "steps": [action("slow", depends_on=[], delay=0.15)],
                "finally": [
                    {
                        "id": "cleanup",
                        "type": "set",
                        "assign": {"vars.cleaned": True},
                    }
                ],
            },
            max_concurrency=1,
        )
        raw["variables"] = {
            "cleaned": {
                "schema": {"type": "boolean"},
                "mutable": True,
                "initial": False,
            }
        }
        runner = WorkflowRunner(
            compile_descriptor(raw), plugins=plugins_for(["slow"], state)
        )
        holder: list[object] = []
        thread = threading.Thread(target=lambda: holder.append(runner.run()))

        thread.start()
        deadline = time.monotonic() + 1
        while time.monotonic() < deadline:
            starts = state.get("starts", {})
            if isinstance(starts, dict) and "slow" in starts:
                break
            time.sleep(0.005)
        runner.cancel()
        thread.join(timeout=1)

        result = holder[0]
        self.assertEqual(result.status, "cancelled")
        self.assertIs(result.variables["cleaned"], True)
        self.assertEqual(result.steps["cleanup"]["status"], "succeeded")

    def test_skipped_step_does_not_consume_exhausted_attempt_budget(self) -> None:
        raw = workflow(
            {
                "id": "first",
                "type": "set",
                "assign": {"vars.value": 1},
            },
            {
                "id": "guarded",
                "type": "set",
                "if": "${{ False }}",
                "assign": {"vars.value": 2},
            },
            max_steps=1,
        )
        raw["variables"] = {
            "value": {
                "schema": {"type": "integer"},
                "mutable": True,
                "initial": 0,
            }
        }

        result = run_descriptor(compile_descriptor(raw))

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.steps["guarded"]["status"], "skipped")
        self.assertEqual(result.variables["value"], 1)

    def test_parent_deadline_bounds_parallel_child_actions(self) -> None:
        state = self.state()
        raw = workflow(
            {
                "id": "parent",
                "type": "block",
                "timeout": "40ms",
                "steps": [
                    action("left", depends_on=[], delay=0.1),
                    action("right", depends_on=[], delay=0.1),
                ],
            },
            max_concurrency=2,
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins=plugins_for(["left", "right"], state),
        )

        self.assertEqual(result.status, "timed_out")
        self.assertIn(result.error.code, {"ACTION.TIMEOUT", "STEP.TIMEOUT"})
        self.assertEqual(state["peak"], 2)

    def test_non_json_context_fails_closed_before_parallel_dispatch(self) -> None:
        state = self.state()
        raw = workflow(
            action("left", depends_on=[], delay=0.01),
            action("right", depends_on=[], delay=0.01),
            max_concurrency=2,
        )
        raw["inputs"] = {
            "opaque": {"schema": True, "required": True}
        }

        result = run_descriptor(
            compile_descriptor(raw),
            inputs={"opaque": object()},
            plugins=plugins_for(["left", "right"], state),
        )

        self.assertEqual(result.error.code, "RUNTIME.CONTEXT_CONFLICT")
        self.assertNotIn("starts", state)


if __name__ == "__main__":
    unittest.main()
