"""Black-box contracts for workflow execution semantics."""

from __future__ import annotations

from pathlib import Path
import sys
import unittest

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.plugin import PluginError, ProcessPlugin
from ai_auto_desktop.runtime import WorkflowRunner, run_descriptor


PROJECT_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_PLUGIN = PROJECT_ROOT / "plugins" / "fixture" / "fixture_plugin.py"


def workflow(
    *steps: dict[str, object],
    inputs: dict[str, object] | None = None,
    variables: dict[str, object] | None = None,
    on_error: dict[str, object] | None = None,
    finally_steps: list[dict[str, object]] | None = None,
    max_duration: str = "5s",
) -> dict[str, object]:
    value: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "runtime-contract"},
        "budgets": {
            "max_duration": max_duration,
            "max_executed_steps": 100,
            "cleanup_timeout": "1s",
        },
        "steps": list(steps),
    }
    if inputs is not None:
        value["inputs"] = inputs
    if variables is not None:
        value["variables"] = variables
    if on_error is not None:
        value["on_error"] = on_error
    if finally_steps is not None:
        value["finally"] = finally_steps
    return value


def variable(schema_type: str, initial: object) -> dict[str, object]:
    return {"schema": {"type": schema_type}, "mutable": True, "initial": initial}


class _FailingPlugin(ProcessPlugin):
    def __init__(self, *, dispatched: bool = True) -> None:
        super().__init__([sys.executable, "-c", "pass"], name="stub")
        self.calls = 0
        self.was_dispatched = dispatched

    def invoke(self, action: str, args: object, timeout: float | None = None) -> object:
        self.calls += 1
        raise PluginError(
            "PLUGIN.HOST_TIMEOUT",
            "fixture timed out",
            retryable=True,
            details={"dispatched": self.was_dispatched},
        )


