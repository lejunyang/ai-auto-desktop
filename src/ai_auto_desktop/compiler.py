"""Strict compiler for canonical v1alpha1 workflow descriptors."""

from __future__ import annotations

import json
import math
import re
from importlib import resources
from pathlib import Path
from typing import Any, Mapping, Sequence

from .errors import DescriptorError, DescriptorIssue
from .expression import ExpressionError, compile_expression
from .model import MISSING, CompiledStep, ErrorHandler, SwitchCase, WorkflowDescriptor, freeze

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

try:
    import jsonschema
except ImportError:  # pragma: no cover
    jsonschema = None

API_VERSION = "ai-auto-desktop.dev/v1alpha1"
KIND = "Workflow"
MAX_DESCRIPTOR_BYTES = 2 * 1024 * 1024

_STEP_ID = re.compile(r"^[a-z][a-z0-9_]{0,63}$")
_NAME = re.compile(r"^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*$")
_IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$")
_USES = re.compile(
    r"^[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*\.[a-z][a-z0-9_]*@[1-9][0-9]*$"
)
_DURATION = re.compile(r"^(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:ms|s|m|h)$")
_EXPRESSION = re.compile(r"^\$\{\{(.*?)\}\}$", re.DOTALL)
_TOP = {"apiVersion", "kind", "metadata", "requires", "inputs", "variables", "outputs", "defaults", "budgets", "policy", "steps", "on_error", "finally", "extensions"}
_COMMON = {"id", "type", "description", "if", "timeout", "attempt_timeout", "retry", "on_error", "finally", "extensions"}
_FIELDS = {
    "action": {"uses", "with", "effect", "risk", "precondition", "postcondition"},
    "set": {"assign"}, "if": {"condition", "then", "else"},
    "switch": {"cases", "default"},
    "foreach": {"items", "as", "index_as", "max_items", "concurrency", "steps"},
    "while": {"condition", "max_iterations", "steps"}, "block": {"steps"},
    "script": {"runtime", "source", "entrypoint", "inputs", "output_schema", "capabilities", "sandbox"},
    "fail": {"error"}, "return": {"value"},
}
_EFFECTS = {"read_only", "idempotent", "non_idempotent", "contextual"}
_RISK_CATEGORIES = {"observe", "navigate", "input", "modify", "send", "delete", "purchase", "authorize", "install", "execute_script", "capture_screen", "custom"}
_RISK_LEVELS = {"low", "medium", "high", "critical", "contextual"}

if yaml is not None:
    class _StrictLoader(yaml.SafeLoader):
        def compose_node(self, parent: Any, index: Any) -> Any:
            if self.check_event(yaml.AliasEvent):
                event = self.peek_event()
                raise yaml.constructor.ConstructorError(None, None, "YAML aliases are forbidden", event.start_mark)
            return super().compose_node(parent, index)

    def _mapping(loader: _StrictLoader, node: Any, deep: bool = False) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key_node, value_node in node.value:
            if key_node.tag == "tag:yaml.org,2002:merge" or key_node.value == "<<":
                raise yaml.constructor.ConstructorError(None, None, "YAML merge keys are forbidden", key_node.start_mark)
            key = loader.construct_object(key_node, deep=deep)
            if not isinstance(key, str):
                raise yaml.constructor.ConstructorError(None, None, "mapping keys must be strings", key_node.start_mark)
            if key in result:
                raise yaml.constructor.ConstructorError(None, None, f"duplicate key {key!r}", key_node.start_mark)
            result[key] = loader.construct_object(value_node, deep=deep)
        return result
    _StrictLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, _mapping)


def _object_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate key {key!r}")
        result[key] = value
    return result


