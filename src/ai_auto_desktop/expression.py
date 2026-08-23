"""A small, deliberately constrained expression evaluator.

Expressions are parsed with :mod:`ast`, validated against an allow-list, and
interpreted directly.  Python bytecode is never compiled and ``eval``/``exec``
are never used.
"""

from __future__ import annotations

import ast
import operator
from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import Any, Final

__all__ = [
    "CompiledExpression",
    "ExpressionError",
    "compile",
    "compile_expression",
    "evaluate",
    "evaluate_expression",
]


class ExpressionError(ValueError):
    """Raised when an expression is invalid, unsafe, or cannot be evaluated."""

    def __init__(
        self,
        message: str,
        *,
        source: str | None = None,
        lineno: int | None = None,
        col_offset: int | None = None,
    ) -> None:
        self.message = message
        self.source = source
        self.lineno = lineno
        self.col_offset = col_offset
        super().__init__(message)

    def __str__(self) -> str:
        location = ""
        if self.lineno is not None:
            location = f" at line {self.lineno}"
            if self.col_offset is not None:
                location += f", column {self.col_offset + 1}"
        return f"{self.message}{location}"


_BINARY_OPERATORS: Final = {
    ast.Add: operator.add,
    ast.Sub: operator.sub,
    ast.Mult: operator.mul,
    ast.Div: operator.truediv,
    ast.FloorDiv: operator.floordiv,
    ast.Mod: operator.mod,
    ast.Pow: operator.pow,
}

_UNARY_OPERATORS: Final = {
    ast.Not: operator.not_,
    ast.UAdd: operator.pos,
    ast.USub: operator.neg,
}

_COMPARISON_OPERATORS: Final = {
    ast.Eq: operator.eq,
    ast.NotEq: operator.ne,
    ast.Lt: operator.lt,
    ast.LtE: operator.le,
    ast.Gt: operator.gt,
    ast.GtE: operator.ge,
    ast.In: lambda value, container: operator.contains(container, value),
    ast.NotIn: lambda value, container: not operator.contains(container, value),
    ast.Is: operator.is_,
    ast.IsNot: operator.is_not,
}

_CONSTANT_TYPES: Final = (str, bytes, int, float, complex, bool, type(None))


def _is_dunder_name(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) >= 4
        and value.startswith("__")
        and value.endswith("__")
    )


class _Validator(ast.NodeVisitor):
    def __init__(self, source: str) -> None:
        self.source = source

    def _error(self, node: ast.AST, message: str) -> ExpressionError:
        return ExpressionError(
            message,
            source=self.source,
            lineno=getattr(node, "lineno", None),
            col_offset=getattr(node, "col_offset", None),
        )

    def generic_visit(self, node: ast.AST) -> None:
        raise self._error(
            node, f"unsupported expression element: {type(node).__name__}"
        )

    def visit_Expression(self, node: ast.Expression) -> None:
        self.visit(node.body)

    def visit_Constant(self, node: ast.Constant) -> None:
        if not isinstance(node.value, _CONSTANT_TYPES):
            raise self._error(node, f"unsupported literal: {node.value!r}")

    def visit_Name(self, node: ast.Name) -> None:
        if not isinstance(node.ctx, ast.Load):
            raise self._error(node, "only variable reads are allowed")
        if node.id.startswith("__"):
            raise self._error(node, "dunder variable names are not allowed")

    def visit_Attribute(self, node: ast.Attribute) -> None:
        if not isinstance(node.ctx, ast.Load):
            raise self._error(node, "only attribute reads are allowed")
        if node.attr.startswith("_"):
            raise self._error(
                node, "attributes and mapping keys starting with '_' are not allowed"
            )
        self.visit(node.value)

    def visit_Subscript(self, node: ast.Subscript) -> None:
        if not isinstance(node.ctx, ast.Load):
            raise self._error(node, "only subscript reads are allowed")
        self.visit(node.value)
        self.visit(node.slice)
        if isinstance(node.slice, ast.Constant) and _is_dunder_name(node.slice.value):
            raise self._error(node.slice, "dunder keys are not allowed")

    def visit_Slice(self, node: ast.Slice) -> None:
        for part in (node.lower, node.upper, node.step):
            if part is not None:
                self.visit(part)

    def visit_List(self, node: ast.List) -> None:
        if not isinstance(node.ctx, ast.Load):
            raise self._error(node, "only list literals are allowed")
        for element in node.elts:
            self.visit(element)

    def visit_Tuple(self, node: ast.Tuple) -> None:
        if not isinstance(node.ctx, ast.Load):
            raise self._error(node, "only tuple literals are allowed")
        for element in node.elts:
            self.visit(element)

    def visit_Dict(self, node: ast.Dict) -> None:
        for key, value in zip(node.keys, node.values, strict=True):
            if key is None:
                raise self._error(node, "dictionary unpacking is not allowed")
            self.visit(key)
            if isinstance(key, ast.Constant) and _is_dunder_name(key.value):
                raise self._error(key, "dunder keys are not allowed")
            self.visit(value)

    def visit_BoolOp(self, node: ast.BoolOp) -> None:
        if not isinstance(node.op, (ast.And, ast.Or)):
            raise self._error(node, "unsupported boolean operator")
        for value in node.values:
            self.visit(value)

    def visit_BinOp(self, node: ast.BinOp) -> None:
        if type(node.op) not in _BINARY_OPERATORS:
            raise self._error(
                node, f"unsupported arithmetic operator: {type(node.op).__name__}"
            )
        self.visit(node.left)
        self.visit(node.right)

    def visit_UnaryOp(self, node: ast.UnaryOp) -> None:
        if type(node.op) not in _UNARY_OPERATORS:
            raise self._error(
                node, f"unsupported unary operator: {type(node.op).__name__}"
            )
        self.visit(node.operand)

    def visit_Compare(self, node: ast.Compare) -> None:
        self.visit(node.left)
        for operation, comparator in zip(node.ops, node.comparators, strict=True):
            if type(operation) not in _COMPARISON_OPERATORS:
                raise self._error(
                    node,
                    f"unsupported comparison operator: {type(operation).__name__}",
                )
            self.visit(comparator)

    def visit_IfExp(self, node: ast.IfExp) -> None:
        self.visit(node.test)
        self.visit(node.body)
        self.visit(node.orelse)

    def visit_Call(self, node: ast.Call) -> None:
        raise self._error(node, "function and method calls are not allowed")


