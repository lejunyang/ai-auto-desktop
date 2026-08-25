"""Runtime-only contracts for opt-in durable read-only actions."""

from __future__ import annotations

from copy import deepcopy
import json
import re
import time
import unittest
from unittest import mock

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.errors import AutomationError
from ai_auto_desktop.plugin import PluginError, ProcessPlugin
from ai_auto_desktop.runtime import DurableActionBinding, WorkflowRunner


INPUT_CANARY = "INPUT-CANARY-0b71e8"
RAW_CANARY = "RAW-CANARY-4f903c"
SAFE_VALUE = "safe-title"
PUBLIC = {"input": "public", "output": "public", "error": "public"}


def action_step(
    *,
    action_input: dict[str, object] | None = None,
    sensitivity: dict[str, str] | None = None,
    checkpoint: dict[str, object] | None = None,
    effect: str = "read_only",
    postcondition: dict[str, object] | None = None,
    timeout: str | None = None,
    attempt_timeout: str | None = None,
) -> dict[str, object]:
    step: dict[str, object] = {
        "id": "observe",
        "type": "action",
        "uses": "fixture.read@1",
        "with": action_input or {},
        "effect": {"class": effect},
        "risk": {"category": "observe", "level": "low"},
        "sensitivity": deepcopy(sensitivity if sensitivity is not None else PUBLIC),
        "checkpoint": checkpoint
        or {"output": {"mode": "project", "fields": ["title"]}},
    }
    if postcondition is not None:
        step["postcondition"] = postcondition
    if timeout is not None:
        step["timeout"] = timeout
    if attempt_timeout is not None:
        step["attempt_timeout"] = attempt_timeout
    return step


def workflow(
    step: dict[str, object] | None = None,
    *,
    inputs: dict[str, object] | None = None,
    outputs: dict[str, object] | None = None,
    max_duration: str = "10s",
    max_steps: int = 3,
) -> object:
    raw: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "durable-runtime"},
        "budgets": {
            "max_duration": max_duration,
            "max_executed_steps": max_steps,
            "cleanup_timeout": "1s",
            "max_concurrency": 1,
        },
        "steps": [step or action_step()],
    }
    if inputs is not None:
        raw["inputs"] = inputs
    if outputs is not None:
        raw["outputs"] = outputs
    return compile_descriptor(raw)


def checkpoint_field(
    pointer: str,
    *,
    schema: object | None = None,
    missing: str = "error",
) -> dict[str, object]:
    return {
        "pointer": pointer,
        "schema": schema or {"type": "string"},
        "missing": missing,
    }


def action_contract(
    *,
    effect: str = "read_only",
    errors: list[dict[str, object]] | None = None,
    sensitivity: dict[str, str] | None = None,
    fields: dict[str, dict[str, object]] | None = None,
    input_schema: object | None = None,
    output_schema: object | None = None,
    timeout: str | None = None,
) -> dict[str, object]:
    contract: dict[str, object] = {
        "contract_major": 1,
        "effect": {"default_class": effect},
        "risk": {"category": "observe", "level": "low"},
        "input_schema": input_schema or {"type": "object"},
        "output_schema": output_schema or {"type": "object"},
        "errors": (
            errors
            if errors is not None
            else [{
                "code": "FIXTURE.NOT_READY",
                "retryable": False,
                "effect": "not_applied",
            }]
        ),
        "sensitivity": deepcopy(sensitivity if sensitivity is not None else PUBLIC),
        "durability": {
            "checkpoint_fields": fields
            if fields is not None
            else {"title": checkpoint_field("/public/title")}
        },
    }
    if timeout is not None:
        contract["timeout"] = timeout
    return contract


def manifest(contract: dict[str, object] | None = None) -> dict[str, object]:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": "fixture", "version": "1.0.0"},
        "actions": {"read": contract or action_contract()},
    }


