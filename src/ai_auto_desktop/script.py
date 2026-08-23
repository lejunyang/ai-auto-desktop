"""Helpers for explicitly enabled, process-isolated Python scripts."""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from .errors import AutomationError
from .model import CompiledStep, WorkflowDescriptor


def validate_script_policy(step: CompiledStep) -> None:
    if step.type != "script":
        raise ValueError("step must be a script step")
    if step.params.get("runtime") != "python":
        raise AutomationError("DESCRIPTOR.UNSUPPORTED_FEATURE", "v0 only executes Python scripts", category="script")
    if step.params.get("capabilities"):
        raise AutomationError("SCRIPT.SANDBOX_DENIED", "v0 cannot grant script capabilities", category="script")
    sandbox: Mapping[str, Any] = step.params.get("sandbox", {})
    for boundary in ("network", "filesystem", "environment"):
        config = sandbox.get(boundary, {})
        if isinstance(config, Mapping) and config.get("mode", "deny") != "deny":
            raise AutomationError("SCRIPT.SANDBOX_DENIED", f"v0 cannot grant script {boundary}", category="script")


def resolve_entrypoint(descriptor: WorkflowDescriptor, entrypoint: str) -> str:
    from pathlib import Path
    path = Path(entrypoint)
    if not path.is_absolute() and descriptor.source is not None:
        path = descriptor.source.parent / path
    return str(path.resolve())


__all__ = ["resolve_entrypoint", "validate_script_policy"]