class RuntimeControlFlowTests(unittest.TestCase):
    def test_parent_timeout_bounds_nested_steps(self) -> None:
        raw = workflow(
            {
                "id": "parent",
                "type": "block",
                "timeout": "20ms",
                "steps": [
                    {
                        "id": "child",
                        "type": "action",
                        "uses": "fixture.sleep@1",
                        "with": {"milliseconds": 200},
                        "effect": {"class": "read_only"},
                        "risk": {
                            "category": "custom",
                            "level": "low",
                            "custom_name": "fixture.sleep",
                        },
                    }
                ],
            }
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins={"fixture": [sys.executable, str(FIXTURE_PLUGIN)]},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "PLUGIN.HOST_TIMEOUT")

    def test_set_is_atomic_and_validates_declared_schema(self) -> None:
        raw = workflow(
            {
                "id": "swap",
                "type": "set",
                "assign": {
                    "vars.left": "${{ vars.right }}",
                    "vars.right": "${{ vars.left }}",
                },
            },
            {
                "id": "finish",
                "type": "return",
                "value": ["${{ vars.left }}", "${{ vars.right }}"],
            },
            variables={
                "left": variable("integer", 1),
                "right": variable("integer", 2),
            },
        )

        result = run_descriptor(compile_descriptor(raw))

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.output, [2, 1])

    def test_set_schema_failure_does_not_partially_commit(self) -> None:
        raw = workflow(
            {
                "id": "invalid_update",
                "type": "set",
                "assign": {
                    "vars.left": 7,
                    "vars.right": "wrong type",
                },
            },
            variables={
                "left": variable("integer", 1),
                "right": variable("integer", 2),
            },
        )

        result = run_descriptor(compile_descriptor(raw))

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "VARIABLE.INVALID")
        self.assertEqual(result.variables, {"left": 1, "right": 2})

    def test_explicit_null_return_is_not_replaced_by_workflow_outputs(self) -> None:
        raw = workflow({"id": "finish", "type": "return", "value": None})
        raw["outputs"] = {"fallback": {"value": "wrong"}}

        result = run_descriptor(compile_descriptor(raw))

        self.assertTrue(result.ok, result.to_dict())
        self.assertIsNone(result.output)

    def test_skipped_step_still_runs_its_finally(self) -> None:
        raw = workflow(
            {
                "id": "guarded",
                "type": "block",
                "if": "${{ False }}",
                "steps": [{"id": "body", "type": "return"}],
                "finally": [
                    {
                        "id": "guarded_cleanup",
                        "type": "set",
                        "assign": {"vars.cleaned": True},
                    }
                ],
            },
            {"id": "finish", "type": "return", "value": "${{ vars.cleaned }}"},
            variables={"cleaned": variable("boolean", False)},
        )

        result = run_descriptor(compile_descriptor(raw))

        self.assertTrue(result.ok, result.to_dict())
        self.assertIs(result.output, True)
        self.assertEqual(result.steps["guarded"]["status"], "skipped")
    def test_if_switch_foreach_and_while_execute_expected_paths(self) -> None:
        raw = workflow(
            {
                "id": "choose",
                "type": "if",
                "condition": "${{ inputs.enabled }}",
                "then": [
                    {"id": "mark_true", "type": "set", "assign": {"vars.branch": "if"}}
                ],
                "else": [
                    {"id": "mark_false", "type": "set", "assign": {"vars.branch": "else"}}
                ],
            },
            {
                "id": "select",
                "type": "switch",
                "cases": [
                    {
                        "when": "${{ vars.branch == 'if' }}",
                        "steps": [
                            {"id": "mark_switch", "type": "set", "assign": {"vars.branch": "switch"}}
                        ],
                    }
                ],
                "default": [
                    {"id": "mark_default", "type": "set", "assign": {"vars.branch": "default"}}
                ],
            },
            {
                "id": "collect",
                "type": "foreach",
                "items": "${{ inputs.values }}",
                "as": "item",
                "index_as": "position",
                "max_items": 4,
                "steps": [
                    {
                        "id": "append_item",
                        "type": "set",
                        "assign": {"vars.items": "${{ vars.items + [item] }}"},
                    }
                ],
            },
            {
                "id": "poll",
                "type": "while",
                "condition": "${{ vars.counter < 3 }}",
                "max_iterations": 3,
                "timeout": "1s",
                "steps": [
                    {
                        "id": "increment",
                        "type": "set",
                        "assign": {"vars.counter": "${{ vars.counter + 1 }}"},
                    }
                ],
            },
            {
                "id": "finish",
                "type": "return",
                "value": {
                    "branch": "${{ vars.branch }}",
                    "items": "${{ vars.items }}",
                    "counter": "${{ vars.counter }}",
                },
            },
            inputs={
                "enabled": {"schema": {"type": "boolean"}, "required": True},
                "values": {"schema": {"type": "array"}, "required": True},
            },
            variables={
                "branch": variable("string", ""),
                "items": variable("array", []),
                "counter": variable("integer", 0),
            },
        )

        result = run_descriptor(
            compile_descriptor(raw), inputs={"enabled": True, "values": ["a", "b"]}
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(
            result.output, {"branch": "switch", "items": ["a", "b"], "counter": 3}
        )
        self.assertNotIn("mark_false", result.steps)
        self.assertNotIn("mark_default", result.steps)

    def test_while_that_remains_true_fails_at_iteration_bound(self) -> None:
        raw = workflow(
            {
                "id": "bounded",
                "type": "while",
                "condition": "${{ True }}",
                "max_iterations": 2,
                "timeout": "1s",
                "steps": [
                    {
                        "id": "increment_forever",
                        "type": "set",
                        "assign": {"vars.iterations": "${{ vars.iterations + 1 }}"},
                    }
                ],
            },
            variables={"iterations": variable("integer", 0)},
        )

        result = run_descriptor(compile_descriptor(raw))

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "LOOP.LIMIT_EXCEEDED")

    def test_step_on_error_can_continue_with_replacement_output(self) -> None:
        raw = workflow(
            {
                "id": "expected_failure",
                "type": "fail",
                "error": {"code": "TEST.EXPECTED", "message": "expected"},
                "on_error": {
                    "match": {"codes": ["TEST.*"]},
                    "steps": [
                        {
                            "id": "record_recovery",
                            "type": "set",
                            "assign": {"vars.recovered": "${{ error.code }}"},
                        }
                    ],
                    "outcome": {"mode": "continue", "output": {"handled": True}},
                },
            },
            {"id": "finish", "type": "return", "value": "${{ steps.expected_failure.output.handled }}"},
            variables={"recovered": variable("string", "")},
        )

        result = run_descriptor(compile_descriptor(raw))

        self.assertTrue(result.ok, result.to_dict())
        self.assertIs(result.output, True)
        self.assertEqual(result.variables["recovered"], "TEST.EXPECTED")

    def test_workflow_finally_runs_after_failure(self) -> None:
        raw = workflow(
            {
                "id": "fail_now",
                "type": "fail",
                "error": {"code": "TEST.FAILURE", "message": "boom"},
            },
            variables={"cleaned": variable("boolean", False)},
            finally_steps=[
                {"id": "cleanup", "type": "set", "assign": {"vars.cleaned": True}}
            ],
        )

        result = run_descriptor(compile_descriptor(raw))

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "TEST.FAILURE")
        self.assertIs(result.variables["cleaned"], True)
        self.assertEqual(result.steps["cleanup"]["status"], "succeeded")


