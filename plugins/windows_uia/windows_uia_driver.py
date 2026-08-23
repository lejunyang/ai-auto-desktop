#!/usr/bin/env python3
"""Windows UI Automation process driver.

The process boundary speaks the repository's UTF-8 NDJSON protocol.  The
driver core is deliberately independent from COM so its locator, snapshot and
stale-target rules can be exercised with a fake backend on every platform.

This driver only exposes native accessibility operations.  It never injects
keyboard or pointer input, captures pixels, or invokes OCR.
"""

from __future__ import annotations

from collections import deque
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
import copy
import ctypes
import hashlib
import json
import math
import os
import sys
import time
from typing import Any, NoReturn, Protocol
import uuid


PLUGIN_NAME = "desktop.windows_uia"
PLUGIN_VERSION = "0.1.0"
MAX_REQUEST_BYTES = 1024 * 1024
MAX_RESPONSE_BYTES = 8 * 1024 * 1024 - 1
MAX_FIELD_CHARS = 4096
DEFAULT_REQUEST_SECONDS = 30.0
DEFAULT_MAX_DEPTH = 32
DEFAULT_MAX_NODES = 1000
MAX_DEPTH = 128
MAX_NODES = 5000
MAX_CANDIDATE_SUMMARIES = 10

ACTION_IDS = {
    name: f"{PLUGIN_NAME}.{name}@1"
    for name in (
        "list_windows",
        "snapshot",
        "find",
        "focus",
        "invoke",
        "set_value",
    )
}
ACTION_NAMES = {full_name: short_name for short_name, full_name in ACTION_IDS.items()}
WRITE_ACTIONS = frozenset({"focus", "invoke", "set_value"})
NODE_ACTIONS = frozenset(WRITE_ACTIONS)
STATE_NAMES = ("enabled", "offscreen", "focusable", "focused", "read_only")


BOUNDS_SCHEMA: dict[str, Any] = {
    "oneOf": [
        {
            "type": "object",
            "required": ["x", "y", "width", "height"],
            "properties": {
                "x": {"type": "integer"},
                "y": {"type": "integer"},
                "width": {"type": "integer", "minimum": 0},
                "height": {"type": "integer", "minimum": 0},
            },
            "additionalProperties": False,
        },
        {"type": "null"},
    ]
}

LOCATOR_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "role": {"type": "string", "maxLength": 256},
        "name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "value": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS},
        "automation_id": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "class_name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "framework_id": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "states": {
            "type": "object",
            "properties": {name: {"type": ["boolean", "null"]} for name in STATE_NAMES},
            "additionalProperties": False,
        },
        "actions": {
            "type": "array",
            "items": {"enum": sorted(NODE_ACTIONS)},
            "uniqueItems": True,
        },
        "match": {"const": "exact"},
    },
    "additionalProperties": False,
    "minProperties": 1,
}

TARGET_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["snapshot_id", "revision", "node_id"],
    "properties": {
        "snapshot_id": {"type": "string", "minLength": 1},
        "revision": {"type": "integer", "minimum": 1},
        "node_id": {"type": "string", "minLength": 1},
    },
    "additionalProperties": False,
}

COMMON_ERRORS = (
    ("DRIVER.INVALID_REQUEST", "The action arguments are invalid.", False),
    ("DRIVER.UNAVAILABLE", "Windows UI Automation is unavailable.", False),
    ("DRIVER.ACTION_FAILED", "The native observation operation failed.", False),
    ("DRIVER.TIMEOUT", "The request deadline elapsed.", True),
    ("DRIVER.OUTPUT_TOO_LARGE", "The normalized response exceeds the wire limit.", False),
)
LOCATOR_ERRORS = (
    ("DRIVER.NOT_FOUND", "The locator matched no node.", False),
    ("DRIVER.AMBIGUOUS", "The locator matched more than one node.", False),
    ("DRIVER.STALE_SNAPSHOT", "The snapshot target is no longer current.", False),
    ("DRIVER.SNAPSHOT_TRUNCATED", "A bounded snapshot cannot prove uniqueness.", False),
)
ACTION_ERRORS = (
    ("DRIVER.ACTION_UNSUPPORTED", "The target lacks the required native UIA pattern.", False),
    ("DRIVER.PROTECTED_ELEMENT", "The target exposes protected content.", False),
    ("DRIVER.UNKNOWN_EFFECT", "The native action may have taken effect.", False),
)


def _error_contracts(
    entries: Sequence[tuple[str, str, bool]], *, unknown_effect: bool = False
) -> list[dict[str, Any]]:
    return [
        {
            "code": code,
            "description": description,
            "retryable": retryable,
            "effect": "unknown" if code == "DRIVER.UNKNOWN_EFFECT" else "not_applied",
            "data_schema": {"type": "object"},
        }
        for code, description, retryable in entries
    ]


def _contract(
    description: str,
    *,
    effect: str,
    risk_category: str,
    risk_level: str,
    input_schema: dict[str, Any],
    output_schema: dict[str, Any],
    errors: Sequence[tuple[str, str, bool]],
    permissions: Sequence[str],
) -> dict[str, Any]:
    return {
        "contract_major": 1,
        "description": description,
        "effect": {"default_class": effect},
        "risk": {"category": risk_category, "level": risk_level},
        "permissions": list(permissions),
        "input_schema": input_schema,
        "output_schema": output_schema,
        "errors": _error_contracts(errors, unknown_effect=effect in {"contextual", "non_idempotent"}),
    }


SNAPSHOT_OUTPUT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["snapshot_id", "revision", "app", "window", "nodes"],
    "properties": {
        "snapshot_id": {"type": "string"},
        "revision": {"type": "integer", "minimum": 1},
        "app": {"type": "object"},
        "window": {"type": "object"},
        "nodes": {"type": "array", "items": {"type": "object"}},
        "truncated": {"type": "boolean"},
    },
    "additionalProperties": False,
}

COMMON_WRITE_INPUT: dict[str, Any] = {
    "type": "object",
    "required": ["target", "locator"],
    "properties": {"target": TARGET_SCHEMA, "locator": LOCATOR_SCHEMA},
    "additionalProperties": False,
}
WRITE_OUTPUT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["ok", "action", "resolved"],
    "properties": {
        "ok": {"const": True},
        "action": {"enum": sorted(WRITE_ACTIONS)},
        "resolved": TARGET_SCHEMA,
        "backend_result": {},
    },
    "additionalProperties": False,
}

