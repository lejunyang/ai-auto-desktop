"""Subprocess-backed NDJSON plugin host.

The module intentionally depends only on the Python standard library so it can
sit at the bottom of the package's dependency graph.  A plugin is an ordinary
process which reads one JSON object per line from stdin and writes one JSON
object per line to stdout.
"""

from __future__ import annotations

from collections import deque
from collections.abc import Mapping
from dataclasses import dataclass
from importlib import resources
import json
import math
import os
from pathlib import Path
import queue
import signal
import subprocess
import threading
import time
from types import TracebackType
from typing import Any, BinaryIO
import uuid

try:
    import jsonschema
except ImportError:  # pragma: no cover
    jsonschema = None


DEFAULT_TIMEOUT = 30.0

# A bounded queue prevents an ill-behaved plugin from making the host retain an
# unlimited number of unsolicited stdout messages.  Individual messages are
# bounded as well.
_STDOUT_QUEUE_SIZE = 128
_MAX_STDOUT_LINE_BYTES = 8 * 1024 * 1024
_MAX_STDERR_BYTES = 64 * 1024
_STDERR_CHUNK_BYTES = 4096
_MANIFEST_PROBE_SECONDS = 0.10
_TERMINATE_GRACE_SECONDS = 0.50
_MANIFEST_SCHEMA_RESOURCE = (
    "schemas",
    "capabilities",
    "v1alpha1",
    "capability-manifest.schema.json",
)


def _json_pointer_tokens(pointer: str) -> tuple[str, ...]:
    """Decode the RFC 6901 tokens of a schema-validated JSON pointer."""

    return tuple(
        raw_token.replace("~1", "/").replace("~0", "~")
        for raw_token in pointer.split("/")[1:]
    )


class PluginError(RuntimeError):
    """A structured error raised by a process plugin or its host.

    ``details["dispatched"]`` is true when the corresponding request was
    successfully written and flushed.  Callers can use that fact to avoid
    blindly retrying non-idempotent actions after an ambiguous timeout or EOF.
    """

    def __init__(
        self,
        code: str,
        message: str,
        *,
        details: Mapping[str, Any] | None = None,
        retryable: bool = False,
    ) -> None:
        super().__init__(message)
        self.code = str(code)
        self.message = str(message)
        self.details: dict[str, Any] = dict(details or {})
        self.retryable = bool(retryable)

    @property
    def dispatched(self) -> bool:
        """Whether the request reached a successful stdin flush boundary."""

        return self.details.get("dispatched") is True

    def to_dict(self) -> dict[str, Any]:
        """Return a JSON-compatible representation of the error."""

        return {
            "code": self.code,
            "message": self.message,
            "details": dict(self.details),
            "retryable": self.retryable,
        }

    def __str__(self) -> str:
        return f"{self.code}: {self.message}"


@dataclass(frozen=True, slots=True)
class _StreamEvent:
    kind: str
    value: str | None = None


