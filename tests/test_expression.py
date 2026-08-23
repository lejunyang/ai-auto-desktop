"""Unit tests for the constrained expression evaluator."""

from __future__ import annotations

import unittest

from ai_auto_desktop.expression import (
    CompiledExpression,
    ExpressionError,
    compile_expression,
    evaluate_expression,
)


class _CallProbe:
    """Callable that records whether untrusted code invoked it."""

    def __init__(self) -> None:
        self.calls = 0

    def __call__(self, *args: object, **kwargs: object) -> str:
        self.calls += 1
        return "unsafe callable was executed"


class ExpressionEvaluationTests(unittest.TestCase):
    def test_arithmetic_operators(self) -> None:
        cases = {
            "2 + 3 * 4 - 5": 9,
            "20 / 4": 5.0,
            "17 // 5": 3,
            "17 % 5": 2,
            "2 ** 5": 32,
            "-(3 + 4)": -7,
            "+amount": 6,
        }

        for source, expected in cases.items():
            with self.subTest(source=source):
                self.assertEqual(
                    evaluate_expression(source, {"amount": 6}), expected
                )

    def test_compiled_expression_can_be_reused(self) -> None:
        expression = compile_expression("subtotal * (1 - discount)")

        self.assertIsInstance(expression, CompiledExpression)
        self.assertEqual(
            expression.evaluate({"subtotal": 100, "discount": 0.2}), 80.0
        )
        self.assertEqual(
            expression.evaluate({"subtotal": 50, "discount": 0.1}), 45.0
        )

    def test_boolean_comparisons_and_conditional_expression(self) -> None:
        context = {
            "enabled": True,
            "disabled": False,
            "attempts": 2,
            "role": "admin",
        }

        self.assertTrue(
            evaluate_expression(
                "enabled and attempts < 3 and role == 'admin'", context
            )
        )
        self.assertTrue(evaluate_expression("disabled or attempts >= 2", context))
        self.assertTrue(evaluate_expression("not disabled", context))
        self.assertEqual(
            evaluate_expression("'allowed' if enabled else 'denied'", context),
            "allowed",
        )

    def test_boolean_operators_short_circuit(self) -> None:
        self.assertFalse(evaluate_expression("False and unknown_variable"))
        self.assertTrue(evaluate_expression("True or unknown_variable"))

    def test_mapping_attributes_and_subscripts(self) -> None:
        context = {
            "user": {
                "profile": {"name": "Ada"},
                "roles": ["admin", "editor"],
                "items": "mapping value wins over dict.items",
            },
            "field": "name",
            "values": [10, 20, 30, 40, 50],
        }

        self.assertEqual(evaluate_expression("user.profile.name", context), "Ada")
        self.assertEqual(
            evaluate_expression("user['profile'][field]", context), "Ada"
        )
        self.assertEqual(evaluate_expression("user.roles[1]", context), "editor")
        self.assertEqual(
            evaluate_expression("user.items", context),
            "mapping value wins over dict.items",
        )
        self.assertEqual(evaluate_expression("values[1:5:2]", context), [20, 40])

    def test_builtin_names_are_not_implicitly_available(self) -> None:
        with self.assertRaises(ExpressionError):
            evaluate_expression("len")

        explicit_value = object()
        self.assertIs(
            evaluate_expression("len", {"len": explicit_value}), explicit_value
        )


class ExpressionSafetyTests(unittest.TestCase):
    def assert_rejected(self, source: str) -> None:
        with self.assertRaises(ExpressionError):
            evaluate_expression(source)

    def test_arbitrary_function_calls_are_rejected(self) -> None:
        for source in (
            "callback()",
            "open('/tmp/unsafe')",
            "max([1, 2])()",
        ):
            with self.subTest(source=source):
                self.assert_rejected(source)

    def test_method_calls_are_rejected(self) -> None:
        for source in ("text.upper()", "service.run('command')"):
            with self.subTest(source=source):
                self.assert_rejected(source)

    def test_builtin_calls_are_rejected(self) -> None:
        for source in (
            "len(values)",
            "min(values)",
            "sum(values)",
            "sorted(values)",
            "str(value)",
        ):
            with self.subTest(source=source):
                self.assert_rejected(source)

    def test_lambda_and_comprehensions_are_rejected(self) -> None:
        expressions = (
            "lambda value: value",
            "[value for value in values]",
            "{value for value in values}",
            "{value: value for value in values}",
            "(value for value in values)",
        )

        for source in expressions:
            with self.subTest(source=source):
                self.assert_rejected(source)

    def test_dunder_attributes_are_rejected(self) -> None:
        for source in ("value.__class__", "value.__dict__"):
            with self.subTest(source=source):
                self.assert_rejected(source)

    def test_dunder_subscript_and_literal_keys_are_rejected(self) -> None:
        cases = (
            ("payload['__class__']", {"payload": {"__class__": object}}),
            ("payload[key]", {"payload": {"__dict__": {}}, "key": "__dict__"}),
            ("{'__class__': 1}", {}),
        )

        for source, context in cases:
            with self.subTest(source=source):
                with self.assertRaises(ExpressionError):
                    evaluate_expression(source, context)

    def test_unknown_variable_is_reported(self) -> None:
        with self.assertRaises(ExpressionError) as raised:
            evaluate_expression("missing + 1", {"present": 1})

        self.assertEqual(raised.exception.source, "missing + 1")
        self.assertEqual(raised.exception.lineno, 1)
        self.assertEqual(raised.exception.col_offset, 0)

    def test_malicious_context_callable_is_never_executed(self) -> None:
        callback = _CallProbe()
        context = {
            "callback": callback,
            "holder": {"callback": callback},
            "values": [3, 1, 2],
        }
        expressions = (
            "callback()",
            "holder.callback()",
            "sorted(values, key=callback)",
        )

        for source in expressions:
            with self.subTest(source=source):
                with self.assertRaises(ExpressionError):
                    evaluate_expression(source, context)

        self.assertEqual(callback.calls, 0)


if __name__ == "__main__":
    unittest.main()