class _Evaluator:
    def __init__(self, source: str, variables: Mapping[str, Any]) -> None:
        self.source = source
        self.variables = variables

    def _error(self, node: ast.AST, message: str) -> ExpressionError:
        return ExpressionError(
            message,
            source=self.source,
            lineno=getattr(node, "lineno", None),
            col_offset=getattr(node, "col_offset", None),
        )

    def _operation(self, node: ast.AST, operation: Any, *values: Any) -> Any:
        try:
            return operation(*values)
        except ExpressionError:
            raise
        except Exception as exc:
            raise self._error(node, f"operation failed: {exc}") from exc

    def _truth(self, node: ast.AST, value: Any) -> bool:
        try:
            return bool(value)
        except Exception as exc:
            raise self._error(node, f"truth-value test failed: {exc}") from exc

    def evaluate(self, node: ast.AST) -> Any:
        method = getattr(self, f"_evaluate_{type(node).__name__}", None)
        if method is None:
            raise self._error(
                node, f"unsupported expression element: {type(node).__name__}"
            )
        return method(node)

    def _evaluate_Expression(self, node: ast.Expression) -> Any:
        return self.evaluate(node.body)

    def _evaluate_Constant(self, node: ast.Constant) -> Any:
        if not isinstance(node.value, _CONSTANT_TYPES):
            raise self._error(node, f"unsupported literal: {node.value!r}")
        return node.value

    def _evaluate_Name(self, node: ast.Name) -> Any:
        if node.id.startswith("__"):
            raise self._error(node, "dunder variable names are not allowed")
        try:
            return self.variables[node.id]
        except KeyError as exc:
            raise self._error(node, f"unknown variable: {node.id!r}") from exc
        except Exception as exc:
            raise self._error(
                node, f"could not read variable {node.id!r}: {exc}"
            ) from exc

    def _evaluate_Attribute(self, node: ast.Attribute) -> Any:
        if node.attr.startswith("_"):
            raise self._error(
                node, "attributes and mapping keys starting with '_' are not allowed"
            )

        value = self.evaluate(node.value)
        if isinstance(value, Mapping):
            try:
                return value[node.attr]
            except KeyError:
                pass
            except Exception as exc:
                raise self._error(
                    node, f"could not read mapping key {node.attr!r}: {exc}"
                ) from exc

        try:
            return getattr(value, node.attr)
        except AttributeError as exc:
            raise self._error(
                node, f"attribute or mapping key {node.attr!r} was not found"
            ) from exc
        except Exception as exc:
            raise self._error(
                node, f"could not read attribute {node.attr!r}: {exc}"
            ) from exc

    def _evaluate_Subscript(self, node: ast.Subscript) -> Any:
        value = self.evaluate(node.value)
        index = self.evaluate(node.slice)
        if _is_dunder_name(index):
            raise self._error(node, "dunder keys are not allowed")
        try:
            return value[index]
        except Exception as exc:
            raise self._error(node, f"subscript lookup failed: {exc}") from exc

    def _evaluate_Slice(self, node: ast.Slice) -> slice:
        lower = self.evaluate(node.lower) if node.lower is not None else None
        upper = self.evaluate(node.upper) if node.upper is not None else None
        step = self.evaluate(node.step) if node.step is not None else None
        return slice(lower, upper, step)

    def _evaluate_List(self, node: ast.List) -> list[Any]:
        return [self.evaluate(element) for element in node.elts]

    def _evaluate_Tuple(self, node: ast.Tuple) -> tuple[Any, ...]:
        return tuple(self.evaluate(element) for element in node.elts)

    def _evaluate_Dict(self, node: ast.Dict) -> dict[Any, Any]:
        result: dict[Any, Any] = {}
        for key, value in zip(node.keys, node.values, strict=True):
            if key is None:
                raise self._error(node, "dictionary unpacking is not allowed")
            evaluated_key = self.evaluate(key)
            if _is_dunder_name(evaluated_key):
                raise self._error(key, "dunder keys are not allowed")
            evaluated_value = self.evaluate(value)
            try:
                result[evaluated_key] = evaluated_value
            except Exception as exc:
                raise self._error(key, f"invalid dictionary key: {exc}") from exc
        return result

    def _evaluate_BoolOp(self, node: ast.BoolOp) -> Any:
        if isinstance(node.op, ast.And):
            result = self.evaluate(node.values[0])
            for value in node.values[1:]:
                if not self._truth(value, result):
                    return result
                result = self.evaluate(value)
            return result
        if isinstance(node.op, ast.Or):
            result = self.evaluate(node.values[0])
            for value in node.values[1:]:
                if self._truth(value, result):
                    return result
                result = self.evaluate(value)
            return result
        raise self._error(node, "unsupported boolean operator")

    def _evaluate_BinOp(self, node: ast.BinOp) -> Any:
        operation = _BINARY_OPERATORS.get(type(node.op))
        if operation is None:
            raise self._error(
                node, f"unsupported arithmetic operator: {type(node.op).__name__}"
            )
        return self._operation(
            node, operation, self.evaluate(node.left), self.evaluate(node.right)
        )

    def _evaluate_UnaryOp(self, node: ast.UnaryOp) -> Any:
        operation = _UNARY_OPERATORS.get(type(node.op))
        if operation is None:
            raise self._error(
                node, f"unsupported unary operator: {type(node.op).__name__}"
            )
        return self._operation(node, operation, self.evaluate(node.operand))

    def _evaluate_Compare(self, node: ast.Compare) -> bool:
        left = self.evaluate(node.left)
        for operation_node, comparator_node in zip(
            node.ops, node.comparators, strict=True
        ):
            operation = _COMPARISON_OPERATORS.get(type(operation_node))
            if operation is None:
                raise self._error(
                    node,
                    f"unsupported comparison operator: {type(operation_node).__name__}",
                )
            right = self.evaluate(comparator_node)
            comparison_result = self._operation(node, operation, left, right)
            if not self._truth(node, comparison_result):
                return False
            left = right
        return True

    def _evaluate_IfExp(self, node: ast.IfExp) -> Any:
        test = self.evaluate(node.test)
        branch = node.body if self._truth(node.test, test) else node.orelse
        return self.evaluate(branch)

    def _evaluate_Call(self, node: ast.Call) -> Any:
        raise self._error(node, "function and method calls are not allowed")