def _parse(text: str, source: str) -> Any:
    if len(text.encode("utf-8")) > MAX_DESCRIPTOR_BYTES:
        raise DescriptorError(issues=[DescriptorIssue("$", "descriptor exceeds 2 MiB", "limit")])
    try:
        suffix = Path(source).suffix.lower()
        if suffix == ".json" or (suffix not in {".yaml", ".yml"} and text.lstrip().startswith(("{", "["))):
            return json.loads(text, object_pairs_hook=_object_pairs)
        if yaml is None:
            raise DescriptorError("YAML support is unavailable", code="DESCRIPTOR.UNSUPPORTED_FEATURE")
        return yaml.load(text, Loader=_StrictLoader)
    except DescriptorError:
        raise
    except Exception as exc:
        raise DescriptorError(f"Cannot parse descriptor: {exc}", issues=[DescriptorIssue("$", str(exc), "parse")], cause=exc) from exc


def load_descriptor(path: str | Path) -> WorkflowDescriptor:
    source = Path(path).expanduser().resolve()
    try:
        text = source.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise DescriptorError(f"Cannot read descriptor: {exc}", issues=[DescriptorIssue("$", str(exc), "read")], cause=exc) from exc
    return compile_descriptor(_parse(text, str(source)), source=source)


_WORKFLOW_SCHEMA_RESOURCE = (
    "schemas",
    "workflow",
    "v1alpha1",
    "workflow.schema.json",
)


def _workflow_schema() -> Any:
    if jsonschema is None:
        raise DescriptorError(
            "Canonical workflow schema validation is unavailable",
            code="DESCRIPTOR.UNSUPPORTED_FEATURE",
            details={"dependency": "jsonschema"},
        )

    resource_name = "/".join(_WORKFLOW_SCHEMA_RESOURCE)
    try:
        resource = resources.files("ai_auto_desktop").joinpath(
            *_WORKFLOW_SCHEMA_RESOURCE
        )
        schema = json.loads(resource.read_text(encoding="utf-8"))
        jsonschema.Draft202012Validator.check_schema(schema)
    except (OSError, TypeError, ValueError, jsonschema.SchemaError) as exc:
        raise DescriptorError(
            "Canonical workflow schema resource is unavailable or invalid",
            code="DESCRIPTOR.SCHEMA_UNAVAILABLE",
            details={"resource": resource_name},
            cause=exc,
        ) from exc
    return schema


def _schema_issues(descriptor: Any) -> list[DescriptorIssue]:
    """Validate against the packaged canonical schema before compilation."""

    schema = _workflow_schema()
    assert jsonschema is not None
    validator = jsonschema.Draft202012Validator(schema)
    issues: list[DescriptorIssue] = []
    for error in sorted(validator.iter_errors(descriptor), key=lambda item: list(item.absolute_path)):
        location = "$" + "".join(
            f"[{part}]" if isinstance(part, int) else f".{part}"
            for part in error.absolute_path
        )
        issues.append(DescriptorIssue(location, error.message, "schema"))
    return issues


def parse_duration(value: str | None, default: float | None = None) -> float | None:
    if value is None:
        return default
    if not isinstance(value, str):
        raise ValueError("duration must be a string such as 250ms or 2s")
    match = re.fullmatch(r"([1-9][0-9]*)(ms|s|m|h)", value)
    if not match:
        raise ValueError(f"invalid duration: {value!r}")
    return float(match.group(1)) * {"ms": .001, "s": 1, "m": 60, "h": 3600}[match.group(2)]