class StubPlugin(ProcessPlugin):
    def __init__(
        self,
        manifest_value: object,
        outcome: object | BaseException | None = None,
    ) -> None:
        super().__init__(["unused"], name="fixture")
        self.manifest_value = deepcopy(manifest_value)
        self.manifest = (
            deepcopy(manifest_value)
            if isinstance(manifest_value, dict)
            else None
        )
        self.outcome = (
            {"public": {"title": SAFE_VALUE}, "secret": RAW_CANARY}
            if outcome is None
            else outcome
        )
        self.calls: list[tuple[str, object, float | None]] = []

    def start(self, timeout: float | None = None) -> object:
        return deepcopy(self.manifest_value)

    def invoke(
        self, action: str, args: object, timeout: float | None = None
    ) -> object:
        self.calls.append((action, deepcopy(args), timeout))
        if isinstance(self.outcome, BaseException):
            raise self.outcome
        return deepcopy(self.outcome)


def durable_runner(
    plan: object, plugin: ProcessPlugin, *, inputs: dict[str, object] | None = None
) -> tuple[WorkflowRunner, DurableActionBinding]:
    runner = WorkflowRunner(
        plan, plugins={"fixture": plugin}, durable_action_mode="read_only"
    )
    runner.initialize(inputs)
    return runner, runner.durable_action_binding(plan.steps[0])


def execute_durable(
    plan: object, plugin: ProcessPlugin, *, inputs: dict[str, object] | None = None,
    deadline: int | None = None,
) -> tuple[WorkflowRunner, object, object]:
    runner, binding = durable_runner(plan, plugin, inputs=inputs)
    runner.prepare_segment()
    runner.reserve_prepared_action_attempt()
    segment = runner.run_durable_action_segment(binding, deadline)
    result = runner.finalize()
    return runner, segment, result


