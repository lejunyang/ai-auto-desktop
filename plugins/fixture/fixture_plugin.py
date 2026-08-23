#!/usr/bin/env python3
"""Deterministic NDJSON process fixture for the desktop plugin runtime.

Protocol messages are written to stdout, one compact JSON object per line.
Diagnostics are deliberately written to stderr so they cannot corrupt NDJSON.
"""

from __future__ import annotations

import json
import math
import os
import sys
import time
from typing import Any, NoReturn


PLUGIN_NAME = "fixture"
PLUGIN_VERSION = "1.0.0"
PROTOCOL_VERSION = 1

ACTION_CONTRACTS: dict[str, dict[str, Any]] = {
    "ocr": {
        "contract_major": 1,
        "description": "Return deterministic mock OCR data.",
        "effect": {
            "default_class": "read_only",
        },
        "risk": {"category": "observe", "level": "low"},
        "input_schema": {
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "language": {"type": "string"},
                "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                "blocks": {"type": "array", "items": {"type": "object"}},
                "result": {},
            },
            "additionalProperties": True,
        },
        "output_schema": {
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "language": {"type": "string"},
                "confidence": {"type": "number"},
                "blocks": {"type": "array"},
            },
        },
        "errors": [
            {
                "code": "FIXTURE.INVALID_ARGS",
                "description": "An OCR mock argument has an invalid type or range.",
                "retryable": False,
                "effect": "not_applied",
                "data_schema": {"type": "object"},
            }
        ],
    },
    "invoke": {
        "contract_major": 1,
        "description": "Acknowledge a mock desktop invocation and echo its arguments.",
        "effect": {
            "default_class": "non_idempotent",
        },
        "risk": {"category": "navigate", "level": "medium"},
        "input_schema": {
            "type": "object",
            "properties": {
                "target": {},
                "operation": {"type": "string"},
                "result": {},
            },
            "additionalProperties": True,
        },
        "output_schema": {
            "type": "object",
            "properties": {
                "ok": {"const": True},
                "invoked": {"const": True},
                "operation": {"type": "string"},
                "target": {},
                "args": {"type": "object"},
            },
        },
        "errors": [
            {
                "code": "FIXTURE.INVALID_ARGS",
                "description": "An invocation argument has an invalid type.",
                "retryable": False,
                "effect": "not_applied",
                "data_schema": {"type": "object"},
            }
        ],
    },
    "transient": {
        "contract_major": 1,
        "description": "Fail retryably for the first N calls for a key, then succeed.",
        "effect": {
            "default_class": "idempotent",
        },
        "risk": {
            "category": "custom",
            "level": "low",
            "custom_name": "fixture.transient",
        },
        "input_schema": {
            "type": "object",
            "properties": {
                "failures": {"type": "integer", "minimum": 0},
                "key": {"type": "string", "minLength": 1},
                "code": {"type": "string"},
                "message": {"type": "string"},
                "result": {},
            },
            "additionalProperties": True,
        },
        "output_schema": {
            "type": "object",
            "properties": {
                "ok": {"const": True},
                "key": {"type": "string"},
                "attempt": {"type": "integer"},
                "failures": {"type": "integer"},
            },
        },
        "errors": [
            {
                "code": "FIXTURE.TRANSIENT",
                "description": "The configured transient attempt has not yet succeeded.",
                "retryable": True,
                "effect": "not_applied",
                "data_schema": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "attempt": {"type": "integer"},
                        "failures": {"type": "integer"},
                    },
                },
            },
            {
                "code": "FIXTURE.INVALID_ARGS",
                "description": "A transient-control argument has an invalid type.",
                "retryable": False,
                "effect": "not_applied",
                "data_schema": {"type": "object"},
            },
        ],
    },
    "error": {
        "contract_major": 1,
        "description": "Return a requested structured error.",
        "effect": {
            "default_class": "contextual",
        },
        "risk": {
            "category": "custom",
            "level": "contextual",
            "custom_name": "fixture.error",
        },
        "input_schema": {
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "message": {"type": "string"},
                "retryable": {"type": "boolean"},
                "data": {},
            },
            "additionalProperties": True,
        },
        "output_schema": {},
        "errors": [
            {
                "code": "FIXTURE.REQUESTED",
                "description": "A caller-requested fixture error; its actual code may be overridden.",
                "retryable": False,
                "effect": "not_applied",
                "data_schema": {},
            },
            {
                "code": "FIXTURE.INVALID_ARGS",
                "description": "An error-control argument has an invalid type.",
                "retryable": False,
                "effect": "not_applied",
                "data_schema": {"type": "object"},
            },
        ],
    },
    "sleep": {
        "contract_major": 1,
        "description": "Sleep for a requested duration before succeeding.",
        "effect": {
            "default_class": "read_only",
        },
        "risk": {
            "category": "custom",
            "level": "low",
            "custom_name": "fixture.sleep",
        },
        "input_schema": {
            "type": "object",
            "properties": {
                "seconds": {"type": "number", "minimum": 0},
                "milliseconds": {"type": "number", "minimum": 0},
                "ms": {"type": "number", "minimum": 0},
                "result": {},
            },
            "additionalProperties": True,
        },
        "output_schema": {
            "type": "object",
            "properties": {
                "ok": {"const": True},
                "sleptSeconds": {"type": "number"},
            },
        },
        "errors": [
            {
                "code": "FIXTURE.INVALID_ARGS",
                "description": "A duration has an invalid type or range.",
                "retryable": False,
                "effect": "not_applied",
                "data_schema": {"type": "object"},
            }
        ],
    },
}

