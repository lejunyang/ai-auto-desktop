"""Synchronous v0 workflow execution engine."""

from __future__ import annotations

import fnmatch
import random
import re
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any

from .compiler import parse_duration
from .errors import AutomationError, ensure_automation_error
from .expression import ExpressionError, evaluate_expression
from .model import MISSING, CompiledStep, ErrorHandler, RunResult, WorkflowDescriptor, thaw
from .plugin import PluginError, ProcessPlugin
from .script import execute_python_script

try:
    import jsonschema
except ImportError:  # pragma: no cover
    jsonschema = None

_TEMPLATE = re.compile(r"\$\{\{(.*?)\}\}", re.DOTALL)
RUNTIME_VERSION = "0.1.0"


class _ReturnFlow(Exception):
    def __init__(self, value: Any) -> None:
        self.value = value


class WorkflowRunner:
    def __init__(
        self,
        descriptor: WorkflowDescriptor,
        *,
        plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None = None,
        allow_scripts: bool = False,
        granted_permissions: Sequence[str] | None = None,
        event_sink: Callable[[Mapping[str, Any]], None] | None = None,
    ) -> None:
        self.descriptor = descriptor
        self.allow_scripts = allow_scripts
        self.granted_permissions = frozenset(granted_permissions or ())
        self.event_sink = event_sink
        self.plugins: dict[str, ProcessPlugin] = {}
        self._owned: set[str] = set()
        for name, plugin in (plugins or {}).items():
            if isinstance(plugin, ProcessPlugin):
                self.plugins[name] = plugin
            else:
                command = [plugin] if isinstance(plugin, str) else list(plugin)
                self.plugins[name] = ProcessPlugin(command, name=name)
                self._owned.add(name)
        self.events: list[dict[str, Any]] = []
        self.step_records: dict[str, dict[str, Any]] = {}
        self.context: dict[str, Any] = {}
        self.variables: dict[str, Any] = {}
        self._deadline: float | None = None
        self._deadline_stack: list[float] = []
        self._executed = 0

    def run(self, inputs: Mapping[str, Any] | None = None) -> RunResult:
        self.events, self.step_records, self._executed = [], {}, 0
        self._deadline_stack = []
        self._handler_output = MISSING
        budget = _duration(self.descriptor.budgets.get("max_duration"))
        self._deadline = time.monotonic() + budget if budget else None
        error: AutomationError | None = None
        output: Any = MISSING
        try:
            actual_inputs = self._prepare_inputs(dict(inputs or {}))
            self.context = {"inputs": actual_inputs, "vars": {}, "steps": {}}
            self.variables = self._prepare_variables()
            self.context["vars"] = self.variables
            self._check_requirements()
            try:
                self._run_steps(self.descriptor.steps)
            except _ReturnFlow as returned:
                output = returned.value
            except AutomationError as caught:
                error = self._apply_handler(self.descriptor.on_error, caught)
                if error is None:
                    output = getattr(self, "_handler_output", None)
            if output is MISSING and error is None:
                output = self._workflow_outputs()
        except _ReturnFlow as returned:
            output = returned.value
        except AutomationError as caught:
            error = caught
        except Exception as caught:
            error = ensure_automation_error(caught)

        cleanup_timeout = _duration(self.descriptor.budgets.get("cleanup_timeout"), 5.0) or 5.0
        original_deadline = self._deadline
        self._deadline = time.monotonic() + cleanup_timeout
        try:
            self._run_steps(self.descriptor.finally_steps, cleanup=True)
        except _ReturnFlow:
            pass
        except AutomationError as cleanup_error:
            if error is not None:
                error.add_suppressed(cleanup_error)
            else:
                error = AutomationError("WORKFLOW.FINALLY_FAILED", "Workflow cleanup failed", phase="cleanup", cause=cleanup_error)
        finally:
            self._deadline = original_deadline
            for name in self._owned:
                self.plugins[name].close()

        status = "succeeded"
        if error is not None:
            if error.code == "ACTION.UNKNOWN_EFFECT" or error.effect == "unknown": status = "unknown_effect"
            elif error.code in {
                "WORKFLOW.TIMEOUT",
                "ACTION.TIMEOUT",
                "STEP.TIMEOUT",
                "SCRIPT.TIMEOUT",
            }:
                status = "timed_out"
            elif error.code == "WORKFLOW.CANCELLED": status = "cancelled"
            else: status = "failed"
        return RunResult(
            status,
            None if output is MISSING else output,
            dict(self.variables),
            error,
            list(self.events),
            dict(self.step_records),
        )

    def close(self) -> None:
        for name in self._owned: self.plugins[name].close()

    def _prepare_inputs(self, supplied: dict[str, Any]) -> dict[str, Any]:
        extras = set(supplied) - set(self.descriptor.inputs)
        if extras: raise AutomationError("INPUT.UNKNOWN", f"Unknown inputs: {', '.join(sorted(extras))}", category="input")
        result: dict[str, Any] = {}
        for name, definition in self.descriptor.inputs.items():
            if name in supplied: value = supplied[name]
            elif "default" in definition: value = thaw(definition["default"])
            elif definition.get("required", False): raise AutomationError("INPUT.REQUIRED", f"Required input {name!r} is missing", category="input")
            else: continue
            self._validate_schema(value, definition.get("schema"), "INPUT.INVALID", name)
            result[name] = value
        return result

    def _prepare_variables(self) -> dict[str, Any]:
        result: dict[str, Any] = {}
        local = {"inputs": self.context["inputs"], "vars": result, "steps": {}}
        for name, definition in self.descriptor.variables.items():
            if "initial" in definition:
                value = self._evaluate(thaw(definition["initial"]), local)
                self._validate_schema(value, definition.get("schema"), "VARIABLE.INVALID", name)
                result[name] = value
        return result

    def _workflow_outputs(self) -> Any:
        if not self.descriptor.outputs: return None
        result: dict[str, Any] = {}
        for name, definition in self.descriptor.outputs.items():
            value = self._evaluate(thaw(definition["value"]))
            self._validate_schema(value, definition.get("schema"), "OUTPUT.INVALID", name)
            result[name] = value
        return result

    def _check_requirements(self) -> None:
        requirements = thaw(self.descriptor.requires)
        runtime_range = requirements.get("runtime")
        if runtime_range and not _version_matches(RUNTIME_VERSION, runtime_range):
            raise AutomationError(
                "DESCRIPTOR.VERSION_UNSUPPORTED",
                f"Runtime {RUNTIME_VERSION} does not satisfy {runtime_range!r}",
                category="descriptor",
            )
        current_platform = _platform_name()
        allowed_platforms = requirements.get("platforms")
        if allowed_platforms and current_platform not in allowed_platforms:
            raise AutomationError(
                "CAPABILITY.PLATFORM_UNSUPPORTED",
                f"Workflow does not support platform {current_platform!r}",
                category="capability",
            )
        missing_permissions = sorted(
            set(requirements.get("permissions", ())) - self.granted_permissions
        )
        if missing_permissions:
            raise AutomationError(
                "POLICY.DENIED",
                "Workflow permissions were not granted",
                category="policy",
                details={"missing_permissions": missing_permissions},
            )
        for required in requirements.get("capabilities", ()):
            name = required["name"]
            plugin = self.plugins.get(name)
            if plugin is None:
                if required.get("optional", False):
                    continue
                raise AutomationError(
                    "CAPABILITY.MISSING",
                    f"Required capability {name!r} is not registered",
                    category="capability",
                )
            manifest = self._plugin_manifest(
                plugin, timeout_code="WORKFLOW.TIMEOUT"
            )
            if manifest.get("metadata", {}).get("name") != name:
                raise AutomationError(
                    "CAPABILITY.MISSING",
                    f"Registered plugin does not provide {name!r}",
                    category="capability",
                )
            version_range = required.get("version")
            version = manifest.get("metadata", {}).get("version")
            if version_range and (not isinstance(version, str) or not _version_matches(version, version_range)):
                raise AutomationError(
                    "CAPABILITY.VERSION_INCOMPATIBLE",
                    f"Capability {name!r} version {version!r} does not satisfy {version_range!r}",
                    category="capability",
                )
            missing_actions = sorted(
                set(required.get("actions", ()))
                - set(manifest.get("actions", {}))
            )
            if missing_actions:
                raise AutomationError(
                    "CAPABILITY.MISSING",
                    f"Capability {name!r} is missing required actions",
                    category="capability",
                    details={"actions": missing_actions},
                )

    def _validate_schema(self, value: Any, schema: Any, code: str, name: str) -> None:
        if schema is None or schema is True: return
        if jsonschema is None:
            raise AutomationError(
                "RUNTIME.DEPENDENCY_MISSING",
                "jsonschema is required to enforce value contracts",
                category="runtime",
            )
        try: jsonschema.validate(value, thaw(schema))
        except Exception as exc: raise AutomationError(code, f"{name!r} does not satisfy its schema", details={"validation": str(exc)}) from exc

    def _evaluate(self, value: Any, context: Mapping[str, Any] | None = None) -> Any:
        context = context or self.context
        if isinstance(value, Mapping): return {key: self._evaluate(item, context) for key, item in value.items()}
        if isinstance(value, (list, tuple)): return [self._evaluate(item, context) for item in value]
        if not isinstance(value, str): return value
        matches = list(_TEMPLATE.finditer(value))
        if not matches: return value
        try:
            if len(matches) == 1 and matches[0].span() == (0, len(value)):
                return evaluate_expression(matches[0].group(1).strip(), context)
            cursor, chunks = 0, []
            for match in matches:
                chunks += [value[cursor:match.start()], str(evaluate_expression(match.group(1).strip(), context))]
                cursor = match.end()
            chunks.append(value[cursor:]); return "".join(chunks)
        except ExpressionError as exc: raise AutomationError("EXPR.EVALUATION_FAILED", str(exc), category="expr", cause=exc) from exc

    def _remaining(self, local_deadline: float | None = None) -> float | None:
        deadlines = [
            item
            for item in (self._deadline, local_deadline, *self._deadline_stack)
            if item is not None
        ]
        return max(0.0, min(deadlines) - time.monotonic()) if deadlines else None

    def _check_budget(self, cleanup: bool = False) -> None:
        if self._remaining() == 0: raise AutomationError("WORKFLOW.TIMEOUT", "Workflow deadline exceeded", phase="execute")
        limit = self.descriptor.budgets.get("max_executed_steps")
        if limit is not None and self._executed >= limit and not cleanup: raise AutomationError("WORKFLOW.STEP_LIMIT", "Workflow step budget exceeded", details={"max_executed_steps": limit})

    def _event(self, event: str, **fields: Any) -> None:
        record = {"event": event, "time": time.time(), **fields}; self.events.append(record)
        if self.event_sink: self.event_sink(record)

    def _run_steps(self, steps: Sequence[CompiledStep], cleanup: bool = False) -> None:
        for step in steps: self._run_step(step, cleanup)

    def _run_step(self, step: CompiledStep, cleanup: bool = False) -> Any:
        self._check_budget(cleanup)
        if "if" in step.params and not bool(self._evaluate(thaw(step.params["if"]))):
            self.step_records[step.id] = {"status": "skipped"}
            self.context["steps"][step.id] = {"status": "skipped", "output": None}
            self._event("step.skipped", step_id=step.id)
            self._run_steps(step.finally_steps, cleanup)
            return None
        started = time.monotonic()
        self.step_records[step.id] = {"status": "running", "attempts": 0}; self._event("step.started", step_id=step.id, step_type=step.type)
        timeout = _duration(step.params.get("timeout"), _duration(self.descriptor.defaults.get("timeout")))
        local_deadline = started + timeout if timeout else None
        if local_deadline is not None:
            self._deadline_stack.append(local_deadline)
        pending: AutomationError | None = None
        result: Any = None
        try:
            result = self._attempt_step(step, local_deadline)
            self.context["steps"][step.id] = {"status": "succeeded", "output": result}
        except _ReturnFlow:
            self.step_records[step.id].update(status="succeeded", duration_ms=round((time.monotonic() - started) * 1000, 3))
            raise
        except AutomationError as caught:
            caught.at_step(step.id, step_path=step.path, workflow=self.descriptor.name); pending = self._apply_handler(step.on_error, caught)
            if pending is not None:
                status = "unknown_effect" if pending.effect == "unknown" else "timed_out" if pending.code.endswith(".TIMEOUT") else "failed"
                self.step_records[step.id].update(status=status, error=pending.to_dict(), duration_ms=round((time.monotonic() - started) * 1000, 3))
                raise pending
            result = getattr(self, "_handler_output", None); self.context["steps"][step.id] = {"status": "continued", "output": result}
        finally:
            if local_deadline is not None:
                self._deadline_stack.pop()
            try: self._run_steps(step.finally_steps, cleanup)
            except AutomationError as final_error:
                if pending is not None: pending.add_suppressed(final_error)
                else: raise AutomationError("WORKFLOW.FINALLY_FAILED", f"Finally for step {step.id!r} failed", cause=final_error) from final_error
        elapsed = time.monotonic() - started
        self.step_records[step.id].update(status="succeeded", output=result, duration_ms=round(elapsed * 1000, 3)); self._event("step.succeeded", step_id=step.id)
        return result

    def _attempt_step(self, step: CompiledStep, local_deadline: float | None) -> Any:
        retry = thaw(step.params.get("retry", self.descriptor.defaults.get("retry", {"max_attempts": 1})))
        max_attempts = int(retry.get("max_attempts", 1))
        effect = (
            thaw(step.params.get("effect", {})).get("class", "contextual")
            if step.type == "action"
            else "idempotent"
        )
        for attempt in range(1, max_attempts + 1):
            attempt_started = time.monotonic()
            self._check_budget()
            self._executed += 1
            self.step_records[step.id]["attempts"] = attempt; remaining = self._remaining(local_deadline)
            if remaining == 0: raise AutomationError("STEP.TIMEOUT", f"Step {step.id!r} timed out")
            attempt_timeout = _duration(step.params.get("attempt_timeout"), remaining)
            if remaining is not None: attempt_timeout = min(attempt_timeout, remaining) if attempt_timeout is not None else remaining
            attempt_deadline = (
                time.monotonic() + attempt_timeout
                if attempt_timeout is not None
                else None
            )
            try:
                if attempt_deadline is not None:
                    self._deadline_stack.append(attempt_deadline)
                try:
                    contract: Mapping[str, Any] | None = None
                    if step.type == "action":
                        contract = self._resolve_action_contract(step)
                        effect = self._effective_action_effect(
                            step, contract=contract
                        )
                    effective_timeout = self._remaining(attempt_deadline)
                    if effective_timeout == 0:
                        raise AutomationError(
                            "STEP.TIMEOUT", f"Step {step.id!r} timed out"
                        )
                    if step.type == "action":
                        assert contract is not None
                        manifest_timeout = _duration(contract.get("timeout"))
                        if manifest_timeout is not None:
                            manifest_remaining = max(
                                0.0,
                                attempt_started
                                + manifest_timeout
                                - time.monotonic(),
                            )
                            if manifest_remaining == 0:
                                raise AutomationError(
                                    "ACTION.TIMEOUT",
                                    "Action deadline expired before dispatch",
                                    category="action",
                                    retryable=True,
                                    effect="not_applied",
                                )
                            effective_timeout = (
                                min(effective_timeout, manifest_remaining)
                                if effective_timeout is not None
                                else manifest_remaining
                            )
                    return self._execute(
                        step, effective_timeout, action_contract=contract
                    )
                finally:
                    if attempt_deadline is not None:
                        self._deadline_stack.pop()
            except _ReturnFlow: raise
            except AutomationError as error:
                error.at_step(step.id, step_path=step.path, attempt=attempt, workflow=self.descriptor.name)
                if attempt >= max_attempts or effect not in {"read_only", "idempotent"} or not self._retry_match(retry, error): raise
                delay = self._retry_delay(retry, attempt); remaining = self._remaining(local_deadline)
                if remaining is not None and delay >= remaining: raise AutomationError("STEP.TIMEOUT", f"Step {step.id!r} timed out during retry", cause=error) from error
                self._event("step.retrying", step_id=step.id, attempt=attempt, delay=delay)
                if delay: time.sleep(delay)
        raise AssertionError("unreachable")

    def _resolve_action_contract(
        self, step: CompiledStep
    ) -> Mapping[str, Any]:
        uses = step.params["uses"]
        capability = uses.rsplit(".", 1)[0]
        plugin = self.plugins.get(capability)
        if plugin is None:
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"No plugin registered for {capability!r}",
                details={"uses": uses},
            )
        return self._action_contract(plugin, capability, uses)

    def _effective_action_effect(
        self, step: CompiledStep, *, contract: Mapping[str, Any] | None = None
    ) -> str:
        declared = thaw(step.params.get("effect", {})).get("class")
        if contract is None:
            contract = self._resolve_action_contract(step)
        provider = thaw(contract.get("effect", {})).get("default_class")
        return _max_effect(provider, declared)

    def _retry_match(self, retry: Mapping[str, Any], error: AutomationError) -> bool:
        if not error.retryable: return False
        on = retry.get("on")
        if not on: return True
        return any(fnmatch.fnmatchcase(error.code, pattern) for pattern in on.get("codes", ())) or error.category in on.get("categories", ())

    def _retry_delay(self, retry: Mapping[str, Any], attempt: int) -> float:
        backoff = retry.get("backoff") or {}; initial = _duration(backoff.get("initial_delay"), 0.0) or 0.0
        delay = initial if backoff.get("strategy", "fixed") == "fixed" else initial * float(backoff.get("multiplier", 2.0)) ** (attempt - 1)
        maximum = _duration(backoff.get("max_delay")); delay = min(delay, maximum) if maximum is not None else delay
        jitter = float(backoff.get("jitter", 0.0)); return delay * random.uniform(1 - jitter, 1 + jitter) if jitter else delay

    def _execute(
        self,
        step: CompiledStep,
        timeout: float | None,
        *,
        action_contract: Mapping[str, Any] | None = None,
    ) -> Any:
        if step.type == "action":
            return self._action(step, timeout, contract=action_contract)
        if step.type == "set":
            snapshot_context = dict(self.context)
            snapshot_context["vars"] = dict(self.variables)
            pending_values = {
                target.partition(".")[2]: self._evaluate(thaw(raw), snapshot_context)
                for target, raw in step.params["assign"].items()
            }
            for name, value in pending_values.items():
                definition = self.descriptor.variables[name]
                self._validate_schema(value, definition.get("schema"), "VARIABLE.INVALID", name)
            self.variables.update(pending_values)
            return dict(self.variables)
        if step.type == "if": self._run_steps(step.then_steps if bool(self._evaluate(thaw(step.params["condition"]))) else step.else_steps); return None
        if step.type == "switch":
            for case in step.cases:
                if bool(self._evaluate(thaw(case.when))): self._run_steps(case.steps); return None
            self._run_steps(step.default_steps); return None
        if step.type == "foreach":
            if int(step.params.get("concurrency", 1)) != 1:
                raise AutomationError(
                    "DESCRIPTOR.UNSUPPORTED_FEATURE",
                    "Concurrent foreach execution is not supported by this runtime",
                    category="descriptor",
                    details={"concurrency": step.params["concurrency"]},
                )
            items = self._evaluate(thaw(step.params["items"])); limit = int(step.params["max_items"])
            if isinstance(items, (str, bytes, Mapping)) or not isinstance(items, Sequence): raise AutomationError("EXPR.TYPE_MISMATCH", "foreach items must be an array")
            if len(items) > limit: raise AutomationError("LOOP.LIMIT_EXCEEDED", "foreach input exceeds max_items", details={"max_items": limit, "actual": len(items)})
            item_name, index_name = step.params["as"], step.params.get("index_as", "index"); old_item, old_index = self.context.get(item_name, MISSING), self.context.get(index_name, MISSING)
            try:
                for index, item in enumerate(items): self.context[item_name] = item; self.context[index_name] = index; self._run_steps(step.steps)
            finally: self._restore(item_name, old_item); self._restore(index_name, old_index)
            return None
        if step.type == "while":
            limit = int(step.params["max_iterations"])
            for _ in range(limit):
                if not bool(self._evaluate(thaw(step.params["condition"]))): return None
                self._run_steps(step.steps)
            if bool(self._evaluate(thaw(step.params["condition"]))): raise AutomationError("LOOP.LIMIT_EXCEEDED", "while condition remains true", details={"max_iterations": limit})
            return None
        if step.type == "block": self._run_steps(step.steps); return None
        if step.type == "script": return self._script(step, timeout)
        if step.type == "fail":
            raw = self._evaluate(thaw(step.params["error"])); raise AutomationError(raw["code"], raw["message"], category=raw.get("category"), retryable=raw.get("retryable", False), effect=raw.get("effect", "none"), details=raw.get("details"))
        if step.type == "return": raise _ReturnFlow(self._evaluate(thaw(step.params.get("value"))))
        raise AutomationError("DESCRIPTOR.UNSUPPORTED_FEATURE", f"Unsupported step type {step.type!r}")

    def _restore(self, name: str, old: Any) -> None:
        if old is MISSING: self.context.pop(name, None)
        else: self.context[name] = old

    def _action(
        self,
        step: CompiledStep,
        timeout: float | None,
        *,
        contract: Mapping[str, Any] | None = None,
    ) -> Any:
        uses = step.params["uses"]; capability = uses.rsplit(".", 1)[0]; plugin = self.plugins.get(capability)
        if plugin is None: raise AutomationError("CAPABILITY.MISSING", f"No plugin registered for {capability!r}", details={"uses": uses})
        if contract is None:
            contract = self._action_contract(plugin, capability, uses)
        self._enforce_action_policy(step, plugin, contract)
        pre = step.params.get("precondition")
        if pre and not bool(self._evaluate(thaw(pre["condition"]))): raise AutomationError("ACTION.PRECONDITION_FAILED", pre.get("message", "Action precondition failed"), phase="precondition")
        action_input = self._evaluate(thaw(step.params["with"]))
        self._validate_schema(action_input, contract.get("input_schema"), "ACTION.INPUT_INVALID", uses)
        try: result = plugin.invoke(uses, action_input, timeout=timeout)
        except PluginError as exc:
            declared_effect = thaw(step.params.get("effect", {})).get("class")
            provider_effect = thaw(contract.get("effect", {})).get("default_class")
            effective_effect = _max_effect(provider_effect, declared_effect)
            ambiguous = exc.code in {"PLUGIN.HOST_TIMEOUT", "PLUGIN.HOST_EOF", "PLUGIN.HOST_PROTOCOL_ERROR"}
            if exc.dispatched and effective_effect in {"non_idempotent", "contextual"} and ambiguous:
                raise AutomationError("ACTION.UNKNOWN_EFFECT", "Action outcome is unknown after dispatch", category="action", effect="unknown", details={"plugin_error": exc.to_dict()}, cause=exc) from exc
            if exc.code == "PLUGIN.HOST_TIMEOUT":
                raise AutomationError(
                    "ACTION.TIMEOUT",
                    "Action did not complete before its deadline",
                    category="action",
                    retryable=exc.retryable,
                    effect=(
                        "not_applied"
                        if not exc.dispatched or effective_effect in {"read_only", "idempotent"}
                        else "unknown"
                    ),
                    details={"plugin_error": exc.to_dict()},
                    cause=exc,
                ) from exc
            raise AutomationError(exc.code, exc.message, category="plugin", retryable=exc.retryable, details=exc.details, cause=exc) from exc
        self._validate_schema(result, contract.get("output_schema"), "ACTION.OUTPUT_INVALID", uses)
        self.context["steps"][step.id] = {"status": "running", "output": result}
        if step.params.get("postcondition"): self._postcondition(step.params["postcondition"])
        return result

    def _action_contract(
        self, plugin: ProcessPlugin, capability: str, uses: str
    ) -> Mapping[str, Any]:
        if plugin.manifest is None and type(plugin).invoke is not ProcessPlugin.invoke:
            return {}
        manifest = self._plugin_manifest(plugin)
        # Test doubles and trusted in-process adapters may deliberately omit a
        # manifest. Real process plugins are validated by ProcessPlugin.start.
        if not isinstance(manifest, Mapping):
            return {}
        metadata = manifest.get("metadata", {})
        if metadata.get("name") != capability:
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"Plugin {metadata.get('name')!r} does not provide {capability!r}",
                details={"uses": uses},
            )
        action_with_major = uses[len(capability) + 1 :]
        action_name, major_text = action_with_major.rsplit("@", 1)
        contract = manifest.get("actions", {}).get(action_name)
        if not isinstance(contract, Mapping):
            raise AutomationError(
                "CAPABILITY.MISSING",
                f"Capability {capability!r} does not provide action {action_name!r}",
                details={"uses": uses},
            )
        if contract.get("contract_major") != int(major_text):
            raise AutomationError(
                "CAPABILITY.VERSION_INCOMPATIBLE",
                f"Action contract major does not match {uses!r}",
                details={"uses": uses, "available_major": contract.get("contract_major")},
            )
        return contract

    def _plugin_manifest(
        self,
        plugin: ProcessPlugin,
        *,
        timeout_code: str = "ACTION.TIMEOUT",
    ) -> Mapping[str, Any]:
        remaining = self._remaining()
        if remaining == 0:
            raise AutomationError(
                timeout_code,
                "Deadline exceeded during capability handshake",
                category=(
                    "action" if timeout_code == "ACTION.TIMEOUT" else "workflow"
                ),
                phase="execute",
                effect=(
                    "not_applied" if timeout_code == "ACTION.TIMEOUT" else "none"
                ),
            )
        try:
            return plugin.start(timeout=remaining)
        except PluginError as exc:
            if exc.code == "PLUGIN.HOST_TIMEOUT" and self._remaining() == 0:
                raise AutomationError(
                    timeout_code,
                    "Deadline exceeded during capability handshake",
                    category=(
                        "action"
                        if timeout_code == "ACTION.TIMEOUT"
                        else "workflow"
                    ),
                    phase="execute",
                    retryable=exc.retryable,
                    effect=(
                        "not_applied"
                        if timeout_code == "ACTION.TIMEOUT"
                        else "none"
                    ),
                    cause=exc,
                ) from exc
            raise AutomationError(
                exc.code,
                exc.message,
                category="plugin",
                retryable=exc.retryable,
                details=exc.details,
                cause=exc,
            ) from exc

    def _enforce_action_policy(
        self,
        step: CompiledStep,
        plugin: ProcessPlugin,
        contract: Mapping[str, Any],
    ) -> None:
        declared = thaw(step.params.get("risk", {}))
        default = thaw(contract.get("risk", {}))
        risks = [risk for risk in (default, declared) if risk]
        manifest = plugin.manifest if isinstance(plugin.manifest, Mapping) else {}
        runtime = manifest.get("runtime", {}) if isinstance(manifest, Mapping) else {}
        supported_platforms = runtime.get("platforms") if isinstance(runtime, Mapping) else None
        if supported_platforms and _platform_name() not in supported_platforms:
            raise AutomationError(
                "CAPABILITY.PLATFORM_UNSUPPORTED",
                f"Capability is unavailable on platform {_platform_name()!r}",
                category="capability",
            )
        required_permissions = set(manifest.get("permissions", ()))
        required_permissions.update(contract.get("permissions", ()))
        declared_permissions = set(
            thaw(self.descriptor.requires).get("permissions", ())
        )
        undeclared_permissions = sorted(
            required_permissions - declared_permissions
        )
        if undeclared_permissions:
            raise AutomationError(
                "POLICY.DENIED",
                "Action permissions were not declared by the workflow",
                category="policy",
                details={"undeclared_permissions": undeclared_permissions},
            )
        missing_permissions = sorted(required_permissions - self.granted_permissions)
        if missing_permissions:
            raise AutomationError(
                "POLICY.DENIED",
                "Action permissions were not granted",
                category="policy",
                details={"missing_permissions": missing_permissions},
            )
        if not risks:
            return
        order = {"low": 0, "medium": 1, "high": 2, "critical": 3, "contextual": 4}
        policy = thaw(self.descriptor.policy)
        allowed = policy.get("allowed_risk", {})
        if allowed:
            categories = allowed.get("categories")
            denied_categories = [
                risk.get("category")
                for risk in risks
                if categories and risk.get("category") not in categories
            ]
            if denied_categories:
                raise AutomationError(
                    "POLICY.DENIED",
                    f"Risk category {denied_categories[0]!r} is not allowed",
                    category="policy",
                )
            maximum = allowed.get("max_level")
            highest = max(
                risks, key=lambda risk: order.get(risk.get("level"), 99)
            )
            if (
                maximum in order
                and highest.get("level") in order
                and order[highest["level"]] > order[maximum]
            ):
                raise AutomationError(
                    "POLICY.DENIED",
                    f"Risk level {highest['level']!r} exceeds {maximum!r}",
                    category="policy",
                )
        confirmation = policy.get("confirmation", {})
        required_for = confirmation.get("required_for", {}) if isinstance(confirmation, Mapping) else {}
        categories = set(required_for.get("categories", ()))
        minimum = required_for.get("min_level")
        requires_confirmation = any(
            (categories and risk.get("category") in categories)
            or (minimum in order and risk.get("level") in order and order[risk["level"]] >= order[minimum])
            for risk in risks
        )
        if requires_confirmation:
            raise AutomationError(
                "POLICY.CONFIRMATION_REQUIRED",
                "This action requires a bound confirmation token, which v0 cannot verify",
                category="policy",
            )

    def _postcondition(self, post: Mapping[str, Any]) -> None:
        timeout = _duration(post.get("timeout"), 0.0) or 0.0; interval = _duration(post.get("poll_interval"), .1) or .1; deadline = time.monotonic() + timeout
        parent_remaining = self._remaining()
        if parent_remaining is not None:
            deadline = min(deadline, time.monotonic() + parent_remaining)
        while True:
            if bool(self._evaluate(thaw(post["condition"]))): return
            if time.monotonic() >= deadline: raise AutomationError("ACTION.POSTCONDITION_FAILED", post.get("message", "Action postcondition failed"), phase="postcondition")
            time.sleep(min(interval, max(0, deadline - time.monotonic())))

    def _script(self, step: CompiledStep, timeout: float | None) -> Any:
        if not self.allow_scripts: raise AutomationError("SCRIPT.SANDBOX_DENIED", "Scripts are disabled; pass --allow-scripts", category="script")
        result = execute_python_script(
            self.descriptor,
            step,
            self._evaluate(thaw(step.params.get("inputs", {}))),
            timeout,
        )
        self._validate_schema(
            result, step.params["output_schema"], "SCRIPT.OUTPUT_INVALID", step.id
        )
        return result

    def _apply_handler(self, handler: ErrorHandler | None, error: AutomationError) -> AutomationError | None:
        if handler is None or not self._handler_matches(handler, error): return error
        previous = self.context.get(handler.as_name, MISSING); self.context[handler.as_name] = error.to_dict(); self.context["error"] = error.to_dict()
        try:
            self._run_steps(handler.steps)
            if handler.mode == "rethrow": return error
            value = self._evaluate(thaw(handler.output)) if handler.output is not MISSING else None
            if handler.mode == "return": raise _ReturnFlow(value)
            self._handler_output = value; return None
        finally: self._restore(handler.as_name, previous)

    def _handler_matches(self, handler: ErrorHandler, error: AutomationError) -> bool:
        return (not handler.match_codes or any(fnmatch.fnmatchcase(error.code, item) for item in handler.match_codes)) and (not handler.match_categories or error.category in handler.match_categories) and (not handler.match_effects or error.effect in handler.match_effects)