class RuntimeActionTests(unittest.TestCase):
    def test_manifest_output_schema_is_enforced(self) -> None:
        raw = workflow(
            {
                "id": "bad_ocr_output",
                "type": "action",
                "uses": "fixture.ocr@1",
                "with": {"result": "not an object"},
                "effect": {"class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
            }
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins={"fixture": [sys.executable, str(FIXTURE_PLUGIN)]},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "ACTION.OUTPUT_INVALID")

    def test_manifest_rejects_unknown_contract_major_before_dispatch(self) -> None:
        raw = workflow(
            {
                "id": "future_ocr",
                "type": "action",
                "uses": "fixture.ocr@99",
                "with": {},
                "effect": {"class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
            }
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins={"fixture": [sys.executable, str(FIXTURE_PLUGIN)]},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "CAPABILITY.VERSION_INCOMPATIBLE")

    def test_policy_denies_disallowed_risk_category_before_dispatch(self) -> None:
        raw = workflow(
            {
                "id": "navigate",
                "type": "action",
                "uses": "fixture.invoke@1",
                "with": {},
                "effect": {"class": "idempotent"},
                "risk": {"category": "navigate", "level": "low"},
            }
        )
        raw["policy"] = {
            "allowed_risk": {"categories": ["observe"], "max_level": "low"}
        }

        result = run_descriptor(
            compile_descriptor(raw),
            plugins={"fixture": [sys.executable, str(FIXTURE_PLUGIN)]},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "POLICY.DENIED")

    def test_descriptor_cannot_lower_manifest_risk(self) -> None:
        raw = workflow(
            {
                "id": "invoke",
                "type": "action",
                "uses": "fixture.invoke@1",
                "with": {},
                "effect": {"class": "idempotent"},
                "risk": {"category": "observe", "level": "low"},
            }
        )
        raw["policy"] = {
            "allowed_risk": {"categories": ["observe"], "max_level": "low"}
        }

        result = run_descriptor(
            compile_descriptor(raw),
            plugins={"fixture": [sys.executable, str(FIXTURE_PLUGIN)]},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "POLICY.DENIED")

    def test_retryable_idempotent_action_retries_until_success(self) -> None:
        raw = workflow(
            {
                "id": "retry_action",
                "type": "action",
                "uses": "fixture.transient@1",
                "with": {"key": "runtime-retry", "failures": 2},
                "effect": {"class": "idempotent"},
                "risk": {"category": "custom", "level": "low", "custom_name": "fixture.transient"},
                "retry": {
                    "max_attempts": 3,
                    "on": {"codes": ["FIXTURE.TRANSIENT"]},
                },
            }
        )
        command = [sys.executable, str(FIXTURE_PLUGIN)]

        result = run_descriptor(compile_descriptor(raw), plugins={"fixture": command})

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.steps["retry_action"]["attempts"], 3)
        self.assertEqual(result.steps["retry_action"]["output"]["attempt"], 3)

    def test_dispatched_non_idempotent_timeout_is_unknown_and_not_retried(self) -> None:
        plugin = _FailingPlugin(dispatched=True)
        self.addCleanup(plugin.close)
        raw = workflow(
            {
                "id": "send_once",
                "type": "action",
                "uses": "fixture.invoke@1",
                "with": {},
                "effect": {"class": "non_idempotent"},
                "risk": {"category": "send", "level": "high"},
                "retry": {
                    "max_attempts": 3,
                    "on": {"codes": ["ACTION.UNKNOWN_EFFECT"]},
                },
            }
        )

        result = run_descriptor(compile_descriptor(raw), plugins={"fixture": plugin})

        self.assertEqual(result.status, "unknown_effect")
        self.assertEqual(result.error.code, "ACTION.UNKNOWN_EFFECT")
        self.assertEqual(result.error.effect, "unknown")
        self.assertFalse(result.error.retryable)
        self.assertEqual(plugin.calls, 1)

    def test_ocr_output_explicitly_controls_followup_desktop_action(self) -> None:
        raw = workflow(
            {
                "id": "read_dialog",
                "type": "action",
                "uses": "fixture.ocr@1",
                "with": {"text": "Session expired", "confidence": 0.98},
                "effect": {"class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
            },
            {
                "id": "respond",
                "type": "if",
                "condition": "${{ steps.read_dialog.output.text == 'Session expired' }}",
                "then": [
                    {
                        "id": "dismiss_dialog",
                        "type": "action",
                        "uses": "fixture.invoke@1",
                        "with": {"operation": "dismiss", "target": {"role": "button"}},
                        "effect": {"class": "idempotent"},
                        "risk": {"category": "navigate", "level": "low"},
                    }
                ],
                "else": [{"id": "leave_dialog", "type": "return", "value": False}],
            },
            {"id": "finish", "type": "return", "value": "${{ steps.dismiss_dialog.output.invoked }}"},
        )

        result = run_descriptor(
            compile_descriptor(raw),
            plugins={"fixture": [sys.executable, str(FIXTURE_PLUGIN)]},
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertIs(result.output, True)
        self.assertEqual(result.steps["read_dialog"]["status"], "succeeded")
        self.assertEqual(result.steps["dismiss_dialog"]["status"], "succeeded")
        self.assertNotIn("leave_dialog", result.steps)


class ScriptStepTests(unittest.TestCase):
    def script_workflow(self, source: str, *, timeout: str = "1s") -> object:
        return compile_descriptor(
            workflow(
                {
                    "id": "compute",
                    "type": "script",
                    "runtime": "python",
                    "source": source,
                    "inputs": {"value": 21},
                    "output_schema": {
                        "type": "object",
                        "required": ["answer"],
                        "properties": {"answer": {"type": "integer"}},
                        "additionalProperties": False,
                    },
                    "timeout": timeout,
                },
                {"id": "finish", "type": "return", "value": "${{ steps.compute.output }}"},
            )
        )

    def test_scripts_are_denied_without_explicit_gate(self) -> None:
        plan = self.script_workflow(
            "import json, sys\npayload = json.load(sys.stdin)\njson.dump({'answer': payload['value'] * 2}, sys.stdout)\n"
        )

        result = WorkflowRunner(plan).run()

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "SCRIPT.SANDBOX_DENIED")

    def test_script_runs_only_after_explicit_gate_in_available_sandbox(self) -> None:
        plan = self.script_workflow(
            "import json, sys\npayload = json.load(sys.stdin)\njson.dump({'answer': payload['value'] * 2}, sys.stdout)\n"
        )

        result = WorkflowRunner(plan, allow_scripts=True).run()

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.output, {"answer": 42})

    def test_script_rejects_multiple_json_values(self) -> None:
        plan = self.script_workflow(
            "print('{\"answer\": 1}')\nprint('{\"answer\": 2}')\n"
        )

        result = WorkflowRunner(plan, allow_scripts=True).run()

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "SCRIPT.OUTPUT_INVALID")

    def test_sandboxed_script_timeout_is_distinct(self) -> None:
        plan = self.script_workflow(
            "import time\ntime.sleep(2)\nprint('{\"answer\": 1}')\n", timeout="20ms"
        )

        result = WorkflowRunner(plan, allow_scripts=True).run()

        self.assertEqual(result.status, "timed_out")
        self.assertEqual(result.error.code, "SCRIPT.TIMEOUT")


if __name__ == "__main__":
    unittest.main()