MANIFEST: dict[str, Any] = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {
        "name": PLUGIN_NAME,
        "version": PLUGIN_VERSION,
        "description": "Deterministic process fixture for runtime integration tests.",
    },
    "actions": ACTION_CONTRACTS,
    "runtime": {
        "kind": "process",
        "protocol": "ndjson-stdio-v1",
        "entrypoint": "./run.sh",
    },
}

ACTION_ALIASES = {
    "handshake": "manifest",
    "get_manifest": "manifest",
    "get-manifest": "manifest",
    "fixture.ocr@1": "ocr",
    "ocr.mock": "ocr",
    "mock.ocr": "ocr",
    "mock_ocr": "ocr",
    "fixture.invoke@1": "invoke",
    "desktop.invoke": "invoke",
    "desktop_invoke": "invoke",
    "fixture.transient@1": "transient",
    "fail_transient": "transient",
    "fail-transient": "transient",
    "fixture.error@1": "error",
    "fail": "error",
    "fixture.sleep@1": "sleep",
    "delay": "sleep",
}

_transient_attempts: dict[str, int] = {}


class RequestError(Exception):
    """An error that should be returned through the plugin protocol."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        retryable: bool = False,
        data: Any = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.retryable = retryable
        self.data = data


def debug(message: str) -> None:
    """Write a single diagnostic line without touching protocol stdout."""

    print(f"[{PLUGIN_NAME}] {message}", file=sys.stderr, flush=True)


def emit(message: dict[str, Any]) -> None:
    print(
        json.dumps(message, ensure_ascii=False, separators=(",", ":")),
        file=sys.stdout,
        flush=True,
    )


def _error_payload(error: RequestError) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "code": error.code,
        "message": error.message,
        "retryable": error.retryable,
    }
    if error.data is not None:
        payload["data"] = error.data
    return payload


def _response_base(request_id: Any, request: Any = None) -> dict[str, Any]:
    response: dict[str, Any] = {"id": request_id}
    if isinstance(request, dict) and request.get("jsonrpc") == "2.0":
        response["jsonrpc"] = "2.0"
    return response


def emit_result(request_id: Any, result: Any, request: Any = None) -> None:
    response = _response_base(request_id, request)
    response["result"] = result
    emit(response)


def emit_error(request_id: Any, error: RequestError, request: Any = None) -> None:
    response = _response_base(request_id, request)
    response["error"] = _error_payload(error)
    emit(response)


def _as_args(value: Any) -> dict[str, Any]:
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise RequestError(
            "FIXTURE.INVALID_ARGS",
            "args/params must be a JSON object or null",
            data={"receivedType": type(value).__name__},
        )
    return value


def parse_request(request: Any) -> tuple[Any, str, dict[str, Any]]:
    if not isinstance(request, dict):
        raise RequestError("PROTOCOL.INVALID_REQUEST", "request must be a JSON object")

    request_id = request.get("id")
    action = request.get("action")
    if action is None:
        action = request.get("method")

    request_type = request.get("type")
    if action is None and request_type in {
        "handshake",
        "manifest",
        "manifest.request",
        "get_manifest",
    }:
        action = "manifest"

    if not isinstance(action, str) or not action.strip():
        raise RequestError(
            "PROTOCOL.INVALID_REQUEST",
            "request.action must be a non-empty string",
        )

    action = ACTION_ALIASES.get(action.strip(), action.strip())
    args_value = request["args"] if "args" in request else request.get("params")
    return request_id, action, _as_args(args_value)


def _number(value: Any, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise RequestError("FIXTURE.INVALID_ARGS", f"{name} must be a number")
    converted = float(value)
    if not math.isfinite(converted) or converted < 0:
        raise RequestError(
            "FIXTURE.INVALID_ARGS", f"{name} must be finite and non-negative"
        )
    return converted


def _integer(value: Any, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise RequestError(
            "FIXTURE.INVALID_ARGS", f"{name} must be a non-negative integer"
        )
    return value


def action_ocr_mock(args: dict[str, Any]) -> Any:
    if "result" in args:
        return args["result"]

    text = args.get("text", "Fixture OCR text")
    if not isinstance(text, str):
        raise RequestError("FIXTURE.INVALID_ARGS", "text must be a string")
    language = args.get("language", "en")
    if not isinstance(language, str):
        raise RequestError("FIXTURE.INVALID_ARGS", "language must be a string")
    confidence = _number(args.get("confidence", 0.99), "confidence")
    if confidence > 1:
        raise RequestError("FIXTURE.INVALID_ARGS", "confidence must be at most 1")

    blocks = args.get("blocks")
    if blocks is None:
        blocks = (
            []
            if not text
            else [
                {
                    "text": text,
                    "confidence": confidence,
                    "bounds": {"x": 0, "y": 0, "width": 100, "height": 20},
                }
            ]
        )
    if not isinstance(blocks, list):
        raise RequestError("FIXTURE.INVALID_ARGS", "blocks must be an array")

    return {
        "text": text,
        "language": language,
        "confidence": confidence,
        "blocks": blocks,
    }


def action_desktop_invoke(args: dict[str, Any]) -> Any:
    if "result" in args:
        return args["result"]
    operation = args.get("operation", args.get("command", "invoke"))
    if not isinstance(operation, str) or not operation:
        raise RequestError(
            "FIXTURE.INVALID_ARGS", "operation must be a non-empty string"
        )
    return {
        "ok": True,
        "invoked": True,
        "operation": operation,
        "target": args.get("target"),
        "args": args,
    }


def action_transient(args: dict[str, Any]) -> Any:
    failures_value = args.get(
        "failures",
        args.get("fail_count", args.get("times", args.get("n", 1))),
    )
    failures = _integer(failures_value, "failures")
    key = args.get("key", "default")
    if not isinstance(key, str) or not key:
        raise RequestError("FIXTURE.INVALID_ARGS", "key must be a non-empty string")

    attempt = _transient_attempts.get(key, 0) + 1
    _transient_attempts[key] = attempt
    if attempt <= failures:
        code = args.get("code", "FIXTURE.TRANSIENT")
        message = args.get(
            "message",
            f"fixture transient failure {attempt} of {failures}",
        )
        if not isinstance(code, str) or not isinstance(message, str):
            raise RequestError(
                "FIXTURE.INVALID_ARGS", "code and message must be strings"
            )
        raise RequestError(
            code,
            message,
            retryable=True,
            data={"key": key, "attempt": attempt, "failures": failures},
        )

    if "result" in args:
        return args["result"]
    return {
        "ok": True,
        "key": key,
        "attempt": attempt,
        "failures": failures,
    }


def action_error(args: dict[str, Any]) -> NoReturn:
    code = args.get("code", "FIXTURE.REQUESTED")
    message = args.get("message", "fixture requested an error")
    retryable = args.get("retryable", False)
    if not isinstance(code, str) or not code:
        raise RequestError("FIXTURE.INVALID_ARGS", "code must be a non-empty string")
    if not isinstance(message, str):
        raise RequestError("FIXTURE.INVALID_ARGS", "message must be a string")
    if not isinstance(retryable, bool):
        raise RequestError("FIXTURE.INVALID_ARGS", "retryable must be a boolean")
    raise RequestError(
        code,
        message,
        retryable=retryable,
        data=args.get("data"),
    )


def action_sleep(args: dict[str, Any]) -> Any:
    if "seconds" in args:
        seconds = _number(args["seconds"], "seconds")
    elif "milliseconds" in args:
        seconds = _number(args["milliseconds"], "milliseconds") / 1000
    elif "ms" in args:
        seconds = _number(args["ms"], "ms") / 1000
    else:
        seconds = 0.0

    time.sleep(seconds)
    if "result" in args:
        return args["result"]
    return {"ok": True, "sleptSeconds": seconds}


def dispatch(action: str, args: dict[str, Any]) -> Any:
    if action == "manifest":
        return MANIFEST
    if action == "ocr":
        return action_ocr_mock(args)
    if action == "invoke":
        return action_desktop_invoke(args)
    if action == "transient":
        return action_transient(args)
    if action == "error":
        return action_error(args)
    if action == "sleep":
        return action_sleep(args)
    raise RequestError(
        "PROTOCOL.ACTION_NOT_FOUND",
        f"unknown action: {action}",
        data={
            "action": action,
            "availableActions": [
                f"{PLUGIN_NAME}.{name}@{contract['contract_major']}"
                for name, contract in ACTION_CONTRACTS.items()
            ],
        },
    )


def handle_line(line: str) -> None:
    request: Any = None
    request_id: Any = None
    try:
        try:
            request = json.loads(line)
        except json.JSONDecodeError as exc:
            raise RequestError(
                "PROTOCOL.PARSE_ERROR",
                "input line is not valid JSON",
                data={"line": exc.lineno, "column": exc.colno},
            ) from exc

        if isinstance(request, dict):
            request_id = request.get("id")
        request_id, action, args = parse_request(request)
        debug(f"request id={request_id!r} action={action!r}")
        result = dispatch(action, args)
        emit_result(request_id, result, request)
    except RequestError as exc:
        debug(f"error id={request_id!r} code={exc.code!r}: {exc.message}")
        emit_error(request_id, exc, request)
    except Exception as exc:  # Defensive fixture boundary: keep serving later requests.
        debug(f"internal error id={request_id!r}: {type(exc).__name__}: {exc}")
        emit_error(
            request_id,
            RequestError("PLUGIN.INTERNAL", "fixture plugin internal error"),
            request,
        )


def main() -> int:
    # A startup manifest is useful for hosts that negotiate before sending an
    # NDJSON request.  The request form remains canonical and always available.
    if "--manifest" in sys.argv[1:]:
        emit({"type": "manifest", "manifest": MANIFEST})
    debug(
        f"started pid={os.getpid()} protocol={PROTOCOL_VERSION} "
        f"actions={','.join(MANIFEST['actions'])}"
    )
    for raw_line in sys.stdin:
        line = raw_line.strip()
        if line:
            handle_line(line)
    debug("stdin closed; exiting")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