ACTION_CONTRACTS: dict[str, dict[str, Any]] = {
    "list_windows": _contract(
        "List top-level Windows desktop windows through Win32/UIA.",
        effect="read_only",
        risk_category="observe",
        risk_level="low",
        input_schema={
            "type": "object",
            "properties": {"include_invisible": {"type": "boolean"}},
            "additionalProperties": False,
        },
        output_schema={
            "type": "object",
            "required": ["windows"],
            "properties": {"windows": {"type": "array", "items": {"type": "object"}}},
            "additionalProperties": False,
        },
        errors=COMMON_ERRORS,
        permissions=("desktop.observe",),
    ),
    "snapshot": _contract(
        "Capture a bounded normalized UIA Control View for one exact window.",
        effect="read_only",
        risk_category="observe",
        risk_level="low",
        input_schema={
            "type": "object",
            "required": ["window"],
            "properties": {
                "window": {"type": "object", "minProperties": 1},
                "max_depth": {"type": "integer", "minimum": 0, "maximum": MAX_DEPTH},
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": MAX_NODES},
            },
            "additionalProperties": False,
        },
        output_schema=SNAPSHOT_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS[:2],
        permissions=("desktop.observe",),
    ),
    "find": _contract(
        "Resolve an exact-by-default locator in one current snapshot.",
        effect="read_only",
        risk_category="observe",
        risk_level="low",
        input_schema={
            "type": "object",
            "required": ["snapshot_id", "revision", "locator"],
            "properties": {
                "snapshot_id": TARGET_SCHEMA["properties"]["snapshot_id"],
                "revision": TARGET_SCHEMA["properties"]["revision"],
                "locator": LOCATOR_SCHEMA,
            },
            "additionalProperties": False,
        },
        output_schema={
            "type": "object",
            "required": ["target", "node"],
            "properties": {"target": TARGET_SCHEMA, "node": {"type": "object"}},
            "additionalProperties": False,
        },
        errors=COMMON_ERRORS + LOCATOR_ERRORS,
        permissions=("desktop.observe",),
    ),
    "focus": _contract(
        "Re-resolve a snapshot target and call native IUIAutomationElement.SetFocus.",
        effect="contextual",
        risk_category="navigate",
        risk_level="medium",
        input_schema=COMMON_WRITE_INPUT,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "invoke": _contract(
        "Re-resolve a snapshot target and call native InvokePattern.Invoke.",
        effect="non_idempotent",
        risk_category="modify",
        risk_level="high",
        input_schema=COMMON_WRITE_INPUT,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "set_value": _contract(
        "Re-resolve a snapshot target and call native ValuePattern.SetValue.",
        effect="contextual",
        risk_category="input",
        risk_level="high",
        input_schema={
            "type": "object",
            "required": ["target", "locator", "value"],
            "properties": {
                "target": TARGET_SCHEMA,
                "locator": LOCATOR_SCHEMA,
                "value": {"type": "string", "maxLength": MAX_FIELD_CHARS},
            },
            "additionalProperties": False,
        },
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
}

MANIFEST: dict[str, Any] = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {
        "name": PLUGIN_NAME,
        "version": PLUGIN_VERSION,
        "description": "Windows-only native UI Automation process driver.",
    },
    "actions": ACTION_CONTRACTS,
    "runtime": {
        "kind": "process",
        "protocol": "ndjson-stdio-v1",
        "entrypoint": "./run.cmd",
        "platforms": ["windows"],
    },
}


class DriverError(Exception):
    """Stable error returned over the process boundary."""

    def __init__(
        self, code: str, message: str, *, retryable: bool = False, data: Any = None
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.retryable = retryable
        self.data = data


@dataclass(slots=True)
class BackendNode:
    """One backend node.  ``native`` never crosses the wire boundary."""

    native: Any
    parent_index: int | None
    role: str
    name: str | None = None
    value: str | None = None
    states: Mapping[str, bool | None] | None = None
    bounds: Mapping[str, int] | None = None
    actions: Sequence[str] = ()
    provenance: Mapping[str, Any] | None = None


@dataclass(slots=True)
class BackendSnapshot:
    app: Mapping[str, Any]
    window: Mapping[str, Any]
    nodes: Sequence[BackendNode]
    truncated: bool = False


class UIABackend(Protocol):
    """Minimal native boundary used by the platform-independent driver."""

    name: str

    def list_windows(
        self, *, include_invisible: bool, deadline: float
    ) -> Sequence[Mapping[str, Any]]: ...

    def capture(
        self,
        window: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot: ...

    def focus(self, native: Any, *, deadline: float) -> Any: ...

    def invoke(self, native: Any, *, deadline: float) -> Any: ...

    def set_value(self, native: Any, value: str, *, deadline: float) -> Any: ...

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool: ...


@dataclass(slots=True)
class _SnapshotRecord:
    public: dict[str, Any]
    handles: dict[str, Any]
    fingerprints: dict[str, str]
    window_selector: dict[str, Any]
    max_depth: int
    max_nodes: int


def _fail(code: str, message: str, **data: Any) -> NoReturn:
    raise DriverError(code, message, data=data or None)


def _check_deadline(deadline: float, *, post_dispatch: bool = False) -> None:
    if time.monotonic() >= deadline:
        raise DriverError(
            "DRIVER.TIMEOUT",
            "request deadline elapsed",
            retryable=not post_dispatch,
            data={
                "phase": "post_dispatch" if post_dispatch else "before_dispatch",
                "effect": "unknown" if post_dispatch else "not_applied",
            },
        )


def _object(value: Any, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail("DRIVER.INVALID_REQUEST", f"{name} must be an object")
    return value


def _only_keys(value: Mapping[str, Any], allowed: set[str], name: str) -> None:
    unknown = sorted(set(value) - allowed)
    if unknown:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"{name} contains unsupported fields",
            fields=unknown,
        )


def _bounded_integer(value: Any, name: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"{name} must be an integer between {minimum} and {maximum}",
        )
    return value


def _text(value: Any, name: str, *, nullable: bool = False) -> str | None:
    if value is None and nullable:
        return None
    if not isinstance(value, str):
        _fail("DRIVER.INVALID_REQUEST", f"{name} must be a string")
    if len(value) > MAX_FIELD_CHARS:
        _fail("DRIVER.INVALID_REQUEST", f"{name} exceeds {MAX_FIELD_CHARS} characters")
    return value


def _safe_text(value: Any) -> str | None:
    if value is None:
        return None
    try:
        text = str(value)
    except Exception:
        return None
    return text[:MAX_FIELD_CHARS]


def _json_safe(value: Any, *, depth: int = 0) -> Any:
    if depth > 8:
        return None
    if value is None or isinstance(value, (str, bool, int)):
        return value if not isinstance(value, str) else value[:MAX_FIELD_CHARS]
    if isinstance(value, float):
        return value if math.isfinite(value) else None
    if isinstance(value, Mapping):
        return {
            str(key)[:256]: _json_safe(item, depth=depth + 1)
            for key, item in list(value.items())[:256]
        }
    if isinstance(value, Sequence) and not isinstance(value, (str, bytes, bytearray)):
        return [_json_safe(item, depth=depth + 1) for item in list(value)[:1024]]
    return _safe_text(value)


def _normalize_bounds(value: Mapping[str, Any] | None) -> dict[str, int] | None:
    if value is None:
        return None
    try:
        x = int(value["x"])
        y = int(value["y"])
        width = max(0, int(value["width"]))
        height = max(0, int(value["height"]))
    except (KeyError, TypeError, ValueError, OverflowError):
        return None
    return {"x": x, "y": y, "width": width, "height": height}


class WindowsUIADriver:
    """Snapshot-scoped UIA semantics over an injected native backend."""

    def __init__(self, backend: UIABackend | None = None) -> None:
        self.backend: UIABackend = backend if backend is not None else create_default_backend()
        self.generation = uuid.uuid4().hex
        self._revision = 0
        self._current: _SnapshotRecord | None = None

    def execute(self, action: str, args: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        values = {} if args is None else _object(args, "args")
        if action == "list_windows":
            return self._list_windows(values, deadline)
        if action == "snapshot":
            return self._snapshot(values, deadline)
        if action == "find":
            return self._find(values, deadline)
        if action in WRITE_ACTIONS:
            return self._write(action, values, deadline)
        _fail("DRIVER.INVALID_REQUEST", f"unknown action: {action}", action=action)

    def _list_windows(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, {"include_invisible"}, "args")
        include_invisible = args.get("include_invisible", False)
        if not isinstance(include_invisible, bool):
            _fail("DRIVER.INVALID_REQUEST", "include_invisible must be a boolean")
        windows = []
        for item in self.backend.list_windows(
            include_invisible=include_invisible, deadline=deadline
        ):
            _check_deadline(deadline)
            if not isinstance(item, Mapping):
                raise DriverError("DRIVER.ACTION_FAILED", "backend returned an invalid window")
            windows.append(_json_safe(item))
        return {"windows": windows}

    def _snapshot(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, {"window", "max_depth", "max_nodes"}, "args")
        window = _object(args.get("window"), "window")
        if not window:
            _fail("DRIVER.INVALID_REQUEST", "window must contain an exact selector")
        max_depth = _bounded_integer(
            args.get("max_depth", DEFAULT_MAX_DEPTH), "max_depth", 0, MAX_DEPTH
        )
        max_nodes = _bounded_integer(
            args.get("max_nodes", DEFAULT_MAX_NODES), "max_nodes", 1, MAX_NODES
        )
        record = self._capture(
            window, max_depth=max_depth, max_nodes=max_nodes, deadline=deadline
        )
        return copy.deepcopy(record.public)

    def _capture(
        self,
        window: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> _SnapshotRecord:
        _check_deadline(deadline)
        raw = self.backend.capture(
            window, max_depth=max_depth, max_nodes=max_nodes, deadline=deadline
        )
        _check_deadline(deadline)
        if not isinstance(raw, BackendSnapshot):
            raise DriverError("DRIVER.ACTION_FAILED", "backend returned an invalid snapshot")
        self._revision += 1
        revision = self._revision
        snapshot_id = f"{self.generation}:{revision}"
        nodes: list[dict[str, Any]] = []
        handles: dict[str, Any] = {}
        fingerprints: dict[str, str] = {}
        if len(raw.nodes) > max_nodes:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "backend exceeded the requested node limit",
                data={"max_nodes": max_nodes, "actual": len(raw.nodes)},
            )
        for index, backend_node in enumerate(raw.nodes):
            _check_deadline(deadline)
            if not isinstance(backend_node, BackendNode):
                raise DriverError("DRIVER.ACTION_FAILED", "backend returned an invalid node")
            parent_id: str | None = None
            if backend_node.parent_index is not None:
                if (
                    isinstance(backend_node.parent_index, bool)
                    or not isinstance(backend_node.parent_index, int)
                    or backend_node.parent_index < 0
                    or backend_node.parent_index >= index
                ):
                    raise DriverError(
                        "DRIVER.ACTION_FAILED",
                        "backend returned an invalid parent relationship",
                    )
                parent_id = f"n{backend_node.parent_index}"
            node_id = f"n{index}"
            role = _safe_text(backend_node.role) or "unknown"
            states = {name: None for name in STATE_NAMES}
            if backend_node.states is not None:
                for name in STATE_NAMES:
                    value = backend_node.states.get(name)
                    states[name] = value if isinstance(value, bool) or value is None else None
            actions = sorted(
                {str(item) for item in backend_node.actions if str(item) in NODE_ACTIONS}
            )
            provenance = _json_safe(dict(backend_node.provenance or {}))
            if not isinstance(provenance, dict):
                provenance = {}
            provenance["backend"] = _safe_text(getattr(self.backend, "name", None)) or "unknown"
            node = {
                "node_id": node_id,
                "parent_id": parent_id,
                "role": role,
                "name": _safe_text(backend_node.name),
                "value": _safe_text(backend_node.value),
                "states": states,
                "bounds": _normalize_bounds(backend_node.bounds),
                "actions": actions,
                "provenance": provenance,
            }
            nodes.append(node)
            handles[node_id] = backend_node.native
            fingerprints[node_id] = self._fingerprint(node)
        public = {
            "snapshot_id": snapshot_id,
            "revision": revision,
            "app": _json_safe(raw.app),
            "window": _json_safe(raw.window),
            "nodes": nodes,
            "truncated": bool(raw.truncated),
        }
        if not isinstance(public["app"], dict) or not isinstance(public["window"], dict):
            raise DriverError("DRIVER.ACTION_FAILED", "backend returned invalid app/window identity")
        record = _SnapshotRecord(
            public=public,
            handles=handles,
            fingerprints=fingerprints,
            window_selector=copy.deepcopy(dict(window)),
            max_depth=max_depth,
            max_nodes=max_nodes,
        )
        self._current = record
        return record

    @staticmethod
    def _fingerprint(node: Mapping[str, Any]) -> str:
        provenance = node.get("provenance")
        if not isinstance(provenance, Mapping):
            provenance = {}
        identity = {
            "role": node.get("role"),
            "name": node.get("name"),
            "automation_id": provenance.get("automation_id"),
            "class_name": provenance.get("class_name"),
            "framework_id": provenance.get("framework_id"),
            "process_id": provenance.get("process_id"),
            "native_window_handle": provenance.get("native_window_handle"),
            # RuntimeId is not a public or durable node identifier, but it is
            # useful as one ingredient when detecting replacement between the
            # caller's snapshot and the mandatory pre-dispatch re-snapshot.
            "runtime_id": provenance.get("runtime_id"),
        }
        encoded = json.dumps(
            identity, ensure_ascii=False, allow_nan=False, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        return "sha256:" + hashlib.sha256(encoded).hexdigest()

    def _record(self, snapshot_id: Any, revision: Any) -> _SnapshotRecord:
        if not isinstance(snapshot_id, str) or not snapshot_id:
            _fail("DRIVER.INVALID_REQUEST", "snapshot_id must be a non-empty string")
        if isinstance(revision, bool) or not isinstance(revision, int) or revision < 1:
            _fail("DRIVER.INVALID_REQUEST", "revision must be a positive integer")
        record = self._current
        if (
            record is None
            or record.public["snapshot_id"] != snapshot_id
            or record.public["revision"] != revision
        ):
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "snapshot is not the current driver revision",
                data={
                    "snapshot_id": snapshot_id,
                    "revision": revision,
                    "current_snapshot_id": None if record is None else record.public["snapshot_id"],
                    "current_revision": None if record is None else record.public["revision"],
                },
            )
        return record

    def _find(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, {"snapshot_id", "revision", "locator"}, "args")
        record = self._record(args.get("snapshot_id"), args.get("revision"))
        if record.public.get("truncated"):
            raise DriverError(
                "DRIVER.SNAPSHOT_TRUNCATED",
                "a truncated snapshot cannot prove a unique locator match",
            )
        locator = self._locator(args.get("locator"))
        node = self._resolve(record, locator, deadline)
        return {
            "target": self._target(record, node["node_id"]),
            "node": copy.deepcopy(node),
        }

    def _locator(self, raw: Any) -> dict[str, Any]:
        locator = _object(raw, "locator")
        allowed = {
            "role",
            "name",
            "value",
            "automation_id",
            "class_name",
            "framework_id",
            "states",
            "actions",
            "match",
        }
        _only_keys(locator, allowed, "locator")
        selector_names = set(locator) - {"match"}
        if not selector_names:
            _fail("DRIVER.INVALID_REQUEST", "locator must contain at least one selector")
        normalized: dict[str, Any] = {"match": locator.get("match", "exact")}
        if normalized["match"] != "exact":
            _fail("DRIVER.INVALID_REQUEST", "v0 only supports exact locator matching")
        for name in ("role", "name", "automation_id", "class_name", "framework_id"):
            if name in locator:
                normalized[name] = _text(locator[name], f"locator.{name}")
        if "value" in locator:
            normalized["value"] = _text(locator["value"], "locator.value", nullable=True)
        if "states" in locator:
            states = _object(locator["states"], "locator.states")
            _only_keys(states, set(STATE_NAMES), "locator.states")
            normalized_states: dict[str, bool | None] = {}
            for name, value in states.items():
                if not isinstance(value, bool) and value is not None:
                    _fail("DRIVER.INVALID_REQUEST", f"locator.states.{name} must be boolean or null")
                normalized_states[name] = value
            normalized["states"] = normalized_states
        if "actions" in locator:
            actions = locator["actions"]
            if isinstance(actions, (str, bytes)) or not isinstance(actions, list):
                _fail("DRIVER.INVALID_REQUEST", "locator.actions must be an array")
            normalized_actions: list[str] = []
            for action in actions:
                if not isinstance(action, str) or action not in NODE_ACTIONS:
                    _fail("DRIVER.INVALID_REQUEST", "locator.actions contains an unsupported action")
                if action in normalized_actions:
                    _fail("DRIVER.INVALID_REQUEST", "locator.actions must be unique")
                normalized_actions.append(action)
            normalized["actions"] = normalized_actions
        return normalized

    @staticmethod
    def _string_matches(actual: Any, expected: Any, mode: str) -> bool:
        if actual is None or expected is None:
            return actual is expected
        if not isinstance(actual, str) or not isinstance(expected, str):
            return actual == expected
        return actual == expected

    def _matches(self, node: Mapping[str, Any], locator: Mapping[str, Any]) -> bool:
        mode = str(locator.get("match", "exact"))
        provenance = node.get("provenance")
        if not isinstance(provenance, Mapping):
            provenance = {}
        for name in ("role", "name", "value"):
            if name in locator and not self._string_matches(node.get(name), locator[name], mode):
                return False
        for name in ("automation_id", "class_name", "framework_id"):
            if name in locator and not self._string_matches(provenance.get(name), locator[name], mode):
                return False
        states = locator.get("states", {})
        node_states = node.get("states", {})
        if any(node_states.get(name) is not expected for name, expected in states.items()):
            return False
        required_actions = set(locator.get("actions", ()))
        return required_actions.issubset(set(node.get("actions", ())))

    def _resolve(
        self, record: _SnapshotRecord, locator: Mapping[str, Any], deadline: float
    ) -> dict[str, Any]:
        candidates: list[dict[str, Any]] = []
        for node in record.public["nodes"]:
            _check_deadline(deadline)
            if self._matches(node, locator):
                candidates.append(node)
        if not candidates:
            raise DriverError(
                "DRIVER.NOT_FOUND",
                "locator matched no node",
                data={"locator": dict(locator), "snapshot_id": record.public["snapshot_id"]},
            )
        if len(candidates) > 1:
            summaries = [
                {
                    "node_id": node["node_id"],
                    "role": node["role"],
                    "name": node["name"],
                    "actions": node["actions"],
                }
                for node in candidates[:MAX_CANDIDATE_SUMMARIES]
            ]
            raise DriverError(
                "DRIVER.AMBIGUOUS",
                "locator matched more than one node",
                data={
                    "candidate_count": len(candidates),
                    "candidates": summaries,
                    "snapshot_id": record.public["snapshot_id"],
                },
            )
        return candidates[0]

    @staticmethod
    def _target(record: _SnapshotRecord, node_id: str) -> dict[str, Any]:
        return {
            "snapshot_id": record.public["snapshot_id"],
            "revision": record.public["revision"],
            "node_id": node_id,
        }

    def _write(self, action: str, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        allowed = {"target", "locator"} | ({"value"} if action == "set_value" else set())
        _only_keys(args, allowed, "args")
        if "target" not in args or "locator" not in args:
            _fail("DRIVER.INVALID_REQUEST", "target and locator are required")
        target = _object(args["target"], "target")
        _only_keys(target, {"snapshot_id", "revision", "node_id"}, "target")
        node_id = target.get("node_id")
        if not isinstance(node_id, str) or not node_id:
            _fail("DRIVER.INVALID_REQUEST", "target.node_id must be a non-empty string")
        record = self._record(target.get("snapshot_id"), target.get("revision"))
        if record.public.get("truncated"):
            raise DriverError(
                "DRIVER.SNAPSHOT_TRUNCATED",
                "a truncated snapshot cannot be used for a write action",
            )
        locator = self._locator(args["locator"])
        expected = self._resolve(record, locator, deadline)
        if expected["node_id"] != node_id or node_id not in record.handles:
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "target does not identify the locator result in its snapshot",
                data={"node_id": node_id, "resolved_node_id": expected["node_id"]},
            )
        expected_fingerprint = record.fingerprints[node_id]
        _check_deadline(deadline)
        fresh = self._capture(
            record.window_selector,
            max_depth=record.max_depth,
            max_nodes=record.max_nodes,
            deadline=deadline,
        )
        try:
            resolved = self._resolve(fresh, locator, deadline)
        except DriverError as exc:
            if exc.code in {"DRIVER.NOT_FOUND", "DRIVER.AMBIGUOUS"}:
                raise DriverError(
                    "DRIVER.STALE_SNAPSHOT",
                    "locator no longer resolves to the original unique target",
                    data={"reason": exc.code, **(exc.data if isinstance(exc.data, dict) else {})},
                ) from exc
            raise
        if fresh.public.get("truncated"):
            raise DriverError(
                "DRIVER.SNAPSHOT_TRUNCATED",
                "the pre-dispatch snapshot was truncated",
            )
        fresh_node_id = resolved["node_id"]
        same_element = getattr(self.backend, "same_element", None)
        if callable(same_element):
            if not same_element(
                record.handles[node_id],
                fresh.handles[fresh_node_id],
                deadline=deadline,
            ):
                raise DriverError(
                    "DRIVER.STALE_SNAPSHOT",
                    "locator resolved to a different native target",
                )
        elif (
            record.public["nodes"][int(node_id[1:])]["provenance"].get("runtime_id")
            is None
            or resolved.get("provenance", {}).get("runtime_id") is None
        ):
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "native target identity cannot be verified",
            )
        if fresh.fingerprints[fresh_node_id] != expected_fingerprint:
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "locator resolved to a different semantic target",
                data={
                    "previous_snapshot_id": record.public["snapshot_id"],
                    "current_snapshot_id": fresh.public["snapshot_id"],
                },
            )
        if action not in resolved["actions"]:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                f"target does not support native {action}",
                data={"action": action, "available_actions": resolved["actions"]},
            )
        value: str | None = None
        if action == "set_value":
            if "value" not in args:
                _fail("DRIVER.INVALID_REQUEST", "value is required for set_value")
            value = _text(args["value"], "value")
            provenance = resolved.get("provenance", {})
            if isinstance(provenance, Mapping) and provenance.get("value_redacted") is True:
                raise DriverError(
                    "DRIVER.PROTECTED_ELEMENT",
                    "set_value is disabled for password or protected elements",
                )
        _check_deadline(deadline)
        native = fresh.handles[fresh_node_id]
        dispatched = False
        try:
            dispatched = True
            if action == "focus":
                backend_result = self.backend.focus(native, deadline=deadline)
            elif action == "invoke":
                backend_result = self.backend.invoke(native, deadline=deadline)
            else:
                assert value is not None
                backend_result = self.backend.set_value(native, value, deadline=deadline)
            _check_deadline(deadline, post_dispatch=True)
        except DriverError as exc:
            if dispatched and exc.code in {"DRIVER.ACTION_FAILED", "DRIVER.TIMEOUT"}:
                details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                details.setdefault("action", action)
                details["effect"] = "unknown"
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "native action outcome is unknown after dispatch",
                    retryable=False,
                    data=details,
                ) from exc
            raise
        except Exception as exc:
            raise DriverError(
                "DRIVER.UNKNOWN_EFFECT",
                f"native {action} outcome is unknown after dispatch",
                data={
                    "action": action,
                    "effect": "unknown" if dispatched else "not_applied",
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        finally:
            if dispatched:
                # Any native write may change the tree.  Require a new public
                # snapshot before another target can be used.
                self._current = None
        result = {
            "ok": True,
            "action": action,
            "resolved": self._target(fresh, fresh_node_id),
            "backend_result": _json_safe(backend_result),
        }
        return result


class UnavailableBackend:
    """Backend used when the OS or optional COM dependency is unavailable."""

    name = "windows_uia_unavailable"

    def __init__(self, reason: str, **details: Any) -> None:
        self.reason = reason
        self.details = details

    def _raise(self) -> NoReturn:
        raise DriverError(
            "DRIVER.UNAVAILABLE",
            "Windows UI Automation backend is unavailable",
            data={"reason": self.reason, **_json_safe(self.details)},
        )

    def list_windows(self, *, include_invisible: bool, deadline: float) -> Sequence[Mapping[str, Any]]:
        self._raise()

    def capture(
        self,
        window: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot:
        self._raise()

    def focus(self, native: Any, *, deadline: float) -> Any:
        self._raise()

    def invoke(self, native: Any, *, deadline: float) -> Any:
        self._raise()

    def set_value(self, native: Any, value: str, *, deadline: float) -> Any:
        self._raise()

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool:
        self._raise()


CONTROL_TYPE_ROLES = {
    50000: "button",
    50001: "calendar",
    50002: "checkbox",
    50003: "combobox",
    50004: "edit",
    50005: "link",
    50006: "image",
    50007: "list_item",
    50008: "list",
    50009: "menu",
    50010: "menu_bar",
    50011: "menu_item",
    50012: "progress_bar",
    50013: "radio_button",
    50014: "scroll_bar",
    50015: "slider",
    50016: "spinner",
    50017: "status_bar",
    50018: "tab",
    50019: "tab_item",
    50020: "text",
    50021: "toolbar",
    50022: "tooltip",
    50023: "tree",
    50024: "tree_item",
    50025: "custom",
    50026: "group",
    50027: "thumb",
    50028: "data_grid",
    50029: "data_item",
    50030: "document",
    50031: "split_button",
    50032: "window",
    50033: "pane",
    50034: "header",
    50035: "header_item",
    50036: "table",
    50037: "title_bar",
    50038: "separator",
    50039: "semantic_zoom",
    50040: "app_bar",
}


class ComtypesUIABackend:
    """Thin optional adapter for UIAutomationClient generated by comtypes."""

    name = "windows_uia"

    def __init__(self) -> None:
        if sys.platform != "win32":
            raise DriverError("DRIVER.UNAVAILABLE", "Windows UIA requires Windows")
        try:
            # comtypes reads this before initializing the current thread.
            sys.coinit_flags = 0  # COINIT_MULTITHREADED
            from comtypes.client import CreateObject, GetModule
        except (ImportError, OSError) as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "optional dependency comtypes is not installed or loadable",
                data={"reason": "dependency_missing", "exception_type": type(exc).__name__},
            ) from exc
        try:
            GetModule("UIAutomationCore.dll")
            from comtypes.gen import UIAutomationClient as UIA

            self.UIA = UIA
            self.automation = CreateObject(UIA.CUIAutomation, interface=UIA.IUIAutomation)
            self.walker = self.automation.ControlViewWalker
        except Exception as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "UIAutomationClient could not be initialized",
                data={"reason": "uia_initialization_failed", **self._exception_data(exc)},
            ) from exc

    @staticmethod
    def _exception_data(exc: BaseException) -> dict[str, Any]:
        data: dict[str, Any] = {"exception_type": type(exc).__name__}
        hresult = getattr(exc, "hresult", None)
        if isinstance(hresult, int):
            data["hresult"] = f"0x{hresult & 0xFFFFFFFF:08X}"
        return data

    def _native_failure(self, operation: str, exc: BaseException) -> DriverError:
        hresult = getattr(exc, "hresult", None)
        code = "DRIVER.ACTION_FAILED"
        if isinstance(hresult, int) and hresult & 0xFFFFFFFF == 0x80070005:
            message = f"{operation} was denied by Windows integrity/session boundaries"
        else:
            message = f"native UIA operation {operation} failed"
        return DriverError(code, message, data={"operation": operation, **self._exception_data(exc)})

    @staticmethod
    def _window_text(user32: Any, hwnd: int) -> str:
        length = max(0, int(user32.GetWindowTextLengthW(hwnd)))
        buffer = ctypes.create_unicode_buffer(min(length + 1, MAX_FIELD_CHARS + 1))
        user32.GetWindowTextW(hwnd, buffer, len(buffer))
        return buffer.value

    @staticmethod
    def _class_name(user32: Any, hwnd: int) -> str:
        buffer = ctypes.create_unicode_buffer(512)
        user32.GetClassNameW(hwnd, buffer, len(buffer))
        return buffer.value

    @staticmethod
    def _process_path(pid: int) -> str | None:
        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        kernel32 = ctypes.windll.kernel32
        kernel32.OpenProcess.argtypes = [ctypes.c_ulong, ctypes.c_bool, ctypes.c_ulong]
        kernel32.OpenProcess.restype = ctypes.c_void_p
        kernel32.QueryFullProcessImageNameW.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ulong,
            ctypes.c_wchar_p,
            ctypes.POINTER(ctypes.c_ulong),
        ]
        kernel32.QueryFullProcessImageNameW.restype = ctypes.c_bool
        kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
        kernel32.CloseHandle.restype = ctypes.c_bool
        handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not handle:
            return None
        try:
            size = ctypes.c_ulong(32768)
            buffer = ctypes.create_unicode_buffer(size.value)
            if kernel32.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(size)):
                return buffer.value[:MAX_FIELD_CHARS]
            return None
        finally:
            kernel32.CloseHandle(handle)

    def list_windows(
        self, *, include_invisible: bool, deadline: float
    ) -> Sequence[Mapping[str, Any]]:
        _check_deadline(deadline)
        user32 = ctypes.windll.user32
        windows: list[dict[str, Any]] = []
        callback_type = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)
        user32.EnumWindows.argtypes = [callback_type, ctypes.c_void_p]
        user32.EnumWindows.restype = ctypes.c_bool
        user32.IsWindowVisible.argtypes = [ctypes.c_void_p]
        user32.IsWindowVisible.restype = ctypes.c_bool
        user32.IsWindowEnabled.argtypes = [ctypes.c_void_p]
        user32.IsWindowEnabled.restype = ctypes.c_bool
        user32.GetWindowTextLengthW.argtypes = [ctypes.c_void_p]
        user32.GetWindowTextLengthW.restype = ctypes.c_int
        user32.GetWindowTextW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_int]
        user32.GetWindowTextW.restype = ctypes.c_int
        user32.GetClassNameW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_int]
        user32.GetClassNameW.restype = ctypes.c_int
        user32.GetWindowThreadProcessId.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_ulong),
        ]
        user32.GetWindowThreadProcessId.restype = ctypes.c_ulong
        timed_out = False

        def visit(raw_hwnd: Any, _parameter: Any) -> bool:
            nonlocal timed_out
            # Exceptions raised from a ctypes callback are swallowed by
            # ctypes, so stop enumeration explicitly and raise afterwards.
            if time.monotonic() >= deadline:
                timed_out = True
                return False
            hwnd = int(raw_hwnd or 0)
            visible = bool(user32.IsWindowVisible(hwnd))
            if not include_invisible and not visible:
                return True
            pid = ctypes.c_ulong()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
            title = self._window_text(user32, hwnd)
            class_name = self._class_name(user32, hwnd)
            windows.append(
                {
                    "app": {
                        "process_id": int(pid.value),
                        "executable": self._process_path(int(pid.value)),
                    },
                    "window": {
                        "handle": hwnd,
                        "title": title,
                        "class_name": class_name,
                        "process_id": int(pid.value),
                        "visible": visible,
                        "enabled": bool(user32.IsWindowEnabled(hwnd)),
                    },
                }
            )
            return True

        try:
            callback = callback_type(visit)
            if not user32.EnumWindows(callback, 0):
                if timed_out:
                    _check_deadline(deadline)
                raise ctypes.WinError()
        except DriverError:
            raise
        except Exception as exc:
            raise self._native_failure("EnumWindows", exc) from exc
        _check_deadline(deadline)
        return windows

    def _resolve_window(
        self, selector: Mapping[str, Any], *, deadline: float
    ) -> Mapping[str, Any]:
        allowed = {"handle", "title", "class_name", "process_id"}
        unknown = sorted(set(selector) - allowed)
        if unknown or not selector:
            _fail(
                "DRIVER.INVALID_REQUEST",
                "window selector supports handle, title, class_name and process_id",
                fields=unknown,
            )
        candidates = []
        for item in self.list_windows(include_invisible=True, deadline=deadline):
            window = item["window"]
            if all(window.get(key) == value for key, value in selector.items()):
                candidates.append(item)
        if not candidates:
            raise DriverError(
                "DRIVER.NOT_FOUND", "window selector matched no top-level window", data={"window": dict(selector)}
            )
        if len(candidates) > 1:
            raise DriverError(
                "DRIVER.AMBIGUOUS",
                "window selector matched more than one top-level window",
                data={"candidate_count": len(candidates)},
            )
        return candidates[0]

    @staticmethod
    def _property(element: Any, name: str, default: Any = None) -> Any:
        try:
            return getattr(element, name)
        except Exception:
            return default

    def _pattern_available(self, element: Any, property_name: str) -> bool:
        property_id = getattr(self.UIA, property_name, None)
        if property_id is None:
            return False
        try:
            return bool(element.GetCurrentPropertyValue(property_id))
        except Exception:
            return False

    def _pattern(self, element: Any, pattern_name: str, interface_name: str) -> Any:
        pattern_id = getattr(self.UIA, pattern_name)
        interface = getattr(self.UIA, interface_name)
        try:
            raw = element.GetCurrentPattern(pattern_id)
            if not raw:
                raise ValueError("pattern unavailable")
            return raw.QueryInterface(interface)
        except Exception as exc:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                f"native UIA pattern {pattern_name} is unavailable",
                data={"pattern": pattern_name, **self._exception_data(exc)},
            ) from exc

    @staticmethod
    def _bounds(rectangle: Any) -> dict[str, int] | None:
        try:
            left = int(rectangle.left)
            top = int(rectangle.top)
            right = int(rectangle.right)
            bottom = int(rectangle.bottom)
        except (AttributeError, TypeError, ValueError, OverflowError):
            try:
                left, top, right, bottom = (int(item) for item in rectangle)
            except (TypeError, ValueError, OverflowError):
                return None
        return {"x": left, "y": top, "width": max(0, right - left), "height": max(0, bottom - top)}

    def _read_node(self, element: Any, parent_index: int | None) -> BackendNode:
        control_type = self._property(element, "CurrentControlType")
        try:
            control_type_id = int(control_type)
        except (TypeError, ValueError, OverflowError):
            control_type_id = 0
        enabled = bool(self._property(element, "CurrentIsEnabled", False))
        focusable = bool(self._property(element, "CurrentIsKeyboardFocusable", False))
        invoke_available = self._pattern_available(
            element, "UIA_IsInvokePatternAvailablePropertyId"
        )
        value_available = self._pattern_available(
            element, "UIA_IsValuePatternAvailablePropertyId"
        )
        is_password = bool(self._property(element, "CurrentIsPassword", False))
        value: str | None = None
        read_only: bool | None = None
        if value_available:
            try:
                value_pattern = self._pattern(
                    element, "UIA_ValuePatternId", "IUIAutomationValuePattern"
                )
                read_only = bool(value_pattern.CurrentIsReadOnly)
                if not is_password:
                    value = _safe_text(value_pattern.CurrentValue)
            except DriverError:
                value_available = False
                read_only = None
        actions: list[str] = []
        if enabled and focusable:
            actions.append("focus")
        if enabled and invoke_available:
            actions.append("invoke")
        if enabled and value_available and read_only is False:
            actions.append("set_value")
        runtime_id: list[int] | None = None
        try:
            runtime_id = [int(item) for item in element.GetRuntimeId()][:32]
        except Exception:
            pass
        return BackendNode(
            native=element,
            parent_index=parent_index,
            role=CONTROL_TYPE_ROLES.get(control_type_id, "unknown"),
            name=_safe_text(self._property(element, "CurrentName")),
            value=value,
            states={
                "enabled": enabled,
                "offscreen": bool(self._property(element, "CurrentIsOffscreen", False)),
                "focusable": focusable,
                "focused": bool(self._property(element, "CurrentHasKeyboardFocus", False)),
                "read_only": read_only,
            },
            bounds=self._bounds(self._property(element, "CurrentBoundingRectangle")),
            actions=actions,
            provenance={
                "control_type_id": control_type_id,
                "automation_id": _safe_text(self._property(element, "CurrentAutomationId")),
                "class_name": _safe_text(self._property(element, "CurrentClassName")),
                "framework_id": _safe_text(self._property(element, "CurrentFrameworkId")),
                "process_id": self._property(element, "CurrentProcessId"),
                "native_window_handle": self._property(element, "CurrentNativeWindowHandle"),
                "runtime_id": runtime_id,
                "value_redacted": is_password,
                "coordinate_space": "physical_screen_pixels",
            },
        )

    def capture(
        self,
        window: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot:
        selected = self._resolve_window(window, deadline=deadline)
        hwnd = selected["window"]["handle"]
        try:
            root = self.automation.ElementFromHandle(hwnd)
            queue: deque[tuple[Any, int | None, int]] = deque([(root, None, 0)])
            nodes: list[BackendNode] = []
            truncated = False
            while queue:
                _check_deadline(deadline)
                if len(nodes) >= max_nodes:
                    truncated = True
                    break
                element, parent_index, depth = queue.popleft()
                current_index = len(nodes)
                nodes.append(self._read_node(element, parent_index))
                if depth >= max_depth:
                    try:
                        if self.walker.GetFirstChildElement(element):
                            truncated = True
                    except Exception:
                        truncated = True
                    continue
                child = self.walker.GetFirstChildElement(element)
                while child:
                    _check_deadline(deadline)
                    queue.append((child, current_index, depth + 1))
                    if len(nodes) + len(queue) >= max_nodes:
                        truncated = True
                        break
                    child = self.walker.GetNextSiblingElement(child)
            return BackendSnapshot(
                app=selected["app"],
                window=selected["window"],
                nodes=nodes,
                truncated=truncated,
            )
        except DriverError:
            raise
        except Exception as exc:
            raise self._native_failure("snapshot", exc) from exc

    def focus(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        try:
            native.SetFocus()
            return {"native_pattern": "SetFocus"}
        except Exception as exc:
            raise self._native_failure("SetFocus", exc) from exc

    def invoke(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        pattern = self._pattern(native, "UIA_InvokePatternId", "IUIAutomationInvokePattern")
        _check_deadline(deadline)
        try:
            pattern.Invoke()
            return {"native_pattern": "InvokePattern"}
        except Exception as exc:
            raise self._native_failure("InvokePattern.Invoke", exc) from exc

    def set_value(self, native: Any, value: str, *, deadline: float) -> Any:
        _check_deadline(deadline)
        pattern = self._pattern(native, "UIA_ValuePatternId", "IUIAutomationValuePattern")
        try:
            if bool(pattern.CurrentIsReadOnly):
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "ValuePattern target is read-only",
                    data={"pattern": "ValuePattern"},
                )
            _check_deadline(deadline)
            pattern.SetValue(value)
            return {"native_pattern": "ValuePattern"}
        except DriverError:
            raise
        except Exception as exc:
            raise self._native_failure("ValuePattern.SetValue", exc) from exc

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool:
        _check_deadline(deadline)
        try:
            return bool(self.automation.CompareElements(previous, current))
        except Exception as exc:
            raise self._native_failure("CompareElements", exc) from exc


def create_default_backend() -> UIABackend:
    if sys.platform != "win32":
        return UnavailableBackend("platform", platform=sys.platform, required_platform="win32")
    try:
        return ComtypesUIABackend()
    except DriverError as exc:
        return UnavailableBackend(
            "initialization",
            cause_code=exc.code,
            cause_data=exc.data if isinstance(exc.data, dict) else {},
        )


def _wire_deadline(value: Any) -> float:
    if value is None:
        return time.monotonic() + DEFAULT_REQUEST_SECONDS
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        _fail("DRIVER.INVALID_REQUEST", "deadline_ms must be a Unix timestamp in milliseconds")
    remaining = float(value) / 1000.0 - time.time()
    if not math.isfinite(remaining) or remaining <= 0:
        raise DriverError(
            "DRIVER.TIMEOUT",
            "request deadline elapsed before dispatch",
            retryable=True,
            data={"phase": "before_dispatch", "effect": "not_applied"},
        )
    return time.monotonic() + remaining


def _encode(message: Mapping[str, Any]) -> bytes:
    return (
        json.dumps(
            message,
            ensure_ascii=True,
            allow_nan=False,
            separators=(",", ":"),
        )
        + "\n"
    ).encode("utf-8")


def emit(message: dict[str, Any]) -> None:
    encoded = _encode(message)
    if len(encoded) > MAX_RESPONSE_BYTES:
        encoded = _encode(
            {
                "id": message.get("id"),
                "error": {
                    "code": "DRIVER.OUTPUT_TOO_LARGE",
                    "message": "normalized response exceeds the NDJSON frame limit",
                    "retryable": False,
                    "data": {"limit_bytes": MAX_RESPONSE_BYTES},
                },
            }
        )
    sys.stdout.buffer.write(encoded)
    sys.stdout.buffer.flush()


def debug(message: str) -> None:
    encoded = f"[{PLUGIN_NAME}] {message}\n".encode("utf-8", errors="replace")
    sys.stderr.buffer.write(encoded)
    sys.stderr.buffer.flush()


def emit_error(request_id: Any, error: DriverError) -> None:
    payload: dict[str, Any] = {
        "code": error.code,
        "message": error.message,
        "retryable": error.retryable,
    }
    if error.data is not None:
        payload["data"] = _json_safe(error.data)
    emit({"id": request_id, "error": payload})


def handle_request(request: Any, driver: WindowsUIADriver) -> None:
    request_id: Any = request.get("id") if isinstance(request, dict) else None
    try:
        if not isinstance(request, dict):
            raise DriverError("PROTOCOL.INVALID_REQUEST", "request must be a JSON object")
        if not isinstance(request_id, str) or not request_id or len(request_id) > 256:
            raise DriverError("PROTOCOL.INVALID_REQUEST", "request.id must be a bounded non-empty string")
        request_type = request.get("type")
        if request_type == "manifest":
            emit({"id": request_id, "result": MANIFEST})
            return
        action_id = request.get("action")
        if request_type != "invoke" or action_id not in ACTION_NAMES:
            raise DriverError(
                "PROTOCOL.ACTION_NOT_FOUND",
                f"unknown action: {action_id}",
                data={"action": action_id, "available_actions": list(ACTION_NAMES)},
            )
        deadline = _wire_deadline(request.get("deadline_ms"))
        result = driver.execute(
            ACTION_NAMES[action_id], request.get("args"), deadline=deadline
        )
        emit({"id": request_id, "result": result})
    except DriverError as exc:
        debug(f"request failed code={exc.code}")
        emit_error(request_id, exc)
    except Exception as exc:
        debug(f"internal error type={type(exc).__name__}")
        emit_error(
            request_id,
            DriverError(
                "DRIVER.ACTION_FAILED",
                "Windows UIA driver encountered an internal error",
                data={"exception_type": type(exc).__name__},
            ),
        )


def _discard_until_newline(stream: Any) -> None:
    while True:
        chunk = stream.readline(MAX_REQUEST_BYTES + 1)
        if not chunk or chunk.endswith(b"\n"):
            return


def serve(driver: WindowsUIADriver, stream: Any = None) -> None:
    binary_stream = sys.stdin.buffer if stream is None else stream
    while True:
        raw = binary_stream.readline(MAX_REQUEST_BYTES + 1)
        if not raw:
            return
        if len(raw) > MAX_REQUEST_BYTES:
            if not raw.endswith(b"\n"):
                _discard_until_newline(binary_stream)
            emit_error(
                None,
                DriverError(
                    "PROTOCOL.REQUEST_TOO_LARGE",
                    "request exceeds the NDJSON frame limit",
                    data={"limit_bytes": MAX_REQUEST_BYTES},
                ),
            )
            continue
        try:
            line = raw.decode("utf-8", errors="strict").strip()
        except UnicodeDecodeError as exc:
            emit_error(
                None,
                DriverError(
                    "PROTOCOL.INVALID_ENCODING",
                    "request must be valid UTF-8",
                    data={"start": exc.start},
                ),
            )
            continue
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError as exc:
            emit_error(
                None,
                DriverError(
                    "PROTOCOL.PARSE_ERROR",
                    "request line is not valid JSON",
                    data={"line": exc.lineno, "column": exc.colno},
                ),
            )
            continue
        handle_request(request, driver)


def main() -> int:
    driver = WindowsUIADriver()
    if "--manifest" in sys.argv[1:]:
        emit({"type": "manifest", "manifest": MANIFEST})
    debug(f"started pid={os.getpid()} platform={sys.platform}")
    serve(driver)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
