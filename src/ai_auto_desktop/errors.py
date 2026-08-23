"""Structured errors shared by the compiler, runtime, and CLI."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Iterable, Mapping


def _category(code: str) -> str:
    return code.partition(".")[0].lower() if code else "runtime"


class AutomationError(Exception):
    """A stable, machine-readable workflow error."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        category: str | None = None,
        phase: str | None = None,
        retryable: bool = False,
        effect: str = "none",
        step_id: str | None = None,
        step_path: str | None = None,
        attempt: int | None = None,
        workflow: str | None = None,
        details: Mapping[str, Any] | None = None,
        cause: BaseException | Mapping[str, Any] | None = None,
        suppressed: Iterable["AutomationError" | Mapping[str, Any]] | None = None,
    ) -> None:
        super().__init__(message)
        self.code = str(code)
        self.message = str(message)
        self.category = category or _category(self.code)
        self.phase = phase
        self.retryable = bool(retryable)
        self.effect = effect
        self.step_id = step_id
        self.step_path = step_path or step_id
        self.attempt = attempt
        self.workflow = workflow
        self.details = dict(details or {})
        self.cause = cause
        self.suppressed = list(suppressed or [])

    def add_suppressed(self, error: "AutomationError") -> None:
        self.suppressed.append(error)

    def at_step(
        self,
        step_id: str,
        *,
        step_path: str | None = None,
        attempt: int | None = None,
        workflow: str | None = None,
    ) -> "AutomationError":
        if self.step_id is None:
            self.step_id = step_id
        if self.step_path is None:
            self.step_path = step_path or step_id
        if self.attempt is None:
            self.attempt = attempt
        if self.workflow is None:
            self.workflow = workflow
        return self

    def to_dict(self) -> dict[str, Any]:
        location: dict[str, Any] = {}
        if self.workflow is not None:
            location["workflow"] = self.workflow
        if self.step_path is not None:
            location["step_path"] = self.step_path
        if self.step_id is not None:
            location["step_id"] = self.step_id
        if self.attempt is not None:
            location["attempt"] = self.attempt

        if isinstance(self.cause, AutomationError):
            cause: Any = self.cause.to_dict()
        elif isinstance(self.cause, Mapping):
            cause = dict(self.cause)
        elif self.cause is not None:
            cause = {"type": type(self.cause).__name__, "message": str(self.cause)}
        else:
            cause = None

        suppressed: list[dict[str, Any]] = []
        for item in self.suppressed:
            suppressed.append(item.to_dict() if isinstance(item, AutomationError) else dict(item))

        result: dict[str, Any] = {
            "schema_version": "1",
            "code": self.code,
            "category": self.category,
            "message": self.message,
            "retryable": self.retryable,
            "effect": self.effect,
            "details": dict(self.details),
            "cause": cause,
            "suppressed": suppressed,
        }
        if self.phase is not None:
            result["phase"] = self.phase
        if location:
            result["location"] = location
        return result


@dataclass(frozen=True, slots=True)
class DescriptorIssue:
    path: str
    message: str
    code: str = "invalid"

    def to_dict(self) -> dict[str, str]:
        return {"path": self.path, "message": self.message, "code": self.code}


class DescriptorError(AutomationError):
    """Raised when a descriptor cannot be compiled."""

    def __init__(
        self,
        message: str = "Workflow descriptor is invalid",
        *,
        issues: Iterable[DescriptorIssue | Mapping[str, str]] | None = None,
        code: str = "DESCRIPTOR.INVALID",
        details: Mapping[str, Any] | None = None,
        cause: BaseException | None = None,
    ) -> None:
        normalized = [
            issue.to_dict() if isinstance(issue, DescriptorIssue) else dict(issue)
            for issue in (issues or [])
        ]
        merged = dict(details or {})
        merged["issues"] = normalized
        super().__init__(
            code,
            message,
            category="descriptor",
            phase="compile",
            details=merged,
            cause=cause,
        )
        self.issues = normalized


def ensure_automation_error(
    error: BaseException,
    *,
    code: str = "RUNTIME.INTERNAL",
    message: str | None = None,
) -> AutomationError:
    if isinstance(error, AutomationError):
        return error
    return AutomationError(code, message or str(error) or type(error).__name__, cause=error)