@dataclass(frozen=True, slots=True)
class CompiledExpression:
    """A validated expression that can be evaluated repeatedly."""

    source: str
    tree: ast.Expression = field(repr=False)

    def evaluate(self, variables: Mapping[str, Any] | None = None) -> Any:
        """Evaluate this expression using *variables* as its name context."""

        context = _coerce_variables(variables, self.source)
        # Validate again so a caller mutating the public AST cannot bypass the
        # compile-time allow-list.  The interpreter also checks every node.
        try:
            _Validator(self.source).visit(self.tree)
        except RecursionError as exc:
            raise ExpressionError(
                "expression is too deeply nested", source=self.source
            ) from exc
        return _Evaluator(self.source, context).evaluate(self.tree)


def _coerce_variables(
    variables: Mapping[str, Any] | None, source: str
) -> Mapping[str, Any]:
    if variables is None:
        return {}
    if not isinstance(variables, Mapping):
        raise ExpressionError(
            "variables must be a mapping",
            source=source,
        )
    return variables


def compile_expression(source: str) -> CompiledExpression:
    """Parse and validate *source* without executing it."""

    if not isinstance(source, str):
        raise ExpressionError("expression source must be a string")
    try:
        tree = ast.parse(source, mode="eval")
    except (SyntaxError, ValueError) as exc:
        if isinstance(exc, SyntaxError):
            col_offset = exc.offset - 1 if exc.offset is not None else None
            raise ExpressionError(
                f"invalid expression syntax: {exc.msg}",
                source=source,
                lineno=exc.lineno,
                col_offset=col_offset,
            ) from exc
        raise ExpressionError(f"invalid expression: {exc}", source=source) from exc
    except RecursionError as exc:
        raise ExpressionError("expression is too deeply nested", source=source) from exc

    try:
        _Validator(source).visit(tree)
    except RecursionError as exc:
        raise ExpressionError("expression is too deeply nested", source=source) from exc
    return CompiledExpression(source=source, tree=tree)


def evaluate_expression(
    expression: str | CompiledExpression,
    variables: Mapping[str, Any] | None = None,
) -> Any:
    """Compile if necessary, then evaluate *expression* with *variables*."""

    if isinstance(expression, str):
        compiled = compile_expression(expression)
    elif isinstance(expression, CompiledExpression):
        compiled = expression
    else:
        raise ExpressionError(
            "expression must be a string or CompiledExpression"
        )
    return compiled.evaluate(variables)


def compile(source: str) -> CompiledExpression:
    """Short alias for :func:`compile_expression`."""

    return compile_expression(source)


def evaluate(
    expression: str | CompiledExpression,
    variables: Mapping[str, Any] | None = None,
) -> Any:
    """Short alias for :func:`evaluate_expression`."""

    return evaluate_expression(expression, variables)
