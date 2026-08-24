"""Contract tests for sibling-scoped workflow DAG compilation."""

from __future__ import annotations

import json
from pathlib import Path
import unittest

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.errors import DescriptorError


PROJECT_ROOT = Path(__file__).resolve().parents[1]
API_VERSION = "ai-auto-desktop.dev/v1alpha1"


def descriptor(*steps: dict[str, object]) -> dict[str, object]:
    return {
        "apiVersion": API_VERSION,
        "kind": "Workflow",
        "metadata": {"name": "dag-contract"},
        "budgets": {"max_duration": "30s", "max_executed_steps": 100},
        "steps": list(steps),
    }


def returning(step_id: str, **fields: object) -> dict[str, object]:
    return {"id": step_id, "type": "return", **fields}


class DagCompilerTests(unittest.TestCase):
    def assert_invalid(
        self, value: dict[str, object], code: str, path: str | None = None
    ) -> DescriptorError:
        with self.assertRaises(DescriptorError) as raised:
            compile_descriptor(value)
        issues = raised.exception.issues
        self.assertIn(code, {issue["code"] for issue in issues})
        if path is not None:
            self.assertIn(path, {issue["path"] for issue in issues})
        return raised.exception

    def test_legacy_steps_normalize_to_serial_dependencies(self) -> None:
        compiled = compile_descriptor(
            descriptor(returning("first"), returning("second"), returning("third"))
        )

        self.assertEqual(
            [step.depends_on for step in compiled.steps],
            [(), ("first",), ("second",)],
        )
        self.assertEqual(compiled.budgets["max_concurrency"], 1)
        self.assertNotIn("max_concurrency", compiled.raw["budgets"])

    def test_explicit_empty_dependencies_break_the_implicit_chain(self) -> None:
        compiled = compile_descriptor(
            descriptor(
                returning("first"),
                returning("parallel", depends_on=[]),
                returning("after_parallel"),
            )
        )

        self.assertEqual(compiled.steps[1].depends_on, ())
        self.assertEqual(compiled.steps[2].depends_on, ("parallel",))

    def test_forward_dependencies_are_allowed_and_order_is_preserved(self) -> None:
        compiled = compile_descriptor(
            descriptor(
                returning("join", depends_on=["right", "left"]),
                returning("left", depends_on=[]),
                returning("right", depends_on=[]),
            )
        )

        self.assertEqual(compiled.steps[0].depends_on, ("right", "left"))

    def test_each_nested_step_list_has_an_independent_implicit_chain(self) -> None:
        compiled = compile_descriptor(
            descriptor(
                {
                    "id": "outer",
                    "type": "block",
                    "steps": [returning("inner_a"), returning("inner_b")],
                },
                returning("tail"),
            )
        )

        self.assertEqual(compiled.steps[0].depends_on, ())
        self.assertEqual(compiled.steps[0].steps[0].depends_on, ())
        self.assertEqual(compiled.steps[0].steps[1].depends_on, ("inner_a",))
        self.assertEqual(compiled.steps[1].depends_on, ("outer",))

    def test_unknown_self_cross_scope_and_cycles_are_rejected(self) -> None:
        cases = (
            (
                descriptor(returning("only", depends_on=["missing"])),
                "unknown_dependency",
                "$.steps[0].depends_on[0]",
            ),
            (
                descriptor(returning("only", depends_on=["only"])),
                "self_dependency",
                "$.steps[0].depends_on[0]",
            ),
            (
                descriptor(
                    {
                        "id": "outer",
                        "type": "block",
                        "steps": [returning("inner", depends_on=["outer"])],
                    }
                ),
                "cross_scope_dependency",
                "$.steps[0].steps[0].depends_on[0]",
            ),
            (
                descriptor(
                    returning("a", depends_on=["b"]),
                    returning("b", depends_on=["a"]),
                ),
                "dependency_cycle",
                None,
            ),
        )
        for value, code, path in cases:
            with self.subTest(code=code):
                self.assert_invalid(value, code, path)

    def test_duplicate_dependencies_are_rejected_by_schema(self) -> None:
        self.assert_invalid(
            descriptor(
                returning("source"),
                returning("sink", depends_on=["source", "source"]),
            ),
            "schema",
        )

    def test_static_sibling_reference_requires_transitive_dependency(self) -> None:
        invalid = descriptor(
            returning("source", value=1),
            returning("other", depends_on=[]),
            returning("consumer", value="${{ steps.source.output }}"),
        )
        self.assert_invalid(invalid, "uncovered_step_reference", "$.steps[2]")

        valid = descriptor(
            returning("source", value=1),
            returning("middle"),
            returning("consumer", value="${{ steps.source.output }}"),
        )
        compiled = compile_descriptor(valid)
        self.assertEqual(compiled.steps[2].depends_on, ("middle",))

    def test_static_reference_check_includes_interpolated_expressions(self) -> None:
        invalid = descriptor(
            returning("source", value=1),
            returning("other", depends_on=[]),
            returning(
                "consumer",
                value="prefix ${{ steps.source.output }} suffix",
            ),
        )

        self.assert_invalid(invalid, "uncovered_step_reference", "$.steps[2]")

    def test_outer_scope_step_reference_is_not_misclassified(self) -> None:
        compiled = compile_descriptor(
            descriptor(
                returning("source", value=1),
                {
                    "id": "nested",
                    "type": "block",
                    "steps": [
                        returning("consumer", value="${{ steps.source.output }}")
                    ],
                },
            )
        )

        self.assertEqual(compiled.steps[1].steps[0].depends_on, ())

    def test_max_concurrency_accepts_only_integer_range_1_to_64(self) -> None:
        for value in (0, 65, True, 1.5):
            raw = descriptor(returning("done"))
            raw["budgets"]["max_concurrency"] = value  # type: ignore[index]
            with self.subTest(value=value):
                self.assert_invalid(raw, "schema")

        raw = descriptor(returning("done"))
        raw["budgets"]["max_concurrency"] = 64  # type: ignore[index]
        self.assertEqual(compile_descriptor(raw).budgets["max_concurrency"], 64)

    def test_canonical_and_packaged_workflow_schemas_are_identical(self) -> None:
        canonical = PROJECT_ROOT / "schemas/workflow/v1alpha1/workflow.schema.json"
        packaged = (
            PROJECT_ROOT
            / "src/ai_auto_desktop/schemas/workflow/v1alpha1/workflow.schema.json"
        )

        self.assertEqual(canonical.read_bytes(), packaged.read_bytes())
        json.loads(canonical.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
