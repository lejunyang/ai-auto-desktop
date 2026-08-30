"""Command line interface with JSON-only standard output."""

from __future__ import annotations

import argparse
import json
import os
import shlex
import sys
from pathlib import Path
from typing import Any, Mapping

from .compiler import load_descriptor
from .durable import DurableExecutor
from .errors import AutomationError, ensure_automation_error
from .journal import JournalStore
from .probe import probe_capabilities
from .run_service import RunService
from .runtime import WorkflowRunner


class _Parser(argparse.ArgumentParser):
    def error(self, message: str) -> None:
        raise AutomationError("CLI.INVALID_ARGUMENT", message, category="cli")

    def exit(self, status: int = 0, message: str | None = None) -> None:
        if status:
            raise AutomationError(
                "CLI.INVALID_ARGUMENT",
                (message or "command line parsing failed").strip(),
                category="cli",
            )
        raise _ParserExit(
            {
                "status": "help",
                "program": self.prog,
                "text": (message or self.format_help()).rstrip(),
            }
        )

    def print_help(self, file: Any = None) -> None:
        raise _ParserExit(
            {
                "status": "help",
                "program": self.prog,
                "text": self.format_help().rstrip(),
            }
        )


class _ParserExit(Exception):
    def __init__(self, payload: Mapping[str, Any]) -> None:
        super().__init__(str(payload.get("status", "help")))
        self.payload = dict(payload)


def _split_command(command: str) -> list[str]:
    """Split a plugin command into argv.

    POSIX splitting is wrong on Windows: it treats the path separator as an
    escape, so ``plugins\\windows_uia\\run.cmd`` collapses into a single
    meaningless token and any interpreter path containing spaces is torn apart.
    Measured on Windows, that made every documented invocation of the UIA driver
    fail before the plugin was ever launched.

    Forward slashes are not a workaround: cmd.exe resolves a ``.cmd`` only when
    given backslashes, so the two requirements are mutually exclusive under
    POSIX rules.  Windows therefore uses non-POSIX splitting, which keeps
    separators intact, and the surrounding quotes it preserves are stripped
    because CreateProcess would otherwise treat them as part of the filename.
    """

    if os.name != "nt":
        return shlex.split(command)
    parts = shlex.split(command, posix=False)
    return [part[1:-1] if len(part) > 1 and part[0] == part[-1] == '"' else part
            for part in parts]


def _assignment(value: str, label: str) -> tuple[str, str]:
    if "=" not in value:
        raise AutomationError("CLI.INVALID_ARGUMENT", f"{label} must use NAME=VALUE", category="cli")
    name, payload = value.split("=", 1)
    if not name:
        raise AutomationError("CLI.INVALID_ARGUMENT", f"{label} name is empty", category="cli")
    return name, payload


def build_parser() -> argparse.ArgumentParser:
    parser = _Parser(prog="ai-auto-desktop")
    parser.add_argument("--version", action="store_true")
    commands = parser.add_subparsers(dest="command")
    commands.add_parser("probe")
    validate = commands.add_parser("validate")
    validate.add_argument("file", type=Path)

    edit = commands.add_parser("edit")
    edit.add_argument("file", type=Path)
    edit.add_argument("--no-browser", action="store_true")
    edit.add_argument("--output", type=Path)
    run = commands.add_parser("run")
    run.add_argument("file", type=Path)
    run.add_argument("--input", action="append", default=[], metavar="NAME=JSON")
    run.add_argument("--plugin", action="append", default=[], metavar="NAME=COMMAND")
    run.add_argument("--permission", action="append", default=[])
    run.add_argument("--allow-scripts", action="store_true")

    start = commands.add_parser("start")
    start.add_argument("file", type=Path)
    start.add_argument("--journal", required=True, type=Path)
    start.add_argument("--run-id")
    _execution_arguments(start)

    resume = commands.add_parser("resume")
    resume.add_argument("run_id")
    resume.add_argument("file", type=Path)
    resume.add_argument("--journal", required=True, type=Path)
    _execution_arguments(resume)

    for name in ("status", "pause", "cancel"):
        command = commands.add_parser(name)
        command.add_argument("run_id")
        command.add_argument("--journal", required=True, type=Path)
    listing = commands.add_parser("list")
    listing.add_argument("--journal", required=True, type=Path)
    listing.add_argument("--status")
    listing.add_argument("--limit", type=int, default=100)
    listing.add_argument("--offset", type=int, default=0)
    events = commands.add_parser("events")
    events.add_argument("run_id")
    events.add_argument("--journal", required=True, type=Path)
    events.add_argument("--after-seq", type=int, default=0)
    events.add_argument("--limit", type=int, default=1_000)
    return parser


def _execution_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--input", action="append", default=[], metavar="NAME=JSON")
    parser.add_argument("--plugin", action="append", default=[], metavar="NAME=COMMAND")
    parser.add_argument("--permission", action="append", default=[])
    parser.add_argument("--allow-scripts", action="store_true")
    parser.add_argument(
        "--durable-actions", choices=("deny", "read-only"),
        default="deny",
    )
    parser.add_argument("--owner-id")
    parser.add_argument("--lease-ttl", type=float, default=30.0)


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
            argv = _split_command(command)
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


