"""Focused regressions for runtime capability and deadline contracts."""

from __future__ import annotations

from copy import deepcopy
import sys
import time
import unittest

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.plugin import PluginError, ProcessPlugin
from ai_auto_desktop.runtime import _version_matches, run_descriptor


def workflow(
    *steps: dict[str, object], max_duration: str = "1s"
) -> dict[str, object]:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "runtime-contract-regression"},
        "budgets": {
            "max_duration": max_duration,
            "max_executed_steps": 100,
            "cleanup_timeout": "1s",
        },
        "steps": list(steps),
    }


def action_step(**overrides: object) -> dict[str, object]:
    step: dict[str, object] = {
        "id": "invoke",
        "type": "action",
        "uses": "fixture.invoke@1",
        "with": {"value": 1},
        "effect": {"class": "read_only"},
        "risk": {"category": "observe", "level": "low"},
    }
    step.update(overrides)
    return step


def action_contract(**overrides: object) -> dict[str, object]:
    contract: dict[str, object] = {
        "contract_major": 1,
        "effect": {"default_class": "read_only"},
        "risk": {"category": "observe", "level": "low"},
        "input_schema": {"type": "object"},
        "output_schema": {"type": "object"},
    }
    contract.update(overrides)
    return contract


def manifest(
    *,
    name: str = "fixture",
    version: str = "1.2.3",
    permissions: list[str] | None = None,
    contract: dict[str, object] | None = None,
) -> dict[str, object]:
    value: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": name, "version": version},
        "actions": {"invoke": contract or action_contract()},
    }
    if permissions is not None:
        value["permissions"] = permissions
    return value


class RecordingPlugin(ProcessPlugin):
    def __init__(
        self, manifest_value: dict[str, object], *, start_delay: float = 0
    ) -> None:
        super().__init__([sys.executable, "-c", "pass"], name="recording")
        self.manifest_value = manifest_value
        self.start_delay = start_delay
        self.start_calls = 0
        self.invoke_calls = 0
        self.invoke_timeouts: list[float | None] = []
        self.manifest = deepcopy(manifest_value)

    def start(self, timeout: float | None = None) -> dict[str, object]:
        self.start_calls += 1
        if self.start_delay:
            time.sleep(self.start_delay)
        self.manifest = deepcopy(self.manifest_value)
        return self.manifest

    def invoke(
        self, action: str, args: object, timeout: float | None = None
    ) -> object:
        self.invoke_calls += 1
        self.invoke_timeouts.append(timeout)
        return {"ok": True}