class DurableReadonlyRuntimeTests(unittest.TestCase):
    def test_mode_is_explicit_and_default_run_behavior_is_unchanged(self) -> None:
        plan = workflow(
            outputs={
                "raw": {"value": "${{ steps.observe.output.secret }}"}
            }
        )
        plugin = StubPlugin(manifest())

        ordinary = WorkflowRunner(plan, plugins={"fixture": plugin})
        with self.assertRaises(AutomationError) as denied:
            ordinary.durable_action_binding(plan.steps[0])
        self.assertEqual(denied.exception.code, "DURABLE.UNSUPPORTED_PLAN")
        result = ordinary.run()
        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.output, {"raw": RAW_CANARY})
        with self.assertRaises(ValueError):
            WorkflowRunner(plan, durable_action_mode="sometimes")

    def test_binding_uses_canonical_manifest_and_returns_stable_digests(self) -> None:
        plan = workflow()
        plugin = StubPlugin(manifest())
        runner, first = durable_runner(plan, plugin)
        second = runner.durable_action_binding(plan.steps[0])

        self.assertIsInstance(first, DurableActionBinding)
        self.assertEqual(first, second)
        for digest in (
            first.provider_digest, first.contract_digest, first.projection_digest
        ):
            self.assertRegex(digest, r"^sha256:[0-9a-f]{64}$")
        with self.assertRaises(TypeError):
            first.contract["timeout"] = "1s"  # type: ignore[index]

    def test_manifest_fallback_is_forbidden(self) -> None:
        plan = workflow()
        plugin = StubPlugin(None)
        runner = WorkflowRunner(
            plan, plugins={"fixture": plugin}, durable_action_mode="read_only"
        )
        runner.initialize()

        with self.assertRaises(AutomationError) as rejected:
            runner.durable_action_binding(plan.steps[0])
        self.assertEqual(rejected.exception.code, "DURABLE.UNSUPPORTED_PLAN")
        self.assertEqual(plugin.calls, [])

    def test_effect_errors_and_sensitivity_fail_closed(self) -> None:
        cases: list[tuple[str, dict[str, object], dict[str, object], str]] = []
        cases.append(("provider_effect", action_step(), action_contract(effect="idempotent"), "DURABLE.UNSUPPORTED_PLAN"))
        cases.append(("descriptor_effect", action_step(effect="idempotent"), action_contract(), "DURABLE.UNSUPPORTED_PLAN"))
        cases.append(("empty_errors", action_step(), action_contract(errors=[]), "DURABLE.UNSUPPORTED_PLAN"))
        cases.append(("unsafe_error", action_step(), action_contract(errors=[{"code": "FIXTURE.BAD", "retryable": False, "effect": "unknown"}]), "DURABLE.UNSUPPORTED_PLAN"))
        cases.append(("provider_sensitive", action_step(), action_contract(sensitivity={"input": "public", "output": "sensitive", "error": "public"}), "DURABLE.SENSITIVE_ACTION"))
        cases.append(("descriptor_default_sensitive", action_step(sensitivity={"input": "public", "output": "public"}), action_contract(), "DURABLE.SENSITIVE_ACTION"))

        for name, step, contract, code in cases:
            with self.subTest(name=name):
                plugin = StubPlugin(manifest(contract))
                plan = workflow(step)
                runner = WorkflowRunner(plan, plugins={"fixture": plugin}, durable_action_mode="read_only")
                runner.initialize()
                with self.assertRaises(AutomationError) as rejected:
                    runner.durable_action_binding(plan.steps[0])
                self.assertEqual(rejected.exception.code, code)
                self.assertEqual(plugin.calls, [])

    def test_checkpoint_alias_whitelist_and_postcondition_are_enforced(self) -> None:
        unknown = workflow(action_step(checkpoint={"output": {"mode": "project", "fields": ["unknown"]}}))
        postcondition = workflow(action_step(postcondition={"condition": "${{ True }}"}))
        omit = workflow(action_step(checkpoint={"output": {"mode": "omit"}}))

        for plan in (unknown, postcondition):
            runner = WorkflowRunner(plan, plugins={"fixture": StubPlugin(manifest())}, durable_action_mode="read_only")
            runner.initialize()
            with self.assertRaises(AutomationError) as rejected:
                runner.durable_action_binding(plan.steps[0])
            self.assertEqual(rejected.exception.code, "DURABLE.UNSUPPORTED_PLAN")
        runner, binding = durable_runner(omit, StubPlugin(manifest(action_contract(fields={}))))
        self.assertEqual(binding.to_dict()["projectionDigest"][:7], "sha256:")
        self.assertEqual(runner.context["steps"], {})

    def test_pointer_constraints_duplicate_and_null_schema_are_enforced(self) -> None:
        invalid_fields = (
            {"root": checkpoint_field("")},
            {"bad": checkpoint_field("/bad~2escape")},
            {"long": checkpoint_field("/" + "x" * 1024)},
            {"deep": checkpoint_field("/" + "/".join(["x"] * 65))},
            {"one": checkpoint_field("/same"), "two": checkpoint_field("/same")},
            {"nullable": checkpoint_field("/missing", missing="null")},
        )
        for fields in invalid_fields:
            with self.subTest(fields=fields):
                field = next(iter(fields))
                plan = workflow(action_step(checkpoint={"output": {"mode": "project", "fields": [field]}}))
                runner = WorkflowRunner(plan, plugins={"fixture": StubPlugin(manifest(action_contract(fields=fields)))}, durable_action_mode="read_only")
                runner.initialize()
                with self.assertRaises(AutomationError) as rejected:
                    runner.durable_action_binding(plan.steps[0])
                self.assertEqual(rejected.exception.code, "DURABLE.UNSUPPORTED_PLAN")

    def test_rfc6901_projection_handles_escapes_empty_token_unicode_and_array(self) -> None:
        fields = {
            "escaped": checkpoint_field("/a~1b/~0key/0"),
            "empty": checkpoint_field("/"),
            "unicode": checkpoint_field("/你好"),
        }
        step = action_step(checkpoint={"output": {"mode": "project", "fields": ["escaped", "empty", "unicode"]}})
        plan = workflow(step, outputs={"view": {"value": "${{ steps.observe.output }}"}})
        raw = {"a/b": {"~key": [SAFE_VALUE]}, "": "empty", "你好": "unicode", "secret": RAW_CANARY}
        runner, segment, result = execute_durable(plan, StubPlugin(manifest(action_contract(fields=fields)), raw))

        expected = {"escaped": SAFE_VALUE, "empty": "empty", "unicode": "unicode"}
        self.assertEqual(result.output, {"view": expected})
        self.assertEqual(segment.state.context_steps["observe"]["output"], expected)
        self.assertNotIn(RAW_CANARY, json.dumps({"state": segment.state.to_dict(), "result": result.to_dict(), "context": runner.context}, ensure_ascii=False))

    def test_missing_omit_and_null_are_projected_without_noncanonical_array_indexes(self) -> None:
        fields = {
            "first": checkpoint_field("/items/0"),
            "leading_zero": checkpoint_field("/items/01", missing="omit"),
            "dash": checkpoint_field("/items/-", schema={"type": ["string", "null"]}, missing="null"),
        }
        step = action_step(checkpoint={"output": {"mode": "project", "fields": list(fields)}})
        plan = workflow(step)
        _, segment, _ = execute_durable(plan, StubPlugin(manifest(action_contract(fields=fields)), {"items": [SAFE_VALUE]}))

        self.assertEqual(segment.state.context_steps["observe"]["output"], {"first": SAFE_VALUE, "dash": None})

    def test_missing_error_and_projection_schema_failure_are_redacted(self) -> None:
        cases = (
            (action_contract(fields={"title": checkpoint_field("/missing")}), {"secret": RAW_CANARY}),
            (action_contract(fields={"title": checkpoint_field("/public/title")}), {"public": {"title": {"secret": RAW_CANARY}}}),
        )
        for contract, raw in cases:
            with self.subTest(raw=raw):
                _, _, result = execute_durable(workflow(), StubPlugin(manifest(contract), raw))
                self.assertEqual(result.error.code, "ACTION.OUTPUT_INVALID")
                self.assertNotIn(RAW_CANARY, json.dumps(result.to_dict(), sort_keys=True))
                self.assertEqual(result.error.details, {})
                self.assertIsNone(result.error.cause)
                self.assertEqual(result.error.suppressed, [])

    def test_binding_digest_hashes_evaluated_input_without_returning_it(self) -> None:
        plan = workflow(action_step(action_input={"token": "${{ inputs.token }}"}), inputs={"token": {"schema": {"type": "string"}, "required": True}})
        plugin = StubPlugin(manifest())
        first, binding = durable_runner(plan, plugin, inputs={"token": INPUT_CANARY})
        digest = first.durable_action_binding_digest(plan.steps[0], binding)
        second, second_binding = durable_runner(plan, StubPlugin(manifest()), inputs={"token": "other"})
        other = second.durable_action_binding_digest(plan.steps[0], second_binding)

        self.assertRegex(digest, r"^sha256:[0-9a-f]{64}$")
        self.assertNotEqual(digest, other)
        self.assertNotIn(INPUT_CANARY, json.dumps(binding.to_dict()))

    def test_deadline_layers_are_absolute_and_take_the_tightest_bound(self) -> None:
        base = 1_800_000_000.0
        plan = workflow(action_step(timeout="8s", attempt_timeout="5s"), max_duration="10s")
        plugin = StubPlugin(manifest(action_contract(timeout="3s")))
        runner = WorkflowRunner(plan, plugins={"fixture": plugin}, durable_action_mode="read_only")
        with mock.patch("ai_auto_desktop.runtime.time.time", return_value=base):
            runner.initialize(deadline_epoch_ms=int((base + 2) * 1000))
            binding = runner.durable_action_binding(plan.steps[0])
            deadlines = runner.durable_action_deadlines(plan.steps[0], binding)

        self.assertEqual(deadlines["workflowDeadlineEpochMs"], int((base + 2) * 1000))
        self.assertEqual(deadlines["stepDeadlineEpochMs"], int((base + 8) * 1000))
        self.assertEqual(deadlines["attemptDeadlineEpochMs"], int((base + 2) * 1000))
        self.assertEqual(deadlines["providerDeadlineEpochMs"], int((base + 2) * 1000))
        self.assertEqual(deadlines["dispatchDeadlineEpochMs"], int((base + 2) * 1000))

    def test_reservation_restore_consumes_exactly_one_attempt(self) -> None:
        plan = workflow(max_steps=1)
        plugin = StubPlugin(manifest())
        runner, binding = durable_runner(plan, plugin)
        runner.prepare_segment()
        reserved = runner.reserve_prepared_action_attempt()
        intent = runner.export_action_intent_state()
        self.assertEqual((reserved.executed_attempts, intent.executed_attempts), (1, 1))
        released = runner.release_prepared_action_attempt()
        self.assertEqual(released.executed_attempts, 0)

        runner.reserve_prepared_action_attempt()
        intent = runner.export_action_intent_state()
        restored = WorkflowRunner(plan, plugins={"fixture": plugin}, durable_action_mode="read_only")
        restored_state = restored.restore_action_intent(intent.to_dict())
        self.assertEqual(restored_state.executed_attempts, 1)
        segment = restored.run_durable_action_segment(binding, int((time.time() + 1) * 1000))
        self.assertEqual(segment.state.executed_attempts, 1)
        self.assertEqual(len(plugin.calls), 1)

        invalid = intent.to_dict(); invalid["executedAttempts"] = 0
        with self.assertRaises(AutomationError):
            WorkflowRunner(plan, plugins={"fixture": plugin}, durable_action_mode="read_only").restore_action_intent(invalid)

    def test_expired_dispatch_deadline_never_invokes_provider(self) -> None:
        plan = workflow()
        plugin = StubPlugin(manifest())
        runner, binding = durable_runner(plan, plugin)
        runner.prepare_segment(); runner.reserve_prepared_action_attempt()
        segment = runner.run_durable_action_segment(binding, 1)
        result = runner.finalize()

        self.assertTrue(segment.terminal_ready)
        self.assertEqual(result.status, "timed_out")
        self.assertEqual(result.error.code, "WORKFLOW.TIMEOUT")
        self.assertEqual(plugin.calls, [])

    def test_declared_undeclared_host_and_schema_errors_are_stably_redacted(self) -> None:
        cases = (
            (PluginError("FIXTURE.NOT_READY", RAW_CANARY, details={"raw": RAW_CANARY}), "FIXTURE.NOT_READY", "failed"),
            (PluginError("FIXTURE.SECRET", RAW_CANARY, details={"raw": RAW_CANARY}), "ACTION.UNDECLARED_ERROR", "unknown_effect"),
            (PluginError("PLUGIN.HOST_TIMEOUT", RAW_CANARY, details={"raw": RAW_CANARY, "dispatched": True}, retryable=True), "ACTION.TIMEOUT", "timed_out"),
        )
        for failure, code, status in cases:
            with self.subTest(code=code):
                _, _, result = execute_durable(workflow(), StubPlugin(manifest(), failure))
                self.assertEqual((result.error.code, result.status), (code, status))
                payload = result.error.to_dict()
                self.assertNotIn(RAW_CANARY, json.dumps(payload, sort_keys=True))
                self.assertEqual(payload["details"], {})
                self.assertIsNone(payload["cause"])
                self.assertEqual(payload["suppressed"], [])

    def test_omit_never_publishes_raw_provider_output(self) -> None:
        step = action_step(checkpoint={"output": {"mode": "omit"}})
        plan = workflow(step, outputs={"view": {"value": "${{ steps.observe.output }}"}})
        plugin = StubPlugin(manifest(action_contract(fields={})))
        runner, segment, result = execute_durable(plan, plugin)

        self.assertEqual(result.output, {"view": None})
        self.assertIsNone(segment.state.context_steps["observe"]["output"])
        self.assertNotIn(RAW_CANARY, json.dumps({"state": segment.state.to_dict(), "result": result.to_dict(), "events": runner.events}, sort_keys=True))


if __name__ == "__main__":
    unittest.main()