def _duration(value: Any, default: float | None = None) -> float | None:
    return parse_duration(value, default)


def _max_effect(provider: Any, declared: Any) -> str:
    order = {
        "read_only": 0,
        "idempotent": 1,
        "non_idempotent": 2,
        "contextual": 3,
    }
    values = [item for item in (provider, declared) if item in order]
    return max(values, key=lambda item: order[item]) if values else "contextual"


def run_descriptor(descriptor: WorkflowDescriptor, *, inputs: Mapping[str, Any] | None = None, plugins: Mapping[str, ProcessPlugin | Sequence[str] | str] | None = None, allow_scripts: bool = False, granted_permissions: Sequence[str] | None = None, event_sink: Callable[[Mapping[str, Any]], None] | None = None) -> RunResult:
    return WorkflowRunner(descriptor, plugins=plugins, allow_scripts=allow_scripts, granted_permissions=granted_permissions, event_sink=event_sink).run(inputs)


def _platform_name() -> str:
    if sys.platform.startswith("win"):
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return "linux"


def _semver(
    value: str,
) -> tuple[int, int, int, tuple[int | str, ...] | None]:
    if not isinstance(value, str):
        raise ValueError(f"invalid semantic version: {value!r}")
    match = re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
        r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
        r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?",
        value,
    )
    if not match:
        raise ValueError(f"invalid semantic version: {value!r}")
    major, minor, patch, prerelease_text = match.groups()
    prerelease: tuple[int | str, ...] | None = None
    if prerelease_text is not None:
        identifiers: list[int | str] = []
        for identifier in prerelease_text.split("."):
            if identifier.isdigit():
                if len(identifier) > 1 and identifier.startswith("0"):
                    raise ValueError(
                        f"invalid semantic version: {value!r}"
                    )
                identifiers.append(int(identifier))
            else:
                identifiers.append(identifier)
        prerelease = tuple(identifiers)
    return int(major), int(minor), int(patch), prerelease