class RuntimeDeadlineContractTests(unittest.TestCase):
    def test_workflow_deadline_bounds_requirement_manifest_handshake(self) -> None:
        code = "import time; time.sleep(5)"
        plugin = ProcessPlugin(
            [sys.executable, "-c", code], timeout=2, name="slow"
        )
        self.addCleanup(plugin.close)
        raw = workflow(
            {"id": "done", "type": "return"},
            max_duration="50ms",
        )
        raw["requires"] = {
            "capabilities": [
                {"name": "fixture", "version": "^1.0.0"}
            ]
        }

        started = time.monotonic()
        result = run_descriptor(
            compile_descriptor(raw), plugins={"fixture": plugin}
        )
        elapsed = time.monotonic() - started

        self.assertEqual(result.status, "timed_out")
        self.assertEqual(result.error.code, "WORKFLOW.TIMEOUT")
        self.assertLess(elapsed, 1.5)
        self.assertNotIn("done", result.steps)

    def test_manifest_action_timeout_limits_invocation(self) -> None:
        plugin = RecordingPlugin(
            manifest(contract=action_contract(timeout="40ms"))
        )
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(workflow(action_step(timeout="500ms"))),
            plugins={"fixture": plugin},
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(plugin.invoke_calls, 1)
        self.assertEqual(len(plugin.invoke_timeouts), 1)
        self.assertGreater(plugin.invoke_timeouts[0], 0)
        self.assertLessEqual(plugin.invoke_timeouts[0], 0.04)

    def test_manifest_action_timeout_stops_a_slow_process_action(self) -> None:
        code = r'''
import json, sys, time
manifest = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture", "version": "1.0.0"},
    "actions": {
        "invoke": {
            "contract_major": 1,
            "effect": {"default_class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
            "timeout": "40ms",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"}
        }
    }
}
for line in sys.stdin:
    request = json.loads(line)
    if request["type"] == "manifest":
        print(json.dumps({"id": request["id"], "manifest": manifest}), flush=True)
    else:
        time.sleep(1)
        print(json.dumps({"id": request["id"], "result": {"ok": True}}), flush=True)
'''
        plugin = ProcessPlugin(
            [sys.executable, "-c", code], timeout=2, name="slow-action"
        )
        self.addCleanup(plugin.close)

        started = time.monotonic()
        result = run_descriptor(
            compile_descriptor(workflow(action_step(timeout="500ms"))),
            plugins={"fixture": plugin},
        )
        elapsed = time.monotonic() - started

        self.assertEqual(result.status, "timed_out")
        self.assertEqual(result.error.code, "ACTION.TIMEOUT")
        self.assertLess(elapsed, 0.8)

    def test_action_timeout_includes_contract_handshake_and_invoke(self) -> None:
        code = r'''
import json, sys, time
manifest = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {"name": "fixture", "version": "1.0.0"},
    "actions": {
        "invoke": {
            "contract_major": 1,
            "effect": {"default_class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
            "timeout": "100ms",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"}
        }
    }
}
for line in sys.stdin:
    request = json.loads(line)
    if request["type"] == "manifest":
        time.sleep(0.06)
        print(json.dumps({"id": request["id"], "manifest": manifest}), flush=True)
    else:
        time.sleep(0.06)
        print(json.dumps({"id": request["id"], "result": {"ok": True}}), flush=True)
'''
        plugin = ProcessPlugin(
            [sys.executable, "-c", code], timeout=2, name="shared-budget"
        )
        self.addCleanup(plugin.close)

        started = time.monotonic()
        result = run_descriptor(
            compile_descriptor(workflow(action_step(timeout="500ms"))),
            plugins={"fixture": plugin},
        )
        elapsed = time.monotonic() - started

        self.assertEqual(result.status, "timed_out")
        self.assertEqual(result.error.code, "ACTION.TIMEOUT")
        self.assertGreaterEqual(elapsed, 0.08)
        self.assertLess(elapsed, 0.5)


class RuntimeSemVerContractTests(unittest.TestCase):
    def test_caret_ranges_follow_leftmost_nonzero_component(self) -> None:
        cases = [
            ("1.2.3", "^1.2.3", True),
            ("1.9.9", "^1.2.3", True),
            ("2.0.0", "^1.2.3", False),
            ("0.2.9", "^0.2.3", True),
            ("0.3.0", "^0.2.3", False),
            ("0.0.3", "^0.0.3", True),
            ("0.0.4", "^0.0.3", False),
        ]
        for version, constraint, expected in cases:
            with self.subTest(version=version, constraint=constraint):
                self.assertIs(
                    _version_matches(version, constraint), expected
                )

    def test_stable_range_does_not_admit_prerelease(self) -> None:
        self.assertFalse(_version_matches("1.1.0-beta.1", ">=1.0.0"))
        self.assertTrue(
            _version_matches(
                "1.1.0-beta.2", ">=1.1.0-beta.1 <1.1.0"
            )
        )
        self.assertTrue(_version_matches("1.2.3+build.2", "1.2.3"))


