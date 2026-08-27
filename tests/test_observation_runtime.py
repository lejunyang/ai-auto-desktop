"""Runtime contracts for explicit postcondition observations."""

from __future__ import annotations

from collections import defaultdict, deque
from copy import deepcopy
import sys
import time
import unittest
from typing import Any
from unittest.mock import patch

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.plugin import PluginError, ProcessPlugin
from ai_auto_desktop.runtime import WorkflowRunner, run_descriptor


def action_contract(
    *,
    effect: str = "read_only",
    timeout: str | None = None,
    input_schema: object = True,
    output_schema: object = True,
    permissions: list[str] | None = None,
    errors: list[dict[str, object]] | None = None,
    risk: dict[str, str] | None = None,
) -> dict[str, object]:
    contract: dict[str, object] = {
        "contract_major": 1,
        "effect": {"default_class": effect},
        "risk": risk or {"category": "observe", "level": "low"},
        "input_schema": input_schema,
        "output_schema": output_schema,
    }
    if timeout is not None:
        contract["timeout"] = timeout
    if permissions is not None:
        contract["permissions"] = permissions
    if errors is not None:
        contract["errors"] = errors
    return contract


def manifest(
    name: str,
    actions: dict[str, dict[str, object]],
    *,
    permissions: list[str] | None = None,
    platforms: list[str] | None = None,
) -> dict[str, object]:
    value: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": name, "version": "1.0.0"},
        "actions": actions,
    }
    if permissions is not None:
        value["permissions"] = permissions
    if platforms is not None:
        value["runtime"] = {"platforms": platforms}
    return value


def workflow(
    step: dict[str, object],
    *,
    max_steps: int = 10,
    permissions: list[str] | None = None,
    policy: dict[str, object] | None = None,
) -> dict[str, object]:
    raw: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "observation-runtime"},
        "budgets": {
            "max_duration": "2s",
            "max_executed_steps": max_steps,
            "cleanup_timeout": "1s",
        },
        "steps": [step],
    }
    if permissions is not None:
        raw["requires"] = {"permissions": permissions}
    if policy is not None:
        raw["policy"] = policy
    return raw


def observed_action(
    *,
    main_effect: str = "read_only",
    timeout: str | None = "100ms",
    poll_interval: str = "1ms",
    condition: str = "${{ observation.ready }}",
    observer: str = "fixture.observe@1",
    retry: dict[str, object] | None = None,
) -> dict[str, object]:
    postcondition: dict[str, object] = {
        "condition": condition,
        "observe": {
            "uses": observer,
            "with": {"token": "${{ steps.apply.output.token }}"},
        },
        "poll_interval": poll_interval,
    }
    if timeout is not None:
        postcondition["timeout"] = timeout
    step: dict[str, object] = {
        "id": "apply",
        "type": "action",
        "uses": "fixture.apply@1",
        "with": {"value": 1},
        "effect": {"class": main_effect},
        "risk": {"category": "observe", "level": "low"},
        "postcondition": postcondition,
    }
    if retry is not None:
        step["retry"] = retry
    return step


class ScriptedPlugin(ProcessPlugin):
    def __init__(
        self,
        manifest_value: dict[str, object],
        outcomes: dict[str, list[object]],
    ) -> None:
        super().__init__([sys.executable, "-c", "pass"], name="scripted")
        self.manifest_value = deepcopy(manifest_value)
        self.manifest = deepcopy(manifest_value)
        self.outcomes = {
            name: deque(values) for name, values in outcomes.items()
        }
        self.calls: list[tuple[str, object, float | None]] = []
        self.counts: dict[str, int] = defaultdict(int)

    def start(self, timeout: float | None = None) -> dict[str, object]:
        self.manifest = deepcopy(self.manifest_value)
        return self.manifest

    def invoke(
        self, action: str, args: object, timeout: float | None = None
    ) -> object:
        name = action.rsplit(".", 1)[1].split("@", 1)[0]
        self.calls.append((action, args, timeout))
        self.counts[name] += 1
        values = self.outcomes[name]
        outcome = values.popleft() if len(values) > 1 else values[0]
        if callable(outcome):
            outcome = outcome(timeout)
        if isinstance(outcome, BaseException):
            raise outcome
        return deepcopy(outcome)


