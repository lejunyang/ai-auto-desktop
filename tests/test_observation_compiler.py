"""Contract tests for postcondition observation compilation."""

from __future__ import annotations

from copy import deepcopy
import unittest
from unittest.mock import patch

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.errors import DescriptorError


API_VERSION = "ai-auto-desktop.dev/v1alpha1"


def descriptor() -> dict[str, object]:
    return {
        "apiVersion": API_VERSION,
        "kind": "Workflow",
        "metadata": {"name": "observation-contract"},
        "budgets": {"max_duration": "30s", "max_executed_steps": 10},
        "steps": [
            {
                "id": "invoke",
                "type": "action",
                "uses": "fixture.invoke@1",
                "with": {},
                "postcondition": {
                    "observe": {
                        "uses": "fixture.snapshot@1",
                        "with": {"target": "dialog"},
                    },
                    "condition": "${{ observation.visible == True }}",
                },
            }
        ],
    }


class ObservationCompilerTests(unittest.TestCase):
    def assert_invalid(
        self, value: dict[str, object], *, compiler_only: bool = False
    ) -> DescriptorError:
        if compiler_only:
            with patch(
                "ai_auto_desktop.compiler._schema_issues", return_value=[]
            ):
                with self.assertRaises(DescriptorError) as raised:
                    compile_descriptor(value)
        else:
            with self.assertRaises(DescriptorError) as raised:
                compile_descriptor(value)
        self.assertEqual(raised.exception.code, "DESCRIPTOR.INVALID")
        self.assertTrue(raised.exception.issues)
        return raised.exception

    def assert_invalid_in_schema_and_compiler(
        self, value: dict[str, object]
    ) -> None:
        self.assert_invalid(value)
        self.assert_invalid(value, compiler_only=True)

    def test_postcondition_observe_and_observation_expression_compile(self) -> None:
        compiled = compile_descriptor(descriptor())

        postcondition = compiled.steps[0].params["postcondition"]
        self.assertEqual(
            postcondition["observe"]["uses"], "fixture.snapshot@1"
        )
        self.assertEqual(postcondition["observe"]["with"]["target"], "dialog")
        self.assertEqual(
            postcondition["condition"], "${{ observation.visible == True }}"
        )

    def test_observe_requires_canonical_uses_and_object_with(self) -> None:
        mutations = (
            ("missing uses", lambda observe: observe.pop("uses")),
            ("missing with", lambda observe: observe.pop("with")),
            ("noncanonical uses", lambda observe: observe.update(uses="fixture.snapshot")),
            ("nonobject with", lambda observe: observe.__setitem__("with", [])),
        )
        for label, mutate in mutations:
            value = descriptor()
            observe = value["steps"][0]["postcondition"]["observe"]  # type: ignore[index]
            mutate(observe)  # type: ignore[arg-type]
            with self.subTest(label=label):
                self.assert_invalid_in_schema_and_compiler(value)

    def test_observe_rejects_action_control_and_policy_fields(self) -> None:
        extras: dict[str, object] = {
            "effect": {"class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
            "retry": {"max_attempts": 2},
            "on_error": {"steps": [], "outcome": {"mode": "rethrow"}},
        }
        for field, field_value in extras.items():
            value = descriptor()
            observe = value["steps"][0]["postcondition"]["observe"]  # type: ignore[index]
            observe[field] = field_value  # type: ignore[index]
            with self.subTest(field=field):
                error = self.assert_invalid(value, compiler_only=True)
                self.assertIn(
                    f"$.steps[0].postcondition.observe.{field}",
                    {issue["path"] for issue in error.issues},
                )
                self.assert_invalid(value)

    def test_precondition_cannot_declare_observe(self) -> None:
        value = descriptor()
        action = value["steps"][0]  # type: ignore[index]
        action["precondition"] = {
            "observe": deepcopy(action["postcondition"]["observe"]),
            "condition": "${{ observation.visible == True }}",
        }

        error = self.assert_invalid(value, compiler_only=True)
        self.assertIn(
            "$.steps[0].precondition.observe",
            {issue["path"] for issue in error.issues},
        )
        self.assert_invalid(value)


if __name__ == "__main__":
    unittest.main()