class ProcessPlugin:
    """Host an NDJSON plugin in an isolated subprocess session.

    Args:
        command: Executable and arguments.  The command is never run through a
            shell.
        cwd: Optional child working directory.
        env: Optional complete child environment, with the same semantics as
            :class:`subprocess.Popen`.
        timeout: Default manifest and invocation timeout in seconds.
        name: Human-readable name used in error messages.

    ``invoke`` calls are serialized.  This makes response pairing deterministic
    even for plugins which process stdin synchronously, while still permitting
    callers from multiple threads.
    """

    def __init__(
        self,
        command: list[str],
        cwd: str | os.PathLike[str] | None = None,
        env: Mapping[str, str] | None = None,
        timeout: float = DEFAULT_TIMEOUT,
        name: str | None = None,
    ) -> None:
        if not isinstance(command, list) or not command:
            raise ValueError("command must be a non-empty list of strings")
        if not all(isinstance(part, str) and part for part in command):
            raise ValueError("command must contain only non-empty strings")

        self.command = list(command)
        self.cwd = os.fspath(cwd) if cwd is not None else None
        self.env = dict(env) if env is not None else None
        self.timeout = _validate_timeout(timeout, "timeout")
        self.name = name or Path(command[0]).name or command[0]

        self.manifest: dict[str, Any] | None = None
        self._process: subprocess.Popen[bytes] | None = None
        self._stdout_events: queue.Queue[_StreamEvent] = queue.Queue(
            maxsize=_STDOUT_QUEUE_SIZE
        )
        self._stderr_chunks: deque[str] = deque()
        self._stderr_size = 0
        self._stderr_lock = threading.Lock()
        self._lifecycle_lock = threading.RLock()
        self._request_lock = threading.Lock()
        self._reader_stop = threading.Event()
        self._stdout_thread: threading.Thread | None = None
        self._stderr_thread: threading.Thread | None = None
        self._closed = False
        # If a proactive manifest races with our fallback manifest request, its
        # eventual response remains a paired but ignorable transport message.
        self._discard_response_ids: set[str] = set()

    @property
    def pid(self) -> int | None:
        process = self._process
        return process.pid if process is not None else None

    @property
    def started(self) -> bool:
        return self._process is not None and self.manifest is not None

    @property
    def closed(self) -> bool:
        return self._closed

    @property
    def stderr(self) -> str:
        """Return the bounded tail of stderr captured so far."""

        with self._stderr_lock:
            return "".join(self._stderr_chunks)

    def start(self, timeout: float | None = None) -> dict[str, Any]:
        """Start the plugin within the optional caller-supplied budget."""

        effective_timeout = (
            self.timeout
            if timeout is None
            else min(self.timeout, _validate_timeout(timeout, "start timeout"))
        )
        deadline = time.monotonic() + effective_timeout
        with self._request_lock:
            return self._start_locked(deadline=deadline)

    def _start_locked(self, *, deadline: float | None = None) -> dict[str, Any]:
        with self._lifecycle_lock:
            if self._closed:
                raise PluginError(
                    "PLUGIN.CLOSED",
                    f"plugin {self.name!r} is closed",
                    details={"dispatched": False},
                )
            if self.manifest is not None:
                return self.manifest
            if deadline is not None and time.monotonic() >= deadline:
                raise self._host_error(
                    "PLUGIN.HOST_TIMEOUT",
                    f"plugin {self.name!r} did not respond before the deadline",
                    retryable=True,
                )
            if self._process is not None:
                # A process without a manifest can only exist while this method
                # holds _request_lock, or after a fatal startup failure.
                raise PluginError(
                    "PLUGIN.INVALID_STATE",
                    f"plugin {self.name!r} has not completed startup",
                )

            popen_options: dict[str, Any] = {
                "stdin": subprocess.PIPE,
                "stdout": subprocess.PIPE,
                "stderr": subprocess.PIPE,
                "cwd": self.cwd,
                "env": self.env,
                "shell": False,
                "bufsize": 0,
            }
            if os.name == "posix":
                popen_options["start_new_session"] = True
            elif os.name == "nt":
                popen_options["creationflags"] = getattr(
                    subprocess, "CREATE_NEW_PROCESS_GROUP", 0
                )

            try:
                process = subprocess.Popen(self.command, **popen_options)
            except (OSError, ValueError) as exc:
                self._closed = True
                raise PluginError(
                    "PLUGIN.START_FAILED",
                    f"could not start plugin {self.name!r}: {exc}",
                    details={"exception_type": type(exc).__name__},
                    retryable=isinstance(exc, OSError),
                ) from exc

            self._process = process
            self._start_readers(process)

        startup_deadline = time.monotonic() + self.timeout
        if deadline is not None:
            startup_deadline = min(startup_deadline, deadline)
        # Always reserve most of a short startup timeout for the request-based
        # handshake.  A fixed probe duration would consume the entire budget
        # when callers deliberately configure millisecond-scale timeouts.
        startup_remaining = max(0.0, startup_deadline - time.monotonic())
        probe_seconds = min(_MANIFEST_PROBE_SECONDS, startup_remaining / 10.0)
        probe_deadline = min(
            startup_deadline, time.monotonic() + probe_seconds
        )
        dispatched = False
        try:
            message = self._read_message(probe_deadline, allow_timeout=True)
            if message is not None:
                manifest = self._manifest_from_message(
                    message, expected_id=None, dispatched=False
                )
                self.manifest = manifest
                return manifest
            if time.monotonic() >= startup_deadline:
                raise self._host_error(
                    "PLUGIN.HOST_TIMEOUT",
                    f"plugin {self.name!r} did not respond before the deadline",
                    retryable=True,
                )

            request_id = self._new_request_id()
            dispatched = self._write_request(
                {"type": "manifest", "id": request_id}
            )
            message = self._read_message(startup_deadline)
            assert message is not None

            # A late proactive manifest is valid.  Remember the request id so
            # its later response can be paired and discarded without poisoning
            # the first invocation.
            if "id" not in message:
                manifest = self._manifest_from_message(
                    message, expected_id=None, dispatched=dispatched
                )
                self._discard_response_ids.add(request_id)
            else:
                manifest = self._manifest_from_message(
                    message, expected_id=request_id, dispatched=dispatched
                )
            self.manifest = manifest
            return manifest
        except PluginError as exc:
            if dispatched:
                exc = _with_dispatched(exc)
            self._abort(exc)
            raise AssertionError("unreachable")

    def invoke(
        self, action: str, args: Any, timeout: float | None = None
    ) -> Any:
        """Invoke an action and return its arbitrary JSON result.

        The request shape is ``{type, id, action, args, deadline_ms}``.
        ``deadline_ms`` is an absolute Unix timestamp in milliseconds.
        """

        if not isinstance(action, str) or not action:
            raise PluginError(
                "PLUGIN.INVALID_REQUEST",
                "action must be a non-empty string",
                details={"dispatched": False},
            )
        effective_timeout = (
            self.timeout
            if timeout is None
            else _validate_timeout(timeout, "invoke timeout")
        )
        deadline = time.monotonic() + effective_timeout

        with self._request_lock:
            self._start_locked(deadline=deadline)
            request_id = self._new_request_id()
            dispatched = False
            try:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise self._host_error(
                        "PLUGIN.HOST_TIMEOUT",
                        f"plugin {self.name!r} did not respond before the deadline",
                        retryable=True,
                    )
                deadline_ms = int((time.time() + remaining) * 1000)
                dispatched = self._write_request(
                    {
                        "type": "invoke",
                        "id": request_id,
                        "action": action,
                        "args": args,
                        "deadline_ms": deadline_ms,
                    }
                )
                while True:
                    message = self._read_message(deadline)
                    assert message is not None
                    response_id = message.get("id")
                    if (
                        isinstance(response_id, str)
                        and response_id in self._discard_response_ids
                    ):
                        self._validate_discarded_response(message, response_id)
                        self._discard_response_ids.remove(response_id)
                        continue
                    return self._result_from_message(
                        message, request_id, dispatched=dispatched
                    )
            except PluginError as exc:
                if dispatched:
                    exc = _with_dispatched(exc)
                # A well-formed error response is part of normal plugin RPC and
                # does not invalidate the process.  Host/protocol failures do.
                if exc.code.startswith("PLUGIN.HOST_"):
                    self._abort(exc)
                    raise AssertionError("unreachable")
                raise exc

    def close(self) -> None:
        """Terminate the plugin process tree.  The method is idempotent."""

        with self._lifecycle_lock:
            if self._closed:
                return
            self._closed = True
            process = self._process

        # Wake a request blocked on stdout even if a malicious plugin filled the
        # bounded event queue with unsolicited messages.
        self._force_event(_StreamEvent("closed"))
        if process is not None:
            self._terminate_process(process)
        self._reader_stop.set()
        self._close_pipes(process)
        self._join_readers()

    def __enter__(self) -> ProcessPlugin:
        self.start()
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()

    def __del__(self) -> None:
        # Best effort only: module globals may already be cleared at shutdown.
        try:
            self.close()
        except Exception:
            pass

    def _start_readers(self, process: subprocess.Popen[bytes]) -> None:
        assert process.stdout is not None
        assert process.stderr is not None
        self._stdout_thread = threading.Thread(
            target=self._read_stdout,
            args=(process.stdout,),
            name=f"{self.name}-stdout",
            daemon=True,
        )
        self._stderr_thread = threading.Thread(
            target=self._read_stderr,
            args=(process.stderr,),
            name=f"{self.name}-stderr",
            daemon=True,
        )
        self._stdout_thread.start()
        self._stderr_thread.start()

    def _read_stdout(self, stream: BinaryIO) -> None:
        try:
            while not self._reader_stop.is_set():
                raw_line = stream.readline(_MAX_STDOUT_LINE_BYTES + 1)
                if not raw_line:
                    self._offer_event(_StreamEvent("eof"))
                    return
                if len(raw_line) > _MAX_STDOUT_LINE_BYTES:
                    self._offer_event(
                        _StreamEvent(
                            "error",
                            f"stdout line exceeds {_MAX_STDOUT_LINE_BYTES} bytes",
                        )
                    )
                    return
                try:
                    line = raw_line.decode("utf-8", errors="strict")
                except UnicodeDecodeError as exc:
                    self._offer_event(
                        _StreamEvent("error", f"stdout is not UTF-8: {exc}")
                    )
                    return
                self._offer_event(_StreamEvent("line", line))
        except (OSError, ValueError) as exc:
            if not self._reader_stop.is_set():
                self._offer_event(
                    _StreamEvent("error", f"could not read stdout: {exc}")
                )

    def _read_stderr(self, stream: BinaryIO) -> None:
        try:
            while not self._reader_stop.is_set():
                chunk = stream.read(_STDERR_CHUNK_BYTES)
                if not chunk:
                    return
                text = chunk.decode("utf-8", errors="replace")
                with self._stderr_lock:
                    self._stderr_chunks.append(text)
                    self._stderr_size += len(text.encode("utf-8"))
                    while (
                        self._stderr_size > _MAX_STDERR_BYTES
                        and len(self._stderr_chunks) > 1
                    ):
                        removed = self._stderr_chunks.popleft()
                        self._stderr_size -= len(removed.encode("utf-8"))
        except (OSError, ValueError):
            return

    def _offer_event(self, event: _StreamEvent) -> None:
        while not self._reader_stop.is_set():
            try:
                self._stdout_events.put(event, timeout=0.05)
                return
            except queue.Full:
                continue

    def _force_event(self, event: _StreamEvent) -> None:
        """Insert a lifecycle event, dropping queued data if necessary."""

        while True:
            try:
                self._stdout_events.put_nowait(event)
                return
            except queue.Full:
                try:
                    self._stdout_events.get_nowait()
                except queue.Empty:
                    continue

    def _read_message(
        self, deadline: float, *, allow_timeout: bool = False
    ) -> dict[str, Any] | None:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                if allow_timeout:
                    return None
                raise self._host_error(
                    "PLUGIN.HOST_TIMEOUT",
                    f"plugin {self.name!r} did not respond before the deadline",
                    retryable=True,
                )
            try:
                event = self._stdout_events.get(timeout=remaining)
            except queue.Empty:
                if allow_timeout:
                    return None
                raise self._host_error(
                    "PLUGIN.HOST_TIMEOUT",
                    f"plugin {self.name!r} did not respond before the deadline",
                    retryable=True,
                ) from None

            if event.kind == "closed":
                raise self._host_error(
                    "PLUGIN.HOST_CLOSED", f"plugin {self.name!r} was closed"
                )
            if event.kind == "eof":
                process = self._process
                returncode = process.poll() if process is not None else None
                raise self._host_error(
                    "PLUGIN.HOST_EOF",
                    f"plugin {self.name!r} closed stdout unexpectedly",
                    extra={"returncode": returncode},
                    retryable=True,
                )
            if event.kind == "error":
                raise self._host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    event.value or "invalid plugin stdout",
                )
            if event.kind != "line" or event.value is None:
                raise self._host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR", "invalid stdout reader event"
                )

            line = event.value.rstrip("\r\n")
            if not line.strip():
                raise self._host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    "plugin emitted an empty stdout line",
                )
            try:
                message = json.loads(line)
            except json.JSONDecodeError as exc:
                raise self._host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    f"plugin emitted invalid JSON: {exc.msg}",
                    extra={"line": line[:500]},
                ) from exc
            if not isinstance(message, dict):
                raise self._host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    "plugin response must be a JSON object",
                )
            return message

    def _manifest_from_message(
        self,
        message: dict[str, Any],
        *,
        expected_id: str | None,
        dispatched: bool,
    ) -> dict[str, Any]:
        if expected_id is not None:
            self._require_response_id(message, expected_id)
        elif "id" in message:
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "proactive manifest must not contain a request id",
            )

        if "error" in message:
            raise self._plugin_error_from_message(message, dispatched=dispatched)

        if "manifest" in message:
            manifest = message["manifest"]
        elif "result" in message:
            manifest = message["result"]
        elif message.get("type") == "manifest":
            manifest = {key: value for key, value in message.items() if key != "id"}
        elif expected_id is None:
            # Proactive plugins may emit either a transport wrapper or the bare
            # canonical CapabilityManifest as their first stdout object.
            manifest = message
        else:
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "expected a manifest response",
            )

        if not isinstance(manifest, dict):
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin manifest must be a JSON object",
            )
        self._validate_manifest(manifest)
        return dict(manifest)

    def _validate_manifest(self, manifest: dict[str, Any]) -> None:
        if jsonschema is None:
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "canonical manifest schema validation requires jsonschema",
            )

        resource_name = "/".join(_MANIFEST_SCHEMA_RESOURCE)
        try:
            resource = resources.files("ai_auto_desktop").joinpath(
                *_MANIFEST_SCHEMA_RESOURCE
            )
            schema = json.loads(resource.read_text(encoding="utf-8"))
            jsonschema.Draft202012Validator.check_schema(schema)
        except (OSError, TypeError, ValueError, jsonschema.SchemaError) as exc:
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                f"canonical manifest schema resource {resource_name!r} "
                "is unavailable or invalid",
            ) from exc

        try:
            jsonschema.Draft202012Validator(schema).validate(manifest)
        except jsonschema.ValidationError as exc:
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                f"plugin manifest does not satisfy its schema: {exc}",
            ) from exc

        if manifest.get("apiVersion") != "ai-auto-desktop.dev/v1alpha1":
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin manifest has an unsupported apiVersion",
            )
        if manifest.get("kind") != "CapabilityManifest":
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin manifest kind must be CapabilityManifest",
            )
        metadata = manifest.get("metadata")
        actions = manifest.get("actions")
        if not isinstance(metadata, dict) or not isinstance(metadata.get("name"), str):
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin manifest must contain metadata.name",
            )
        if not isinstance(actions, dict) or not actions:
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin manifest must contain an actions object",
            )
        for action_name, contract in actions.items():
            artifacts = contract.get("artifacts")
            if artifacts is None:
                continue
            inputs = artifacts.get("inputs", {})
            outputs = artifacts.get("outputs", {})
            duplicate_names = sorted(set(inputs).intersection(outputs))
            if duplicate_names:
                raise self._host_error(
                    "PLUGIN.HOST_PROTOCOL_ERROR",
                    "artifact slot names must be unique within an action",
                    extra={
                        "action": action_name,
                        "duplicate_slots": duplicate_names,
                    },
                )
            for direction, slots in (("inputs", inputs), ("outputs", outputs)):
                declared = [
                    (slot_name, slot["pointer"], _json_pointer_tokens(slot["pointer"]))
                    for slot_name, slot in slots.items()
                ]
                for index, (left_name, left_pointer, left_tokens) in enumerate(
                    declared
                ):
                    for right_name, right_pointer, right_tokens in declared[index + 1 :]:
                        shared = min(len(left_tokens), len(right_tokens))
                        if left_tokens[:shared] != right_tokens[:shared]:
                            continue
                        raise self._host_error(
                            "PLUGIN.HOST_PROTOCOL_ERROR",
                            "artifact slot pointers must not overlap per direction",
                            extra={
                                "action": action_name,
                                "direction": direction,
                                "slots": sorted((left_name, right_name)),
                                "pointers": sorted((left_pointer, right_pointer)),
                            },
                        )

    def _result_from_message(
        self, message: dict[str, Any], expected_id: str, *, dispatched: bool
    ) -> Any:
        self._require_response_id(message, expected_id)
        has_result = "result" in message
        has_error = "error" in message
        if has_result == has_error:
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin response must contain exactly one of result or error",
            )
        if has_error:
            raise self._plugin_error_from_message(message, dispatched=dispatched)
        return message["result"]

    def _require_response_id(
        self, message: dict[str, Any], expected_id: str
    ) -> None:
        actual_id = message.get("id")
        if actual_id != expected_id:
            raise self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin response id does not match its request",
                extra={"expected_id": expected_id, "actual_id": actual_id},
            )

    def _validate_discarded_response(
        self, message: dict[str, Any], response_id: str
    ) -> None:
        self._require_response_id(message, response_id)
        if "error" in message:
            error = self._plugin_error_from_message(message, dispatched=True)
            if error.code.startswith("PLUGIN.HOST_"):
                raise error
            return
        # Validate all supported successful manifest response envelopes.
        self._manifest_from_message(
            message, expected_id=response_id, dispatched=True
        )

    def _plugin_error_from_message(
        self, message: dict[str, Any], *, dispatched: bool
    ) -> PluginError:
        payload = message.get("error")
        if not isinstance(payload, dict):
            return self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin error payload must be a JSON object",
            )
        code = payload.get("code", "PLUGIN.ERROR")
        error_message = payload.get("message", "plugin invocation failed")
        retryable = payload.get("retryable", False)
        if not isinstance(code, str) or not isinstance(error_message, str):
            return self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin error code and message must be strings",
            )
        if not isinstance(retryable, bool):
            return self._host_error(
                "PLUGIN.HOST_PROTOCOL_ERROR",
                "plugin error retryable field must be a boolean",
            )

        # ``data`` is the canonical v0 wire spelling; ``details`` is retained
        # for compatibility with early plugins and maps to the same host field.
        raw_details = payload.get("details", payload.get("data"))
        if raw_details is None:
            details: dict[str, Any] = {}
        elif isinstance(raw_details, dict):
            details = dict(raw_details)
        else:
            details = {"data": raw_details}
        if dispatched:
            details["dispatched"] = True
        return PluginError(
            code, error_message, details=details, retryable=retryable
        )

    def _write_request(self, request: dict[str, Any]) -> bool:
        try:
            encoded = (
                json.dumps(
                    request,
                    ensure_ascii=False,
                    allow_nan=False,
                    separators=(",", ":"),
                )
                + "\n"
            ).encode("utf-8")
        except (TypeError, ValueError) as exc:
            raise PluginError(
                "PLUGIN.INVALID_REQUEST",
                f"request is not JSON serializable: {exc}",
                details={"dispatched": False},
            ) from exc

        process = self._process
        if process is None or process.stdin is None:
            raise self._host_error(
                "PLUGIN.HOST_CLOSED", f"plugin {self.name!r} is not running"
            )
        if self._closed:
            raise self._host_error(
                "PLUGIN.HOST_CLOSED", f"plugin {self.name!r} is closed"
            )
        if process.poll() is not None:
            raise self._host_error(
                "PLUGIN.HOST_EOF",
                f"plugin {self.name!r} exited before request dispatch",
                extra={"returncode": process.returncode},
                retryable=True,
            )

        try:
            process.stdin.write(encoded)
            process.stdin.flush()
        except (BrokenPipeError, OSError, ValueError) as exc:
            raise self._host_error(
                "PLUGIN.HOST_IO_ERROR",
                f"could not write to plugin {self.name!r}: {exc}",
                extra={"exception_type": type(exc).__name__},
                retryable=True,
            ) from exc
        return True

    def _host_error(
        self,
        code: str,
        message: str,
        *,
        extra: Mapping[str, Any] | None = None,
        retryable: bool = False,
    ) -> PluginError:
        details = dict(extra or {})
        stderr = self.stderr
        if stderr:
            details["stderr"] = stderr
        return PluginError(code, message, details=details, retryable=retryable)

    def _abort(self, error: PluginError) -> None:
        with self._lifecycle_lock:
            self._closed = True
            process = self._process
        if process is not None:
            self._terminate_process(process)
        self._reader_stop.set()
        self._close_pipes(process)
        self._join_readers()
        raise error

    def _terminate_process(self, process: subprocess.Popen[bytes]) -> None:
        if os.name == "posix":
            self._terminate_posix_group(process)
            return

        # CREATE_NEW_PROCESS_GROUP above is useful to descendants which opt in
        # to console control handling.  terminate/kill is the dependable stdlib
        # fallback on Windows; Python exposes no general process-tree kill API.
        try:
            process.terminate()
        except (OSError, ProcessLookupError):
            pass
        try:
            process.wait(timeout=_TERMINATE_GRACE_SECONDS)
            return
        except (subprocess.TimeoutExpired, OSError):
            pass
        try:
            process.kill()
        except (OSError, ProcessLookupError):
            pass
        try:
            process.wait(timeout=_TERMINATE_GRACE_SECONDS)
        except (subprocess.TimeoutExpired, OSError):
            pass

    def _terminate_posix_group(self, process: subprocess.Popen[bytes]) -> None:
        pgid = process.pid
        try:
            os.killpg(pgid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError, OSError):
            pass

        grace_deadline = time.monotonic() + _TERMINATE_GRACE_SECONDS
        while time.monotonic() < grace_deadline:
            # Reap the group leader when it exits so a zombie leader does not
            # make killpg(..., 0) look like a still-running process group.
            process.poll()
            if not _process_group_exists(pgid):
                break
            time.sleep(0.02)

        if _process_group_exists(pgid):
            try:
                os.killpg(pgid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                pass
        try:
            process.wait(timeout=_TERMINATE_GRACE_SECONDS)
        except (subprocess.TimeoutExpired, OSError):
            pass

    def _close_pipes(self, process: subprocess.Popen[bytes] | None) -> None:
        if process is None:
            return
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass

    def _join_readers(self) -> None:
        current = threading.current_thread()
        for thread in (self._stdout_thread, self._stderr_thread):
            if thread is not None and thread is not current:
                thread.join(timeout=0.20)

    @staticmethod
    def _new_request_id() -> str:
        return uuid.uuid4().hex


def _validate_timeout(value: float, label: str) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{label} must be a positive finite number")
    try:
        timeout = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{label} must be a positive finite number") from exc
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError(f"{label} must be a positive finite number")
    return timeout


def _with_dispatched(error: PluginError) -> PluginError:
    if error.dispatched:
        return error
    details = dict(error.details)
    details["dispatched"] = True
    return PluginError(
        error.code,
        error.message,
        details=details,
        retryable=error.retryable,
    )


def _process_group_exists(pgid: int) -> bool:
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


__all__ = ["DEFAULT_TIMEOUT", "PluginError", "ProcessPlugin"]