def _edit_command(options: Any) -> int:
    """Serve the editor UI until interrupted, then emit the recording.

    The result goes to stdout (or --output) rather than overwriting the input:
    a capture cannot be cheaply retaken, so an editing session must not be able
    to destroy the thing it was editing.
    """

    import json
    import os
    import threading
    import webbrowser

    from .editor_page import PAGE
    from .editor_server import serve

    raw = options.file.read_text(encoding="utf-8")
    if options.file.suffix in {".yaml", ".yml"}:
        import yaml

        recording = yaml.safe_load(raw)
    else:
        recording = json.loads(raw)

    destination = options.output

    def save(edited: dict) -> None:
        """Write the recording atomically after each change.

        Not at exit: measured, a killed process on Windows runs no `finally`
        and receives no KeyboardInterrupt, so an exit-time write can simply
        never happen.  Written via a temporary file and replaced, so a process
        killed mid-write cannot leave a truncated recording behind.
        """

        if destination is None:
            return
        temporary = destination.with_suffix(destination.suffix + ".tmp")
        temporary.write_text(
            json.dumps(edited, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8")
        os.replace(temporary, destination)

    server = serve(recording, PAGE, on_change=save)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    # The URL goes to stderr so that stdout stays a clean recording document
    # and `... edit x.yaml > y.yaml` still produces a usable file.
    print(f"editor: {server.url}", file=sys.stderr, flush=True)
    if not options.no_browser:
        webbrowser.open(server.url)

    try:
        thread.join()
    except KeyboardInterrupt:
        pass
    finally:
        server.shutdown()
        server.server_close()

    edited = server.session.recording
    payload = json.dumps(edited, ensure_ascii=False, indent=2)
    if options.output:
        # Already written by the autosave hook; repeated here so a clean exit
        # still produces a complete file even if no edit was ever made.
        save(edited)
    else:
        sys.stdout.write(payload + "\n")
    return 0


def main(argv: list[str] | None = None) -> int:
    try:
        options = build_parser().parse_args(argv)
        if options.version:
            _emit({"status": "version", "program": "ai-auto-desktop", "version": "0.1.0"})
            return 0
        if options.command is None:
            raise AutomationError(
                "CLI.INVALID_ARGUMENT",
                "a command is required",
                category="cli",
            )
        if options.command == "probe":
            _emit(probe_capabilities().to_dict())
            return 0
        if options.command == "edit":
            return _edit_command(options)
        if options.command in {"status", "list", "events", "pause", "cancel"}:
            with JournalStore(options.journal) as journal:
                service = RunService(journal)
                if options.command == "status":
                    payload = service.get(options.run_id).to_dict()
                elif options.command == "list":
                    payload = {
                        "runs": [
                            run.to_dict()
                            for run in service.list(
                                status=options.status,
                                limit=options.limit,
                                offset=options.offset,
                            )
                        ]
                    }
                elif options.command == "events":
                    items = service.events(
                        options.run_id,
                        after_seq=options.after_seq,
                        limit=options.limit,
                    )
                    payload = {
                        "events": [event.to_dict() for event in items],
                        "nextAfterSeq": (
                            items[-1].seq if items else options.after_seq
                        ),
                    }
                elif options.command == "pause":
                    payload = service.request_pause(options.run_id).to_dict()
                else:
                    payload = service.request_cancel(options.run_id).to_dict()
            _emit(payload)
            return 0
        descriptor = load_descriptor(options.file)
        if options.command == "validate":
            _emit({"status": "valid", "workflow": descriptor.name, "apiVersion": descriptor.api_version, "steps": len(descriptor.all_steps())})
            return 0
        if options.command in {"start", "resume"}:
            with JournalStore(options.journal) as journal:
                executor = DurableExecutor(
                    journal, owner_id=options.owner_id,
                    lease_ttl_seconds=options.lease_ttl,
                    durable_action_mode=options.durable_actions,
                )
                common = {
                    "plugins": _plugins(options.plugin),
                    "allow_scripts": options.allow_scripts,
                    "granted_permissions": options.permission,
                }
                if options.command == "start":
                    outcome = executor.start(
                        descriptor, inputs=_inputs(options.input),
                        run_id=options.run_id, **common,
                    )
                else:
                    if options.input:
                        raise AutomationError(
                            "CLI.INVALID_ARGUMENT",
                            "resume uses the inputs persisted with the run",
                            category="cli",
                        )
                    outcome = executor.resume(
                        options.run_id, descriptor, request_run=True, **common
                    )
            _emit(outcome.to_dict())
            return 0 if outcome.run.status.value in {"succeeded", "paused"} else 1
        result = WorkflowRunner(
            descriptor,
            plugins=_plugins(options.plugin),
            allow_scripts=options.allow_scripts,
            granted_permissions=options.permission,
        ).run(_inputs(options.input))
        _emit(result.to_dict())
        return 0 if result.ok else 1
    except _ParserExit as exc:
        _emit(exc.payload)
        return 0
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