class RuntimeFailClosedContractTests(unittest.TestCase):
    def test_unknown_effect_is_never_retried_for_idempotent_action(self) -> None:
        contract = action_contract(
            effect={"default_class": "idempotent"},
            errors=[
                {
                    "code": "DRIVER.ACTION_FAILED",
                    "retryable": True,
                    "effect": "unknown",
                }
            ],
        )
        plugin = RecordingPlugin(manifest(contract=contract))
        self.addCleanup(plugin.close)

        def fail(
            action: str, args: object, timeout: float | None = None
        ) -> object:
            plugin.invoke_calls += 1
            raise PluginError(
                "DRIVER.ACTION_FAILED",
                "native action outcome is unknown",
                details={"dispatched": True},
                retryable=True,
            )

        plugin.invoke = fail  # type: ignore[method-assign]
        step = action_step(
            effect={"class": "idempotent"},
            retry={
                "max_attempts": 3,
                "on": {"codes": ["DRIVER.ACTION_FAILED"]},
            },
        )

        result = run_descriptor(
            compile_descriptor(workflow(step)), plugins={"fixture": plugin}
        )

        self.assertEqual(result.status, "unknown_effect")
        self.assertEqual(result.error.effect, "unknown")
        self.assertEqual(plugin.invoke_calls, 1)

    def test_declared_plugin_unknown_effect_reaches_run_status(self) -> None:
        contract = action_contract(
            effect={"default_class": "non_idempotent"},
            errors=[
                {
                    "code": "DRIVER.ACTION_FAILED",
                    "retryable": False,
                    "effect": "unknown",
                }
            ],
        )
        plugin = RecordingPlugin(manifest(contract=contract))
        self.addCleanup(plugin.close)

        def fail(
            action: str, args: object, timeout: float | None = None
        ) -> object:
            plugin.invoke_calls += 1
            from ai_auto_desktop.plugin import PluginError

            raise PluginError(
                "DRIVER.ACTION_FAILED",
                "native action outcome is unknown",
                details={"dispatched": True},
            )

        plugin.invoke = fail  # type: ignore[method-assign]
        raw = workflow(
            action_step(effect={"class": "non_idempotent"})
        )
        result = run_descriptor(
            compile_descriptor(raw), plugins={"fixture": plugin}
        )

        self.assertEqual(result.status, "unknown_effect")
        self.assertEqual(result.error.code, "DRIVER.ACTION_FAILED")
        self.assertEqual(result.error.effect, "unknown")
        self.assertEqual(plugin.invoke_calls, 1)

    def test_foreach_rejects_unsupported_concurrency_before_body(self) -> None:
        raw = workflow(
            {
                "id": "parallel",
                "type": "foreach",
                "items": "${{ [1, 2] }}",
                "as": "item",
                "max_items": 2,
                "concurrency": 2,
                "steps": [action_step(id="body")],
            }
        )
        plugin = RecordingPlugin(manifest())
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(raw), plugins={"fixture": plugin}
        )

        self.assertEqual(result.error.code, "DESCRIPTOR.UNSUPPORTED_FEATURE")
        self.assertEqual(plugin.invoke_calls, 0)
        self.assertNotIn("body", result.steps)

    def test_confirmation_without_allowed_risk_blocks_dispatch(self) -> None:
        raw = workflow(
            action_step(risk={"category": "send", "level": "high"})
        )
        raw["policy"] = {
            "confirmation": {"required_for": {"categories": ["send"]}}
        }
        plugin = RecordingPlugin(manifest())
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(raw), plugins={"fixture": plugin}
        )

        self.assertEqual(result.error.code, "POLICY.CONFIRMATION_REQUIRED")
        self.assertEqual(plugin.invoke_calls, 0)

    def test_requirement_action_input_risk_permission_matrix(self) -> None:
        base = workflow(action_step())
        base["requires"] = {
            "capabilities": [
                {
                    "name": "fixture",
                    "version": "^1.2.3",
                    "actions": ["invoke"],
                }
            ]
        }
        cases: list[tuple[str, dict[str, object], dict[str, object], list[str], str]] = []

        wrong_name = deepcopy(base)
        cases.append(("name", wrong_name, manifest(name="other"), [], "CAPABILITY.MISSING"))
        cases.append(("version", deepcopy(base), manifest(version="2.0.0"), [], "CAPABILITY.VERSION_INCOMPATIBLE"))
        missing_action = manifest()
        missing_action["actions"] = {"other": action_contract()}
        cases.append(("required action", deepcopy(base), missing_action, [], "CAPABILITY.MISSING"))

        unknown_action = deepcopy(base)
        unknown_action.pop("requires")
        unknown_action["steps"][0]["uses"] = "fixture.other@1"
        cases.append(("action name", unknown_action, manifest(), [], "CAPABILITY.MISSING"))
        wrong_major = deepcopy(base)
        wrong_major.pop("requires")
        wrong_major["steps"][0]["uses"] = "fixture.invoke@2"
        cases.append(("contract major", wrong_major, manifest(), [], "CAPABILITY.VERSION_INCOMPATIBLE"))

        bad_input = deepcopy(base)
        bad_input.pop("requires")
        bad_contract = action_contract(
            input_schema={"type": "object", "required": ["required"]}
        )
        cases.append(("input", bad_input, manifest(contract=bad_contract), [], "ACTION.INPUT_INVALID"))

        provider_permission = deepcopy(base)
        provider_permission["requires"]["permissions"] = ["desktop.observe"]
        cases.append(("provider permission grant", provider_permission, manifest(permissions=["desktop.observe"]), [], "POLICY.DENIED"))
        undeclared_permission = deepcopy(base)
        cases.append(("provider permission declaration", undeclared_permission, manifest(permissions=["desktop.observe"]), ["desktop.observe"], "POLICY.DENIED"))

        action_permission = deepcopy(base)
        action_permission["requires"]["permissions"] = ["desktop.input"]
        permission_contract = action_contract(permissions=["desktop.input"])
        cases.append(("action permission", action_permission, manifest(contract=permission_contract), [], "POLICY.DENIED"))

        provider_risk = deepcopy(base)
        provider_risk["policy"] = {"allowed_risk": {"categories": ["observe"], "max_level": "low"}}
        high_contract = action_contract(risk={"category": "modify", "level": "high"})
        cases.append(("provider risk", provider_risk, manifest(contract=high_contract), [], "POLICY.DENIED"))

        declared_risk = deepcopy(base)
        declared_risk["steps"][0]["risk"] = {"category": "send", "level": "high"}
        declared_risk["policy"] = {"allowed_risk": {"categories": ["observe"], "max_level": "low"}}
        cases.append(("declared risk", declared_risk, manifest(), [], "POLICY.DENIED"))

        for name, raw, manifest_value, grants, code in cases:
            with self.subTest(name=name):
                plugin = RecordingPlugin(manifest_value)
                self.addCleanup(plugin.close)
                result = run_descriptor(
                    compile_descriptor(raw),
                    plugins={"fixture": plugin},
                    granted_permissions=grants,
                )
                self.assertEqual(result.error.code, code, result.to_dict())
                self.assertEqual(plugin.invoke_calls, 0)

    def test_complete_contract_matrix_dispatches_once(self) -> None:
        raw = workflow(action_step())
        raw["requires"] = {
            "capabilities": [
                {
                    "name": "fixture",
                    "version": "^1.2.3",
                    "actions": ["invoke"],
                }
            ],
            "permissions": ["desktop.observe", "desktop.input"],
        }
        raw["policy"] = {
            "allowed_risk": {
                "categories": ["observe"],
                "max_level": "low",
            }
        }
        contract = action_contract(permissions=["desktop.input"])
        plugin = RecordingPlugin(
            manifest(permissions=["desktop.observe"], contract=contract)
        )
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(raw),
            plugins={"fixture": plugin},
            granted_permissions=["desktop.observe", "desktop.input"],
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(plugin.invoke_calls, 1)


if __name__ == "__main__":
    unittest.main()