class DeterministicClock:
    def __init__(self, now: float = 100.0) -> None:
        self.now = now

    def monotonic(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds

    def sleep_interruptibly(
        self,
        seconds: float,
        local_deadline: float | None = None,
        *,
        cleanup: bool = False,
    ) -> None:
        del local_deadline, cleanup
        self.advance(seconds)


def fixture_plugin(
    *,
    main_effect: str = "read_only",
    observer_effect: str = "read_only",
    observations: list[object] | None = None,
    observer_contract: dict[str, object] | None = None,
) -> ScriptedPlugin:
    contracts = {
        "apply": action_contract(
            effect=main_effect,
            input_schema={"type": "object"},
            output_schema={
                "type": "object",
                "required": ["token"],
                "properties": {"token": {"type": "string"}},
            },
        ),
        "observe": observer_contract
        or action_contract(
            effect=observer_effect,
            input_schema={
                "type": "object",
                "required": ["token"],
                "properties": {"token": {"type": "string"}},
            },
            output_schema={
                "type": "object",
                "required": ["ready"],
                "properties": {"ready": {"type": "boolean"}},
            },
        ),
    }
    return ScriptedPlugin(
        manifest("fixture", contracts),
        {
            "apply": [{"token": "main-output"}],
            "observe": observations or [{"ready": True}],
        },
    )


class ObservationRuntimeTests(unittest.TestCase):
    def test_observes_before_each_condition_without_synthetic_steps(self) -> None:
        plugin = fixture_plugin(
            observations=[{"ready": False}, {"ready": True}]
        )
        self.addCleanup(plugin.close)
        runner = WorkflowRunner(
            compile_descriptor(workflow(observed_action(), max_steps=1)),
            plugins={"fixture": plugin},
        )

        result = runner.run()

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(
            [action for action, _, _ in plugin.calls],
            [
                "fixture.apply@1",
                "fixture.observe@1",
                "fixture.observe@1",
            ],
        )
        self.assertEqual(plugin.calls[1][1], {"token": "main-output"})
        self.assertEqual(set(result.steps), {"apply"})
        self.assertEqual(
            result.steps["apply"]["output"], {"token": "main-output"}
        )
        self.assertEqual(
            [event["event"] for event in result.events],
            ["step.started", "step.succeeded"],
        )
        self.assertNotIn("observation", runner.context)

    def test_observer_contract_must_be_read_only_before_observer_dispatch(self) -> None:
        plugin = fixture_plugin(observer_effect="idempotent")
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(workflow(observed_action())),
            plugins={"fixture": plugin},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "POLICY.DENIED")
        self.assertEqual(plugin.counts, {})

    def test_observer_error_contracts_must_be_not_applied(self) -> None:
        plugin = fixture_plugin(
            observer_contract=action_contract(
                errors=[
                    {
                        "code": "OBSERVER.UNSAFE",
                        "retryable": False,
                        "effect": "unknown",
                    }
                ]
            )
        )
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(workflow(observed_action())),
            plugins={"fixture": plugin},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "POLICY.DENIED")
        self.assertEqual(plugin.counts, {})

    def test_observer_reuses_input_and_output_contracts(self) -> None:
        cases = [
            (
                "input",
                action_contract(
                    input_schema={
                        "type": "object",
                        "properties": {"token": {"type": "integer"}},
                    }
                ),
                [{"ready": True}],
                "ACTION.INPUT_INVALID",
                0,
            ),
            (
                "output",
                action_contract(
                    output_schema={
                        "type": "object",
                        "properties": {"ready": {"type": "boolean"}},
                        "required": ["ready"],
                    }
                ),
                [{"wrong": True}],
                "ACTION.OUTPUT_INVALID",
                1,
            ),
        ]
        for name, contract, observations, code, calls in cases:
            with self.subTest(name=name):
                plugin = fixture_plugin(
                    observations=observations, observer_contract=contract
                )
                self.addCleanup(plugin.close)
                result = run_descriptor(
                    compile_descriptor(workflow(observed_action())),
                    plugins={"fixture": plugin},
                )

                self.assertEqual(result.error.code, code)
                self.assertEqual(plugin.counts["observe"], calls)

    def test_static_invalid_observer_input_blocks_main_dispatch(self) -> None:
        plugin = fixture_plugin(
            observer_contract=action_contract(
                input_schema={
                    "type": "object",
                    "properties": {"token": {"type": "integer"}},
                    "required": ["token"],
                }
            )
        )
        self.addCleanup(plugin.close)
        step = observed_action()
        step["postcondition"]["observe"]["with"] = {
            "token": "not-an-integer"
        }

        result = run_descriptor(
            compile_descriptor(workflow(step)), plugins={"fixture": plugin}
        )

        self.assertEqual(result.error.code, "ACTION.INPUT_INVALID")
        self.assertEqual(plugin.counts, {})

    def test_previous_step_observer_input_is_preflighted_before_main(self) -> None:
        integer_observer = action_contract(
            input_schema={
                "type": "object",
                "properties": {"token": {"type": "integer"}},
                "required": ["token"],
            }
        )
        plugin = ScriptedPlugin(
            manifest(
                "fixture",
                {
                    "previous": action_contract(),
                    "apply": action_contract(
                        output_schema={
                            "type": "object",
                            "required": ["token"],
                            "properties": {"token": {"type": "string"}},
                        }
                    ),
                    "observe": integer_observer,
                },
            ),
            {
                "previous": [{"token": "not-an-integer"}],
                "apply": [{"token": "main-output"}],
                "observe": [{"ready": True}],
            },
        )
        self.addCleanup(plugin.close)
        current = observed_action()
        current["postcondition"]["observe"]["with"] = {
            "token": "${{ steps.previous.output.token }}"
        }
        raw = workflow(current)
        raw["steps"].insert(
            0,
            {
                "id": "previous",
                "type": "action",
                "uses": "fixture.previous@1",
                "with": {},
                "effect": {"class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
            },
        )

        result = run_descriptor(
            compile_descriptor(raw), plugins={"fixture": plugin}
        )

        self.assertEqual(result.error.code, "ACTION.INPUT_INVALID")
        self.assertEqual(plugin.counts["previous"], 1)
        self.assertEqual(plugin.counts["apply"], 0)
        self.assertEqual(plugin.counts["observe"], 0)

    def test_observer_reuses_action_permission_and_risk_policy(self) -> None:
        permission_contract = action_contract(
            permissions=["desktop.observe"]
        )
        permission_plugin = fixture_plugin(
            observer_contract=permission_contract
        )
        self.addCleanup(permission_plugin.close)
        permission_result = run_descriptor(
            compile_descriptor(workflow(observed_action())),
            plugins={"fixture": permission_plugin},
        )

        self.assertEqual(permission_result.error.code, "POLICY.DENIED")
        self.assertEqual(permission_plugin.counts["apply"], 0)
        self.assertEqual(permission_plugin.counts["observe"], 0)

        risk_contract = action_contract(
            risk={"category": "modify", "level": "high"}
        )
        risk_plugin = fixture_plugin(observer_contract=risk_contract)
        self.addCleanup(risk_plugin.close)
        risk_result = run_descriptor(
            compile_descriptor(
                workflow(
                    observed_action(),
                    policy={
                        "allowed_risk": {
                            "categories": ["observe"],
                            "max_level": "low",
                        }
                    },
                )
            ),
            plugins={"fixture": risk_plugin},
        )

        self.assertEqual(risk_result.error.code, "POLICY.DENIED")
        self.assertEqual(risk_plugin.counts["apply"], 0)
        self.assertEqual(risk_plugin.counts["observe"], 0)

    def test_missing_postcondition_timeout_observes_once_immediately(self) -> None:
        plugin = fixture_plugin(observations=[{"ready": True}])
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(workflow(observed_action(timeout=None))),
            plugins={"fixture": plugin},
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(plugin.counts["apply"], 1)
        self.assertEqual(plugin.counts["observe"], 1)

    def test_missing_postcondition_timeout_does_not_poll(self) -> None:
        plugin = fixture_plugin(observations=[{"ready": False}])
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(workflow(observed_action(timeout=None))),
            plugins={"fixture": plugin},
        )

        self.assertEqual(result.error.code, "ACTION.POSTCONDITION_FAILED")
        self.assertEqual(plugin.counts["apply"], 1)
        self.assertEqual(plugin.counts["observe"], 1)

    def test_each_observation_gets_fresh_timeout_clamped_by_postcondition_deadline(
        self,
    ) -> None:
        clock = DeterministicClock()

        def first_observation(timeout: float | None) -> dict[str, bool]:
            self.assertAlmostEqual(timeout or 0.0, 0.05)
            clock.advance(0.02)
            return {"ready": False}

        contract = action_contract(
            timeout="50ms",
            output_schema={
                "type": "object",
                "required": ["ready"],
                "properties": {"ready": {"type": "boolean"}},
            },
        )
        plugin = fixture_plugin(
            observations=[first_observation, {"ready": True}],
            observer_contract=contract,
        )
        self.addCleanup(plugin.close)
        runner = WorkflowRunner(
            compile_descriptor(
                workflow(
                    observed_action(timeout="70ms", poll_interval="30ms")
                )
            ),
            plugins={"fixture": plugin},
        )

        with (
            patch(
                "ai_auto_desktop.runtime.time.monotonic",
                side_effect=clock.monotonic,
            ),
            patch.object(
                runner,
                "_sleep_interruptibly",
                side_effect=clock.sleep_interruptibly,
            ),
        ):
            result = runner.run()

        self.assertTrue(result.ok, result.to_dict())
        observe_timeouts = [
            timeout
            for action, _, timeout in plugin.calls
            if action == "fixture.observe@1"
        ]
        self.assertEqual(len(observe_timeouts), 2)
        self.assertAlmostEqual(observe_timeouts[0] or 0.0, 0.05)
        self.assertAlmostEqual(observe_timeouts[1] or 0.0, 0.02)

    def test_effectful_action_postcondition_expiry_is_unknown_and_not_retried(self) -> None:
        clock = DeterministicClock()
        plugin = fixture_plugin(
            main_effect="idempotent", observations=[{"ready": False}]
        )
        self.addCleanup(plugin.close)
        step = observed_action(
            main_effect="idempotent",
            timeout="20ms",
            poll_interval="100ms",
            retry={
                "max_attempts": 3,
                "on": {"codes": ["ACTION.UNKNOWN_EFFECT"]},
            },
        )
        runner = WorkflowRunner(
            compile_descriptor(workflow(step)), plugins={"fixture": plugin}
        )
        with (
            patch(
                "ai_auto_desktop.runtime.time.monotonic",
                side_effect=clock.monotonic,
            ),
            patch.object(
                runner,
                "_sleep_interruptibly",
                side_effect=clock.sleep_interruptibly,
            ),
        ):
            result = runner.run()

        self.assertEqual(result.status, "unknown_effect")
        self.assertEqual(result.error.code, "ACTION.UNKNOWN_EFFECT")
        self.assertFalse(result.error.retryable)
        self.assertEqual(result.steps["apply"]["attempts"], 1)
        self.assertEqual(plugin.counts["apply"], 1)
        self.assertEqual(plugin.counts["observe"], 1)
        self.assertAlmostEqual(clock.now, 100.02)
        self.assertEqual(
            result.error.details["cause"]["code"],
            "ACTION.POSTCONDITION_FAILED",
        )
        self.assertEqual(
            result.error.details["last_observation"], {"ready": False}
        )

    def test_effectful_action_observer_failure_is_unknown(self) -> None:
        failure = PluginError(
            "OBSERVER.UNAVAILABLE",
            "observation failed",
            details={"dispatched": True},
        )
        observer_contract = action_contract(
            errors=[
                {
                    "code": "OBSERVER.UNAVAILABLE",
                    "retryable": False,
                    "effect": "not_applied",
                }
            ]
        )
        plugin = fixture_plugin(
            main_effect="non_idempotent",
            observations=[failure],
            observer_contract=observer_contract,
        )
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(
                workflow(observed_action(main_effect="non_idempotent"))
            ),
            plugins={"fixture": plugin},
        )

        self.assertEqual(result.status, "unknown_effect")
        self.assertEqual(result.error.code, "ACTION.UNKNOWN_EFFECT")
        self.assertEqual(
            result.error.details["cause"]["code"], "OBSERVER.UNAVAILABLE"
        )
        self.assertIsNone(result.error.details["last_observation"])

    def test_effectful_action_observer_preflight_blocks_main_dispatch(self) -> None:
        plugin = fixture_plugin(
            main_effect="idempotent", observer_effect="idempotent"
        )
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(
                workflow(observed_action(main_effect="idempotent"))
            ),
            plugins={"fixture": plugin},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "POLICY.DENIED")
        self.assertEqual(plugin.counts, {})

    def test_read_only_main_action_preserves_observer_failure(self) -> None:
        failure = PluginError(
            "OBSERVER.UNAVAILABLE",
            "observation failed",
            details={"dispatched": True},
        )
        plugin = fixture_plugin(
            observations=[failure],
            observer_contract=action_contract(
                errors=[
                    {
                        "code": "OBSERVER.UNAVAILABLE",
                        "retryable": False,
                        "effect": "not_applied",
                    }
                ]
            ),
        )
        self.addCleanup(plugin.close)

        result = run_descriptor(
            compile_descriptor(workflow(observed_action())),
            plugins={"fixture": plugin},
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "OBSERVER.UNAVAILABLE")
        self.assertEqual(result.error.effect, "not_applied")

    def test_postcondition_deadline_bounds_observer_manifest_handshake(self) -> None:
        writer = ScriptedPlugin(
            manifest(
                "writer",
                {"apply": action_contract(effect="idempotent")},
            ),
            {"apply": [{"token": "main-output"}]},
        )
        sensor = ProcessPlugin(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            timeout=2,
            name="slow-observer",
        )
        self.addCleanup(writer.close)
        self.addCleanup(sensor.close)
        step = observed_action(
            main_effect="idempotent",
            timeout="40ms",
            observer="sensor.observe@1",
        )
        step["uses"] = "writer.apply@1"
        started = time.monotonic()

        result = run_descriptor(
            compile_descriptor(workflow(step)),
            plugins={"writer": writer, "sensor": sensor},
        )
        elapsed = time.monotonic() - started

        self.assertEqual(result.status, "timed_out")
        self.assertEqual(result.error.code, "ACTION.TIMEOUT")
        self.assertEqual(writer.counts["apply"], 0)
        self.assertLess(elapsed, 0.6)

    def test_postcondition_without_observe_keeps_existing_failure_semantics(self) -> None:
        plugin = fixture_plugin(main_effect="idempotent")
        self.addCleanup(plugin.close)
        step = observed_action(main_effect="idempotent", timeout="1ms")
        step["postcondition"] = {
            "condition": "${{ False }}",
            "timeout": "1ms",
            "poll_interval": "1ms",
        }

        result = run_descriptor(
            compile_descriptor(workflow(step)), plugins={"fixture": plugin}
        )

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.error.code, "ACTION.POSTCONDITION_FAILED")


if __name__ == "__main__":
    unittest.main()