def _compare_semver(
    left: tuple[int, int, int, tuple[int | str, ...] | None],
    right: tuple[int, int, int, tuple[int | str, ...] | None],
) -> int:
    if left[:3] != right[:3]:
        return -1 if left[:3] < right[:3] else 1
    left_pre, right_pre = left[3], right[3]
    if left_pre is None or right_pre is None:
        if left_pre is right_pre:
            return 0
        return 1 if left_pre is None else -1
    for left_item, right_item in zip(left_pre, right_pre):
        if left_item == right_item:
            continue
        if isinstance(left_item, int) and isinstance(right_item, str):
            return -1
        if isinstance(left_item, str) and isinstance(right_item, int):
            return 1
        return -1 if left_item < right_item else 1
    if len(left_pre) == len(right_pre):
        return 0
    return -1 if len(left_pre) < len(right_pre) else 1


def _version_matches(version: str, constraint: str) -> bool:
    try:
        actual = _semver(version)
        tokens = constraint.split()
        comparators: list[
            tuple[
                str,
                tuple[int, int, int, tuple[int | str, ...] | None],
            ]
        ] = []
        for token in tokens:
            if token.startswith("^"):
                floor = _semver(token[1:])
                if floor[0] != 0:
                    ceiling = (floor[0] + 1, 0, 0, None)
                elif floor[1] != 0:
                    ceiling = (0, floor[1] + 1, 0, None)
                else:
                    ceiling = (0, 0, floor[2] + 1, None)
                comparators.extend(((">=", floor), ("<", ceiling)))
                continue
            match = re.fullmatch(r"(>=|<=|>|<|=)?(.+)", token)
            if not match:
                return False
            operator, wanted_text = match.groups()
            wanted = _semver(wanted_text)
            comparators.append((operator or "=", wanted))
        if not comparators:
            return False
        # SemVer ranges do not implicitly opt into prerelease providers.  A
        # comparator must name a prerelease on the same major/minor/patch.
        if actual[3] is not None and not any(
            wanted[3] is not None and wanted[:3] == actual[:3]
            for _, wanted in comparators
        ):
            return False
        for operator, wanted in comparators:
            comparison = _compare_semver(actual, wanted)
            if operator == ">=" and comparison < 0:
                return False
            if operator == "<=" and comparison > 0:
                return False
            if operator == ">" and comparison <= 0:
                return False
            if operator == "<" and comparison >= 0:
                return False
            if operator == "=" and comparison != 0:
                return False
        return True
    except (TypeError, ValueError):
        return False


__all__ = ["WorkflowRunner", "run_descriptor", "RunResult"]
