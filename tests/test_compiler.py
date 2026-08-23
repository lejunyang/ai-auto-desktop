"""Contract tests for strict workflow descriptor compilation."""

from __future__ import annotations

from copy import deepcopy
from pathlib import Path
import json
import tempfile
import unittest

from ai_auto_desktop.compiler import compile_descriptor, load_descriptor
from ai_auto_desktop.errors import DescriptorError
from ai_auto_desktop.model import WorkflowDescriptor


API_VERSION = "ai-auto-desktop.dev/v1alpha1"


def descriptor(*steps: dict[str, object]) -> dict[str, object]:
    return {
        "apiVersion": API_VERSION,
        "kind": "Workflow",
        "metadata": {"name": "compiler-contract"},
        "budgets": {"max_duration": "30s", "max_executed_steps": 100},
        "steps": list(steps),
    }


class CompilerTests(unittest.TestCase):
    def assert_invalid(self, value: dict[str, object]) -> DescriptorError:
        with self.assertRaises(DescriptorError) as raised:
            compile_descriptor(value)
        error = raised.exception
        self.assertEqual(error.code, "DESCRIPTOR.INVALID")
        self.assertTrue(error.issues)
        self.assertEqual(error.to_dict()["details"]["issues"], error.issues)
        return error

    def test_minimal_canonical_descriptor_compiles_to_frozen_model(self) -> None:
        source = descriptor({"id": "finish", "type": "return", "value": 7})

        compiled = compile_descriptor(source)

        self.assertIsInstance(compiled, WorkflowDescriptor)
        self.assertEqual(compiled.api_version, API_VERSION)
        self.assertEqual(compiled.name, "compiler-contract")
        self.assertEqual(compiled.steps[0].id, "finish")
        source["metadata"]["name"] = "mutated"  # type: ignore[index]
        source["steps"][0]["value"] = 99  # type: ignore[index]
        self.assertEqual(compiled.name, "compiler-contract")
        self.assertEqual(compiled.steps[0].get("value"), 7)

    def test_load_descriptor_accepts_canonical_json(self) -> None:
        value = descriptor({"id": "finish", "type": "return", "value": None})
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory, "workflow.json")
            path.write_text(json.dumps(value), encoding="utf-8")

            compiled = load_descriptor(path)

        self.assertEqual(compiled.name, "compiler-contract")
        self.assertEqual(compiled.source, path)

    def test_required_input_cannot_also_have_default(self) -> None:
        value = descriptor({"id": "finish", "type": "return"})
        value["budgets"] = {
            "max_duration": "1s",
            "max_executed_steps": 2,
        }
        value["inputs"] = {
            "name": {
                "schema": {"type": "string"},
                "required": True,
                "default": "Ada",
            }
        }

        self.assert_invalid(value)

    def test_variable_is_immutable_by_default(self) -> None:
        value = descriptor(
            {"id": "write", "type": "set", "assign": {"vars.value": 2}}
        )
        value["budgets"] = {
            "max_duration": "1s",
            "max_executed_steps": 2,
        }
        value["variables"] = {
            "value": {"schema": {"type": "integer"}, "initial": 1}
        }

        self.assert_invalid(value)

    def test_required_budgets_are_rejected_when_missing(self) -> None:
        value = descriptor({"id": "finish", "type": "return"})
        del value["budgets"]

        self.assert_invalid(value)

    def test_legacy_shorthand_identity_is_rejected(self) -> None:
        self.assert_invalid(
            {
                "version": API_VERSION,
                "name": "legacy",
                "steps": [{"id": "finish", "type": "return"}],
            }
        )

    def test_unknown_top_level_field_is_rejected(self) -> None:
        value = descriptor({"id": "finish", "type": "return"})
        value["surprise"] = True

        self.assert_invalid(value)

    def test_unknown_step_field_is_rejected(self) -> None:
        self.assert_invalid(
            descriptor(
                {
                    "id": "finish",
                    "type": "return",
                    "value": None,
                    "surprise": True,
                }
            )
        )

    def test_step_ids_are_unique_across_mutually_exclusive_branches(self) -> None:
        self.assert_invalid(
            descriptor(
                {
                    "id": "choose",
                    "type": "if",
                    "condition": True,
                    "then": [
                        {"id": "duplicate", "type": "return", "value": 1}
                    ],
                    "else": [
                        {"id": "duplicate", "type": "return", "value": 2}
                    ],
                }
            )
        )

    def test_step_ids_are_unique_across_main_handlers_and_finally(self) -> None:
        value = descriptor({"id": "duplicate", "type": "return"})
        value["on_error"] = {
            "steps": [{"id": "duplicate", "type": "return"}],
            "outcome": {"mode": "rethrow"},
        }
        value["finally"] = [{"id": "duplicate", "type": "return"}]

        self.assert_invalid(value)

    def test_while_requires_a_positive_iteration_bound(self) -> None:
        base = {
            "id": "poll",
            "type": "while",
            "condition": "${{ False }}",
            "timeout": "1s",
            "steps": [{"id": "body", "type": "return"}],
        }
        for max_iterations in (None, 0, -1, True):
            step = deepcopy(base)
            if max_iterations is not None:
                step["max_iterations"] = max_iterations
            with self.subTest(max_iterations=max_iterations):
                self.assert_invalid(descriptor(step))

    def test_foreach_requires_a_positive_iteration_bound(self) -> None:
        base = {
            "id": "iterate",
            "type": "foreach",
            "items": "${{ [1, 2] }}",
            "as": "item",
            "steps": [{"id": "body", "type": "return"}],
        }
        for max_items in (None, 0, -1, True):
            step = deepcopy(base)
            if max_items is not None:
                step["max_items"] = max_items
            with self.subTest(max_items=max_items):
                self.assert_invalid(descriptor(step))

    def test_action_requires_canonical_uses_and_strict_effect_shape(self) -> None:
        invalid_steps = (
            {
                "id": "legacy_action",
                "type": "action",
                "plugin": "desktop",
                "action": "invoke",
                "with": {},
            },
            {
                "id": "bad_uses",
                "type": "action",
                "uses": "desktop.invoke",
                "with": {},
            },
            {
                "id": "bad_risk",
                "type": "action",
                "uses": "desktop.invoke@1",
                "with": {},
                "effect": {"class": "non_idempotent", "risk": "send"},
            },
        )
        for step in invalid_steps:
            with self.subTest(step_id=step["id"]):
                self.assert_invalid(descriptor(step))

    def test_expression_function_call_is_rejected_during_compilation(self) -> None:
        self.assert_invalid(
            descriptor(
                {
                    "id": "unsafe",
                    "type": "if",
                    "condition": "${{ len(inputs.values) > 0 }}",
                    "then": [{"id": "finish", "type": "return"}],
                }
            )
        )


if __name__ == "__main__":
    unittest.main()
