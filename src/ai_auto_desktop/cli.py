"""Command line interface with JSON-only standard output."""

from __future__ import annotations

import argparse
import json
import shlex
import sys
from pathlib import Path
from typing import Any

from .compiler import load_descriptor
from .errors import AutomationError, ensure_automation_error
from .runtime import WorkflowRunner


class _Parser(argparse.ArgumentParser):
    def error(self, message: str) -> None:
        raise AutomationError("CLI.INVALID_ARGUMENT", message, category="cli")


def _assignment(value: str, label: str) -> tuple[str, str]:
    if "=" not in value:
        raise AutomationError("CLI.INVALID_ARGUMENT", f"{label} must use NAME=VALUE", category="cli")
    name, payload = value.split("=", 1)
    if not name:
        raise AutomationError("CLI.INVALID_ARGUMENT", f"{label} name is empty", category="cli")
    return name, payload


def build_parser() -> argparse.ArgumentParser:
    parser = _Parser(prog="ai-auto-desktop")
    parser.add_argument("--version", action="version", version="%(prog)s 0.1.0")
    commands = parser.add_subparsers(dest="command", required=True)
    validate = commands.add_parser("validate")
    validate.add_argument("file", type=Path)
    run = commands.add_parser("run")
    run.add_argument("file", type=Path)
    run.add_argument("--input", action="append", default=[], metavar="NAME=JSON")
    run.add_argument("--plugin", action="append", default=[], metavar="NAME=COMMAND")
    run.add_argument("--allow-scripts", action="store_true")
    return parser


def _inputs(values: list[str]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for raw in values:
        name, payload = _assignment(raw, "--input")
        if name in result:
            raise AutomationError("CLI.INVALID_ARGUMENT", f"input {name!r} is repeated", category="cli")
        try:
            result[name] = json.loads(payload)
        except json.JSONDecodeError as exc:
            raise AutomationError("CLI.INVALID_JSON", f"input {name!r} must be JSON: {exc}", category="cli") from exc
    return result


def _plugins(values: list[str]) -> dict[str, list[str]]:
    result: dict[str, list[str]] = {}
    for raw in values:
        name, command = _assignment(raw, "--plugin")
        if name in result:
            raise AutomationError("CLI.INVALID_ARGUMENT", f"plugin {name!r} is repeated", category="cli")
        try:
            argv = shlex.split(command)
        except ValueError as exc:
            raise AutomationError(
                "CLI.INVALID_ARGUMENT",
                f"plugin {name!r} command is invalid: {exc}",
                category="cli",
            ) from exc
        if not argv:
            raise AutomationError("CLI.INVALID_ARGUMENT", f"plugin {name!r} command is empty", category="cli")
        result[name] = argv
    return result


def _emit(value: Any) -> None:
    sys.stdout.write(json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n")


def main(argv: list[str] | None = None) -> int:
    try:
        options = build_parser().parse_args(argv)
        descriptor = load_descriptor(options.file)
        if options.command == "validate":
            _emit({"status": "valid", "workflow": descriptor.name, "apiVersion": descriptor.api_version, "steps": len(descriptor.all_steps())})
            return 0
        result = WorkflowRunner(descriptor, plugins=_plugins(options.plugin), allow_scripts=options.allow_scripts).run(_inputs(options.input))
        _emit(result.to_dict())
        return 0 if result.ok else 1
    except AutomationError as exc:
        _emit({"status": "error", "error": exc.to_dict()})
        return 2
    except KeyboardInterrupt:
        error = AutomationError("WORKFLOW.CANCELLED", "Interrupted", category="workflow")
        _emit({"status": "cancelled", "error": error.to_dict()})
        return 130
    except Exception as exc:
        error = ensure_automation_error(exc)
        _emit({"status": "error", "error": error.to_dict()})
        return 2


__all__ = ["build_parser", "main"]