class _Compiler:
    def __init__(self, raw: Any, source: Path | None) -> None:
        self.raw, self.source = raw, source
        self.issues: list[DescriptorIssue] = []
        self.ids: dict[str, str] = {}

    def issue(self, path: str, message: str, code: str = "invalid") -> None:
        self.issues.append(DescriptorIssue(path, message, code))

    def obj(self, value: Any, path: str) -> Mapping[str, Any] | None:
        if not isinstance(value, Mapping):
            self.issue(path, "must be an object", "type"); return None
        return value

    def arr(self, value: Any, path: str) -> Sequence[Any] | None:
        if not isinstance(value, (list, tuple)):
            self.issue(path, "must be an array", "type"); return None
        return value

    def unknown(self, obj: Mapping[str, Any], allowed: set[str], path: str) -> None:
        for key in obj:
            if key not in allowed: self.issue(f"{path}.{key}", "unknown field", "unknown_field")

    def required(self, obj: Mapping[str, Any], fields: Sequence[str], path: str) -> None:
        for field in fields:
            if field not in obj: self.issue(f"{path}.{field}", "required field is missing", "required")

    def duration(self, value: Any, path: str, allow_zero: bool = False) -> None:
        try: seconds = parse_duration(value)
        except (TypeError, ValueError): seconds = None
        if seconds is None or not math.isfinite(seconds): self.issue(path, "must be a duration such as 250ms or 2s", "format")
        elif seconds < 0 or (seconds == 0 and not allow_zero): self.issue(path, "duration must be positive", "range")

    def strings(self, value: Any, path: str) -> None:
        items = self.arr(value, path)
        if items is not None:
            for index, item in enumerate(items):
                if not isinstance(item, str) or not item: self.issue(f"{path}[{index}]", "must be a non-empty string", "type")

    def expression(self, value: Any, path: str) -> None:
        if not isinstance(value, str): self.issue(path, "must be an expression string", "type"); return
        match = _EXPRESSION.fullmatch(value)
        if not match: self.issue(path, "must be one complete expression template", "expression"); return
        try: compile_expression(match.group(1).strip())
        except ExpressionError as exc: self.issue(path, str(exc), "expression")

    def values(self, value: Any, path: str) -> None:
        if isinstance(value, str):
            for match in re.finditer(r"\$\{\{(.*?)\}\}", value, re.DOTALL):
                try: compile_expression(match.group(1).strip())
                except ExpressionError as exc: self.issue(path, str(exc), "expression")
            if "$" + "{{" in value and not re.search(r"\$\{\{.*?\}\}", value, re.DOTALL): self.issue(path, "unterminated expression", "expression")
        elif isinstance(value, Mapping):
            for key, item in value.items(): self.values(item, f"{path}.{key}")
        elif isinstance(value, (list, tuple)):
            for index, item in enumerate(value): self.values(item, f"{path}[{index}]")

    def retry(self, value: Any, path: str) -> None:
        obj = self.obj(value, path)
        if obj is None: return
        self.unknown(obj, {"max_attempts", "backoff", "on"}, path); self.required(obj, ["max_attempts"], path)
        attempts = obj.get("max_attempts")
        if not isinstance(attempts, int) or isinstance(attempts, bool) or attempts < 1: self.issue(f"{path}.max_attempts", "must be a positive integer", "range")
        if "backoff" in obj:
            backoff = self.obj(obj["backoff"], f"{path}.backoff")
            if backoff is not None:
                self.unknown(backoff, {"strategy", "initial_delay", "max_delay", "multiplier", "jitter"}, f"{path}.backoff"); self.required(backoff, ["strategy", "initial_delay"], f"{path}.backoff")
                if backoff.get("strategy") not in {"fixed", "exponential"}: self.issue(f"{path}.backoff.strategy", "must be fixed or exponential", "enum")
                for key in ("initial_delay", "max_delay"):
                    if key in backoff: self.duration(backoff[key], f"{path}.backoff.{key}", True)
                if "multiplier" in backoff and (not isinstance(backoff["multiplier"], (int, float)) or isinstance(backoff["multiplier"], bool) or backoff["multiplier"] < 1): self.issue(f"{path}.backoff.multiplier", "must be at least 1", "range")
                if "jitter" in backoff and (not isinstance(backoff["jitter"], (int, float)) or isinstance(backoff["jitter"], bool) or not 0 <= backoff["jitter"] <= 1): self.issue(f"{path}.backoff.jitter", "must be between 0 and 1", "range")
        if "on" in obj:
            on = self.obj(obj["on"], f"{path}.on")
            if on is not None:
                self.unknown(on, {"codes", "categories"}, f"{path}.on")
                for key in ("codes", "categories"):
                    if key in on: self.strings(on[key], f"{path}.on.{key}")

    def effect(self, value: Any, path: str) -> None:
        obj = self.obj(value, path)
        if obj is None: return
        self.unknown(obj, {"class"}, path); self.required(obj, ["class"], path)
        if obj.get("class") not in _EFFECTS: self.issue(f"{path}.class", "invalid effect class", "enum")

    def risk(self, value: Any, path: str) -> None:
        risk = self.obj(value, path)
        if risk is not None:
            self.unknown(risk, {"category", "level", "custom_name"}, path); self.required(risk, ["category", "level"], path)
            if risk.get("category") not in _RISK_CATEGORIES: self.issue(f"{path}.category", "invalid risk category", "enum")
            if risk.get("level") not in _RISK_LEVELS: self.issue(f"{path}.level", "invalid risk level", "enum")
            if risk.get("category") == "custom" and not isinstance(risk.get("custom_name"), str): self.issue(f"{path}.custom_name", "required for custom risk", "required")
            if risk.get("category") != "custom" and "custom_name" in risk: self.issue(f"{path}.custom_name", "only valid for custom risk", "policy")

    def observation(self, value: Any, path: str) -> None:
        obj = self.obj(value, path)
        if obj is None: return
        self.unknown(obj, {"uses", "with"}, path); self.required(obj, ["uses", "with"], path)
        if not isinstance(obj.get("uses"), str) or not _USES.fullmatch(obj.get("uses", "")):
            self.issue(f"{path}.uses", "must match capability.action@major", "format")
        if "with" in obj:
            if not isinstance(obj["with"], Mapping): self.issue(f"{path}.with", "must be an object", "type")
            self.values(obj["with"], f"{path}.with")

    def assertion(self, value: Any, path: str, post: bool) -> None:
        obj = self.obj(value, path)
        if obj is None: return
        allowed = {"condition", "message", "timeout", "poll_interval"}
        if post: allowed.add("observe")
        self.unknown(obj, allowed, path); self.required(obj, ["condition"], path)
        if "condition" in obj: self.expression(obj["condition"], f"{path}.condition")
        if "message" in obj and not isinstance(obj["message"], str): self.issue(f"{path}.message", "must be a string", "type")
        if post and "observe" in obj: self.observation(obj["observe"], f"{path}.observe")
        for key in ("timeout", "poll_interval"):
            if key in obj:
                if not post: self.issue(f"{path}.{key}", "preconditions cannot poll", "unsupported")
                self.duration(obj[key], f"{path}.{key}")

    def handler(self, value: Any, path: str) -> ErrorHandler | None:
        obj = self.obj(value, path)
        if obj is None: return None
        self.unknown(obj, {"match", "as", "steps", "outcome"}, path); self.required(obj, ["steps", "outcome"], path)
        codes, categories, effects = ("*",), (), ()
        if "match" in obj:
            match = self.obj(obj["match"], f"{path}.match")
            if match is not None:
                self.unknown(match, {"codes", "categories", "effects"}, f"{path}.match")
                for key in ("codes", "categories", "effects"):
                    if key in match: self.strings(match[key], f"{path}.match.{key}")
                codes = tuple(match.get("codes", ())); categories = tuple(match.get("categories", ())); effects = tuple(match.get("effects", ()))
        as_name = obj.get("as", "error")
        if not isinstance(as_name, str) or not _IDENT.fullmatch(as_name): self.issue(f"{path}.as", "must be an identifier", "format"); as_name = "error"
        outcome = self.obj(obj.get("outcome"), f"{path}.outcome") if "outcome" in obj else None
        mode, output = "rethrow", MISSING
        if outcome is not None:
            self.unknown(outcome, {"mode", "output"}, f"{path}.outcome"); self.required(outcome, ["mode"], f"{path}.outcome")
            mode = outcome.get("mode", "rethrow")
            if mode not in {"rethrow", "continue", "return"}: self.issue(f"{path}.outcome.mode", "invalid outcome mode", "enum"); mode = "rethrow"
            if "output" in outcome: self.values(outcome["output"], f"{path}.outcome.output"); output = freeze(outcome["output"])
        return ErrorHandler(steps=self.steps(obj.get("steps", []), f"{path}.steps"), match_codes=codes, match_categories=categories, match_effects=effects, as_name=as_name, mode=mode, output=output)

    def steps(self, value: Any, path: str) -> tuple[CompiledStep, ...]:
        items = self.arr(value, path)
        if items is None: return ()
        result = []
        for index, value in enumerate(items):
            step = self.step(value, f"{path}[{index}]")
            if step is not None: result.append(step)
        return tuple(result)

    def step(self, value: Any, path: str) -> CompiledStep | None:
        obj = self.obj(value, path)
        if obj is None: return None
        self.required(obj, ["id", "type"], path)
        step_type = obj.get("type"); self.unknown(obj, _COMMON | _FIELDS.get(str(step_type), set()), path)
        if step_type not in _FIELDS: self.issue(f"{path}.type", f"unsupported step type {step_type!r}", "enum"); step_type = str(step_type or "invalid")
        step_id = obj.get("id")
        if not isinstance(step_id, str) or not _STEP_ID.fullmatch(step_id): self.issue(f"{path}.id", "invalid step id", "format"); step_id = f"invalid_{len(self.ids)}"
        elif step_id in self.ids: self.issue(f"{path}.id", f"duplicate step id; first at {self.ids[step_id]}", "duplicate")
        else: self.ids[step_id] = f"{path}.id"
        if "if" in obj: self.expression(obj["if"], f"{path}.if")
        for key in ("timeout", "attempt_timeout"):
            if key in obj: self.duration(obj[key], f"{path}.{key}")
        if "retry" in obj: self.retry(obj["retry"], f"{path}.retry")
        handler = self.handler(obj["on_error"], f"{path}.on_error") if "on_error" in obj else None
        final = self.steps(obj["finally"], f"{path}.finally") if "finally" in obj else ()
        excluded = {"id", "type", "on_error", "finally", "steps", "then", "else", "cases", "default"}
        params = freeze({key: item for key, item in obj.items() if key not in excluded})
        nested = then_steps = else_steps = default_steps = (); cases: tuple[SwitchCase, ...] = ()
        if step_type == "action":
            self.required(obj, ["uses", "with"], path)
            if not isinstance(obj.get("uses"), str) or not _USES.fullmatch(obj.get("uses", "")): self.issue(f"{path}.uses", "must match capability.action@major", "format")
            if "with" in obj:
                if not isinstance(obj["with"], Mapping): self.issue(f"{path}.with", "must be an object", "type")
                self.values(obj["with"], f"{path}.with")
            if "effect" in obj: self.effect(obj["effect"], f"{path}.effect")
            if "risk" in obj: self.risk(obj["risk"], f"{path}.risk")
            if "precondition" in obj: self.assertion(obj["precondition"], f"{path}.precondition", False)
            if "postcondition" in obj: self.assertion(obj["postcondition"], f"{path}.postcondition", True)
        elif step_type == "set":
            self.required(obj, ["assign"], path); assign = self.obj(obj.get("assign"), f"{path}.assign") if "assign" in obj else None
            if assign is not None:
                if not assign: self.issue(f"{path}.assign", "must not be empty", "range")
                for target, assigned in assign.items():
                    if not re.fullmatch(r"vars\.[A-Za-z_][A-Za-z0-9_]*", target): self.issue(f"{path}.assign.{target}", "target must be vars.name", "format")
                    self.values(assigned, f"{path}.assign.{target}")
        elif step_type == "if":
            self.required(obj, ["condition", "then"], path)
            if "condition" in obj: self.expression(obj["condition"], f"{path}.condition")
            then_steps = self.steps(obj.get("then", []), f"{path}.then"); else_steps = self.steps(obj["else"], f"{path}.else") if "else" in obj else ()
        elif step_type == "switch":
            self.required(obj, ["cases"], path); raw_cases = self.arr(obj.get("cases"), f"{path}.cases") if "cases" in obj else None; built = []
            if raw_cases is not None:
                for index, raw_case in enumerate(raw_cases):
                    case_path = f"{path}.cases[{index}]"; case = self.obj(raw_case, case_path)
                    if case is None: continue
                    self.unknown(case, {"when", "steps"}, case_path); self.required(case, ["when", "steps"], case_path)
                    if "when" in case: self.expression(case["when"], f"{case_path}.when")
                    built.append(SwitchCase(steps=self.steps(case.get("steps", []), f"{case_path}.steps"), when=freeze(case.get("when"))))
            cases = tuple(built); default_steps = self.steps(obj["default"], f"{path}.default") if "default" in obj else ()
        elif step_type in {"foreach", "while"}:
            required = ["items", "as", "max_items", "steps"] if step_type == "foreach" else ["condition", "max_iterations", "timeout", "steps"]
            self.required(obj, required, path); expression_key = "items" if step_type == "foreach" else "condition"
            if expression_key in obj: self.expression(obj[expression_key], f"{path}.{expression_key}")
            if step_type == "foreach":
                for key in ("as", "index_as"):
                    if key in obj and (not isinstance(obj[key], str) or not _IDENT.fullmatch(obj[key])): self.issue(f"{path}.{key}", "must be an identifier", "format")
            limit_key = "max_items" if step_type == "foreach" else "max_iterations"
            limit = obj.get(limit_key)
            if not isinstance(limit, int) or isinstance(limit, bool) or limit < 1: self.issue(f"{path}.{limit_key}", "must be a positive integer", "range")
            if step_type == "foreach" and "concurrency" in obj and (not isinstance(obj["concurrency"], int) or isinstance(obj["concurrency"], bool) or obj["concurrency"] < 1): self.issue(f"{path}.concurrency", "must be a positive integer", "range")
            nested = self.steps(obj.get("steps", []), f"{path}.steps")
        elif step_type == "block": self.required(obj, ["steps"], path); nested = self.steps(obj.get("steps", []), f"{path}.steps")
        elif step_type == "script":
            self.required(obj, ["runtime", "output_schema"], path)
            if obj.get("runtime") not in {"python", "javascript", "shell"}: self.issue(f"{path}.runtime", "invalid script runtime", "enum")
            if ("source" in obj) == ("entrypoint" in obj): self.issue(path, "exactly one of source and entrypoint is required", "one_of")
            if "inputs" in obj:
                if not isinstance(obj["inputs"], Mapping): self.issue(f"{path}.inputs", "must be an object", "type")
                self.values(obj["inputs"], f"{path}.inputs")
            if "output_schema" in obj and not isinstance(obj["output_schema"], (Mapping, bool)):
                self.issue(f"{path}.output_schema", "must be an object or boolean JSON Schema", "type")
            if "capabilities" in obj: self.strings(obj["capabilities"], f"{path}.capabilities")
            if "source" in obj and "$" + "{{" in obj["source"]: self.issue(f"{path}.source", "expressions are forbidden in source", "policy")
            if "sandbox" in obj:
                sandbox = self.obj(obj["sandbox"], f"{path}.sandbox")
                if sandbox is not None: self.unknown(sandbox, {"network", "filesystem", "environment", "max_output_bytes"}, f"{path}.sandbox")
        elif step_type == "fail":
            self.required(obj, ["error"], path); error = self.obj(obj.get("error"), f"{path}.error") if "error" in obj else None
            if error is not None:
                self.unknown(error, {"code", "message", "category", "retryable", "effect", "details"}, f"{path}.error"); self.required(error, ["code", "message"], f"{path}.error")
                if not isinstance(error.get("code"), str) or not re.fullmatch(r"[A-Z][A-Z0-9_]*(?:\.[A-Z][A-Z0-9_]*)+", error.get("code", "")): self.issue(f"{path}.error.code", "must be an uppercase dotted code", "format")
                if not isinstance(error.get("message"), str): self.issue(f"{path}.error.message", "must be a string", "type")
                self.values(error, f"{path}.error")
        elif step_type == "return" and "value" in obj: self.values(obj["value"], f"{path}.value")
        return CompiledStep(id=step_id, type=step_type, path=path, params=params, steps=nested, then_steps=then_steps, else_steps=else_steps, cases=cases, default_steps=default_steps, on_error=handler, finally_steps=final)

    def named(self, value: Any, path: str, kind: str) -> Mapping[str, Any]:
        obj = self.obj(value, path)
        if obj is None: return {}
        fields = {"inputs": {"schema", "required", "default", "sensitive"}, "variables": {"schema", "mutable", "initial"}, "outputs": {"value", "schema", "sensitive"}}[kind]
        required = {"inputs": {"schema"}, "variables": {"schema"}, "outputs": {"value"}}[kind]
        for name, raw in obj.items():
            item = self.obj(raw, f"{path}.{name}")
            if not _IDENT.fullmatch(name): self.issue(f"{path}.{name}", "name must be an identifier", "format")
            if item is not None:
                self.unknown(item, fields, f"{path}.{name}"); self.required(item, required, f"{path}.{name}")
                if "schema" in item and not isinstance(item["schema"], (Mapping, bool)):
                    self.issue(f"{path}.{name}.schema", "must be an object or boolean JSON Schema", "type")
                for flag in ("required", "sensitive", "mutable"):
                    if flag in item and not isinstance(item[flag], bool): self.issue(f"{path}.{name}.{flag}", "must be a boolean", "type")
                if kind == "inputs" and item.get("required") is True and "default" in item:
                    self.issue(f"{path}.{name}", "required input cannot also define a default", "policy")
                for key in ("default", "initial", "value"):
                    if key in item: self.values(item[key], f"{path}.{name}.{key}")
        return obj

    def compile(self) -> WorkflowDescriptor:
        root = self.obj(self.raw, "$")
        if root is None: raise DescriptorError(issues=self.issues)
        self.unknown(root, _TOP, "$"); self.required(root, ["apiVersion", "kind", "metadata", "budgets", "steps"], "$")
        if root.get("apiVersion") != API_VERSION: self.issue("$.apiVersion", f"only {API_VERSION} is supported", "unsupported_version")
        if root.get("kind") != KIND: self.issue("$.kind", "must be Workflow", "enum")
        metadata = self.obj(root.get("metadata"), "$.metadata") if "metadata" in root else {}; metadata = metadata or {}
        self.unknown(metadata, {"name", "version", "description", "labels", "annotations"}, "$.metadata"); self.required(metadata, ["name"], "$.metadata")
        if not isinstance(metadata.get("name"), str) or not _NAME.fullmatch(metadata.get("name", "")): self.issue("$.metadata.name", "invalid workflow name", "format")
        inputs = self.named(root["inputs"], "$.inputs", "inputs") if "inputs" in root else {}; variables = self.named(root["variables"], "$.variables", "variables") if "variables" in root else {}; outputs = self.named(root["outputs"], "$.outputs", "outputs") if "outputs" in root else {}
        defaults = self.obj(root["defaults"], "$.defaults") if "defaults" in root else {}; defaults = defaults or {}; self.unknown(defaults, {"timeout", "retry"}, "$.defaults")
        if "timeout" in defaults: self.duration(defaults["timeout"], "$.defaults.timeout")
        if "retry" in defaults: self.retry(defaults["retry"], "$.defaults.retry")
        budgets = self.obj(root["budgets"], "$.budgets") if "budgets" in root else {}; budgets = budgets or {}; self.unknown(budgets, {"max_duration", "max_executed_steps", "cleanup_timeout"}, "$.budgets")
        if "budgets" in root: self.required(budgets, ["max_duration", "max_executed_steps"], "$.budgets")
        for key in ("max_duration", "cleanup_timeout"):
            if key in budgets: self.duration(budgets[key], f"$.budgets.{key}")
        if "max_executed_steps" in budgets and (not isinstance(budgets["max_executed_steps"], int) or isinstance(budgets["max_executed_steps"], bool) or budgets["max_executed_steps"] < 1): self.issue("$.budgets.max_executed_steps", "must be a positive integer", "range")
        requires = self.obj(root["requires"], "$.requires") if "requires" in root else {}; policy = self.obj(root["policy"], "$.policy") if "policy" in root else {}; extensions = self.obj(root["extensions"], "$.extensions") if "extensions" in root else {}
        requires, policy, extensions = requires or {}, policy or {}, extensions or {}
        self.unknown(requires, {"runtime", "platforms", "capabilities", "permissions"}, "$.requires"); self.unknown(policy, {"allowed_risk", "confirmation", "untrusted_inputs", "screenshots", "desktop"}, "$.policy")
        for key in policy:
            if not isinstance(policy[key], Mapping): self.issue(f"$.policy.{key}", "must be an object", "type")
        for key in extensions:
            if "/" not in key: self.issue(f"$.extensions.{key}", "extension name must use domain/name", "format")
        steps = self.steps(root.get("steps", []), "$.steps")
        if not steps: self.issue("$.steps", "must contain at least one step", "range")
        handler = self.handler(root["on_error"], "$.on_error") if "on_error" in root else None; final = self.steps(root["finally"], "$.finally") if "finally" in root else ()
        mutable = {name for name, definition in variables.items() if isinstance(definition, Mapping) and definition.get("mutable", False)}
        for step in self.walk(steps, handler, final):
            if step.type == "set":
                for target in step.params.get("assign", {}):
                    name = target.partition(".")[2]
                    if name not in variables: self.issue(f"{step.path}.assign.{target}", "variable is not declared", "reference")
                    elif name not in mutable: self.issue(f"{step.path}.assign.{target}", "variable is immutable", "policy")
        if self.issues: raise DescriptorError(issues=self.issues)
        return WorkflowDescriptor(api_version=API_VERSION, name=metadata["name"], steps=steps, source=self.source, description=metadata.get("description"), metadata=freeze(metadata), inputs=freeze(inputs), variables=freeze(variables), outputs=freeze(outputs), requires=freeze(requires), defaults=freeze(defaults), budgets=freeze(budgets), policy=freeze(policy), extensions=freeze(extensions), on_error=handler, finally_steps=final, raw=freeze(root))

    def walk(self, steps: tuple[CompiledStep, ...], handler: ErrorHandler | None, final: tuple[CompiledStep, ...]) -> list[CompiledStep]:
        result: list[CompiledStep] = []
        def visit(items: tuple[CompiledStep, ...]) -> None:
            for step in items:
                result.append(step); visit(step.steps); visit(step.then_steps); visit(step.else_steps)
                for case in step.cases: visit(case.steps)
                visit(step.default_steps)
                if step.on_error: visit(step.on_error.steps)
                visit(step.finally_steps)
        visit(steps)
        if handler: visit(handler.steps)
        visit(final); return result


def compile_descriptor(descriptor: Mapping[str, Any] | Any, *, source: str | Path | None = None) -> WorkflowDescriptor:
    schema_issues = _schema_issues(descriptor)
    if schema_issues:
        raise DescriptorError(issues=schema_issues)
    return _Compiler(descriptor, Path(source).expanduser().resolve() if source is not None else None).compile()


__all__ = ["API_VERSION", "KIND", "DescriptorError", "compile_descriptor", "load_descriptor", "parse_duration"]
