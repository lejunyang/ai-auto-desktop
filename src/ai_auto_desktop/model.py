"""Immutable descriptor model and public run result types."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from types import MappingProxyType
from typing import Any, Mapping

from .artifacts import ArtifactError, ArtifactHandle, ArtifactRef, ArtifactStore
from .errors import AutomationError


MISSING = object()


def freeze(value: Any) -> Any:
    if isinstance(value, Mapping):
        return MappingProxyType({str(key): freeze(item) for key, item in value.items()})
    if isinstance(value, (list, tuple)):
        return tuple(freeze(item) for item in value)
    if isinstance(value, (set, frozenset)):
        return frozenset(freeze(item) for item in value)
    return value


def thaw(value: Any) -> Any:
    if isinstance(value, Mapping):
        return {key: thaw(item) for key, item in value.items()}
    if isinstance(value, tuple):
        return [thaw(item) for item in value]
    if isinstance(value, frozenset):
        return [thaw(item) for item in value]
    return value


@dataclass(frozen=True, slots=True)
class ErrorHandler:
    steps: tuple["CompiledStep", ...]
    match_codes: tuple[str, ...] = ("*",)
    match_categories: tuple[str, ...] = ()
    match_effects: tuple[str, ...] = ()
    as_name: str = "error"
    mode: str = "rethrow"
    output: Any = MISSING


@dataclass(frozen=True, slots=True)
class SwitchCase:
    steps: tuple["CompiledStep", ...]
    when: Any = MISSING
    value: Any = MISSING


@dataclass(frozen=True, slots=True)
class CompiledStep:
    id: str
    type: str
    path: str
    depends_on: tuple[str, ...] = ()
    params: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    steps: tuple["CompiledStep", ...] = ()
    then_steps: tuple["CompiledStep", ...] = ()
    else_steps: tuple["CompiledStep", ...] = ()
    cases: tuple[SwitchCase, ...] = ()
    default_steps: tuple["CompiledStep", ...] = ()
    on_error: ErrorHandler | None = None
    finally_steps: tuple["CompiledStep", ...] = ()

    def get(self, key: str, default: Any = None) -> Any:
        return self.params.get(key, default)


@dataclass(frozen=True, slots=True)
class WorkflowDescriptor:
    api_version: str
    name: str
    steps: tuple[CompiledStep, ...]
    source: Path | None = None
    description: str | None = None
    metadata: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    inputs: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    variables: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    outputs: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    requires: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    defaults: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    budgets: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    policy: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    extensions: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}))
    on_error: ErrorHandler | None = None
    finally_steps: tuple[CompiledStep, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: MappingProxyType({}), repr=False)

    @property
    def version(self) -> str:
        return self.api_version

    def all_steps(self) -> tuple[CompiledStep, ...]:
        found: list[CompiledStep] = []

        def visit(items: tuple[CompiledStep, ...]) -> None:
            for step in items:
                found.append(step)
                visit(step.steps)
                visit(step.then_steps)
                visit(step.else_steps)
                for case in step.cases:
                    visit(case.steps)
                visit(step.default_steps)
                if step.on_error:
                    visit(step.on_error.steps)
                visit(step.finally_steps)

        visit(self.steps)
        if self.on_error:
            visit(self.on_error.steps)
        visit(self.finally_steps)
        return tuple(found)


@dataclass(slots=True)
class RunResult:
    status: str
    output: Any = None
    variables: dict[str, Any] = field(default_factory=dict)
    error: AutomationError | None = None
    events: list[dict[str, Any]] = field(default_factory=list)
    steps: dict[str, dict[str, Any]] = field(default_factory=dict)
    _artifact_store: Any = field(default=None, repr=False, compare=False)
    _owns_artifact_store: bool = field(default=False, repr=False, compare=False)

    @property
    def ok(self) -> bool:
        return self.status == "succeeded"

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status,
            "output": thaw(self.output),
            "variables": thaw(self.variables),
            "steps": thaw(self.steps),
            "events": thaw(self.events),
            "error": self.error.to_dict() if self.error is not None else None,
        }

    def close(self) -> None:
        store = self._artifact_store
        self._artifact_store = None
        if store is not None and self._owns_artifact_store:
            store.cleanup()
        self._owns_artifact_store = False

    def resolve_artifact(
        self, reference: ArtifactRef | Mapping[str, Any]
    ) -> ArtifactHandle:
        """Resolve a returned ref while this run result still owns its scope."""

        store = self._artifact_store
        if not isinstance(store, ArtifactStore):
            raise ArtifactError(
                "ARTIFACT.STORE_UNAVAILABLE",
                "This run result has no live artifact store.",
            )
        return store.resolve(reference)

    def __enter__(self) -> "RunResult":
        return self

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass

