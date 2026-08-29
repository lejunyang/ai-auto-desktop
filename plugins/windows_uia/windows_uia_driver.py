#!/usr/bin/env python3
"""Windows UI Automation process driver.

The process boundary speaks the repository's UTF-8 NDJSON protocol.  The
driver core is deliberately independent from COM so its locator, snapshot and
stale-target rules can be exercised with a fake backend on every platform.

The only input-injection operations are the explicit ``type_text`` and
``pointer_click`` actions.  They use bounded Win32 ``SendInput`` keyboard or
mouse batches after UIA target verification.  The driver never falls back to
either input path from other actions, never captures pixels, and never invokes
OCR.
"""

from __future__ import annotations

from collections import deque
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
import copy
import ctypes
from ctypes import wintypes
import hashlib
import json
import math
import os
import sys
import threading
import time
from typing import Any, Callable, NoReturn, Protocol
import unicodedata
import uuid


PLUGIN_NAME = "desktop.windows_uia"
PLUGIN_VERSION = "0.1.0"
MAX_REQUEST_BYTES = 1024 * 1024
MAX_RESPONSE_BYTES = 8 * 1024 * 1024 - 1
MAX_FIELD_CHARS = 4096
MAX_TYPE_TEXT_CHARS = 1024
MAX_TYPE_TEXT_UTF16_UNITS = MAX_TYPE_TEXT_CHARS * 2
DEFAULT_REQUEST_SECONDS = 30.0
DEFAULT_MAX_DEPTH = 32
DEFAULT_MAX_NODES = 1000
MAX_DEPTH = 128
MAX_NODES = 5000
MAX_CANDIDATE_SUMMARIES = 10

# Capture session limits.  The buffer is bounded because a session the operator
# forgets to stop must not grow without limit; overflow is reported rather than
# hidden, since a silently dropped event is indistinguishable from no
# interaction at all.
# UIA event ids mapped to the recorder vocabulary.  Numeric literals rather
# than UIA module attributes, so the mapping stays testable on hosts without
# comtypes.  Measured against a real window: Invoked=20009, TextChanged=20015,
# ElementSelected=20012, and the Value property is 30045.
_CAPTURE_EVENT_KINDS = {
    20009: "invoked",
    20015: "value_changed",
    20012: "selection_changed",
}
UIA_VALUE_PROPERTY_ID = 30045

MAX_CAPTURE_EVENTS = 512
MAX_CAPTURE_SESSIONS = 4
MAX_CAPTURE_POLL_EVENTS = 128
CAPTURE_SESSION_SECONDS = 3600.0

# Interactions measured to raise no accessibility event whatsoever.  Reported to
# the caller so the recorder UI can say "nothing recordable here" instead of
# dropping the interaction silently.
CAPTURE_BLIND_SPOTS = (
    {
        "kind": "non_focusable_click",
        "detail": (
            "Clicking a control that cannot take focus raises no event; such "
            "controls do not respond to the click at all."
        ),
    },
    {
        "kind": "pointer_motion",
        "detail": (
            "Hover and pointer motion raise no event and are deliberately not "
            "recorded; they are not replayable as a semantic action."
        ),
    },
)

ACTION_IDS = {
    name: f"{PLUGIN_NAME}.{name}@1"
    for name in (
        "list_windows",
        "snapshot",
        "find",
        "focus",
        "invoke",
        "set_value",
        "type_text",
        "pointer_click",
        "capture_start",
        "capture_poll",
        "capture_stop",
    )
}
ACTION_NAMES = {full_name: short_name for short_name, full_name in ACTION_IDS.items()}
WRITE_ACTIONS = frozenset({"focus", "invoke", "set_value", "type_text", "pointer_click"})
CAPTURE_ACTIONS = frozenset({"capture_start", "capture_poll", "capture_stop"})
NODE_ACTIONS = frozenset({"focus", "invoke", "set_value", "type_text", "pointer_click"})
STATE_NAMES = ("enabled", "offscreen", "focusable", "focused", "read_only")

INPUT_MOUSE = 0
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_VIRTUALDESK = 0x4000
MOUSEEVENTF_ABSOLUTE = 0x8000

SM_XVIRTUALSCREEN = 76
SM_YVIRTUALSCREEN = 77
SM_CXVIRTUALSCREEN = 78
SM_CYVIRTUALSCREEN = 79

POINTER_BUTTONS = frozenset({"left"})
POINTER_POSITIONS = frozenset({"center"})


class _MouseInput(ctypes.Structure):
    # INPUT is a tagged union; this ABI member is required for correct struct
    # size/alignment for explicit pointer SendInput batches.
    _fields_ = [
        ("dx", ctypes.c_int32),
        ("dy", ctypes.c_int32),
        ("mouseData", ctypes.c_uint32),
        ("dwFlags", ctypes.c_uint32),
        ("time", ctypes.c_uint32),
        ("dwExtraInfo", ctypes.c_size_t),
    ]


class _KeyboardInput(ctypes.Structure):
    _fields_ = [
        ("wVk", ctypes.c_uint16),
        ("wScan", ctypes.c_uint16),
        ("dwFlags", ctypes.c_uint32),
        ("time", ctypes.c_uint32),
        ("dwExtraInfo", ctypes.c_size_t),
    ]


class _HardwareInput(ctypes.Structure):
    _fields_ = [
        ("uMsg", ctypes.c_uint32),
        ("wParamL", ctypes.c_uint16),
        ("wParamH", ctypes.c_uint16),
    ]


class _InputPayload(ctypes.Union):
    _fields_ = [
        ("mi", _MouseInput),
        ("ki", _KeyboardInput),
        ("hi", _HardwareInput),
    ]


class _Input(ctypes.Structure):
    _anonymous_ = ("payload",)
    _fields_ = [("type", ctypes.c_uint32), ("payload", _InputPayload)]


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
    ("DRIVER.ACTION_UNSUPPORTED", "The target lacks the required native operation.", False),
    ("DRIVER.PROTECTED_ELEMENT", "The target exposes protected content.", False),
    ("DRIVER.UNKNOWN_EFFECT", "The native action may have taken effect.", False),
)
TYPE_TEXT_ERRORS = tuple(
    (code, description, False)
    for code, description, _retryable in COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS
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
POINTER_CLICK_INPUT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["target", "locator"],
    "properties": {
        "target": TARGET_SCHEMA,
        "locator": LOCATOR_SCHEMA,
        "button": {"enum": sorted(POINTER_BUTTONS)},
        "position": {"enum": sorted(POINTER_POSITIONS)},
    },
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

CAPTURE_ERRORS: tuple[tuple[str, str, bool], ...] = (
    ("DRIVER.CAPTURE_UNSUPPORTED", "Event capture is unavailable on this host.", False),
    ("DRIVER.CAPTURE_NOT_FOUND", "The capture session does not exist.", False),
    ("DRIVER.CAPTURE_LIMIT", "Too many concurrent capture sessions.", False),
)

# An observed interaction.  There is deliberately no field for the element's
# value: the value is readable, but recording it would persist whatever the user
# typed, so the recorder captures only that a change occurred.
CAPTURE_EVENT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["kind", "sequence", "element"],
    "properties": {
        "kind": {
            "enum": ["focus_changed", "invoked", "value_changed", "selection_changed"]
        },
        "sequence": {"type": "integer", "minimum": 0},
        "element": {
            "type": "object",
            "required": ["role_id", "name", "class_name", "automation_id"],
            "properties": {
                "role_id": {"type": ["integer", "null"]},
                "name": {"type": ["string", "null"]},
                "class_name": {"type": ["string", "null"]},
                "automation_id": {"type": ["string", "null"]},
                "framework_id": {"type": ["string", "null"]},
                "process_id": {"type": ["integer", "null"]},
            },
            "additionalProperties": False,
        },
    },
    "additionalProperties": False,
}

CAPTURE_BLIND_SPOT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["kind", "detail"],
    "properties": {"kind": {"type": "string"}, "detail": {"type": "string"}},
    "additionalProperties": False,
}

CAPTURE_SESSION_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["session_id", "window", "blind_spots"],
    "properties": {
        "session_id": {"type": "string"},
        "window": {"type": "object"},
        "blind_spots": {"type": "array", "items": CAPTURE_BLIND_SPOT_SCHEMA},
    },
    "additionalProperties": False,
}

CAPTURE_POLL_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["session_id", "events", "dropped_events", "active"],
    "properties": {
        "session_id": {"type": "string"},
        "events": {"type": "array", "items": CAPTURE_EVENT_SCHEMA},
        "dropped_events": {"type": "integer", "minimum": 0},
        "active": {"type": "boolean"},
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
    "type_text": _contract(
        (
            "Explicitly focus a freshly re-resolved UIA target and inject bounded "
            "ordinary text with Win32 SendInput Unicode keyboard events."
        ),
        effect="contextual",
        risk_category="input",
        risk_level="high",
        input_schema={
            "type": "object",
            "required": ["target", "locator", "text"],
            "properties": {
                "target": TARGET_SCHEMA,
                "locator": LOCATOR_SCHEMA,
                "text": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_TYPE_TEXT_CHARS,
                    "description": (
                        "Non-empty UTF-16-encodable text without Unicode control "
                        "or non-character code points."
                    ),
                },
            },
            "additionalProperties": False,
        },
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=TYPE_TEXT_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "pointer_click": _contract(
        (
            "Explicitly re-resolve a current UIA target and submit one Win32 "
            "SendInput absolute mouse batch at the target center."
        ),
        effect="non_idempotent",
        risk_category="modify",
        risk_level="high",
        input_schema=POINTER_CLICK_INPUT_SCHEMA,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "capture_start": _contract(
        (
            "Subscribe to accessibility events for one window so interactions "
            "can be observed.  Reads no element values and injects no input."
        ),
        effect="contextual",
        risk_category="observe",
        risk_level="medium",
        input_schema={
            "type": "object",
            "required": ["window"],
            "properties": {"window": {"type": "object", "minProperties": 1}},
            "additionalProperties": False,
        },
        output_schema=CAPTURE_SESSION_SCHEMA,
        errors=COMMON_ERRORS + CAPTURE_ERRORS,
        permissions=("desktop.observe",),
    ),
    "capture_poll": _contract(
        (
            "Drain buffered interaction events for one capture session.  Never "
            "returns element values; reports dropped events and known blind "
            "spots instead of hiding them."
        ),
        effect="contextual",
        risk_category="observe",
        risk_level="low",
        input_schema={
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "minLength": 1, "maxLength": 64},
                "max_events": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_CAPTURE_POLL_EVENTS,
                },
            },
            "additionalProperties": False,
        },
        output_schema=CAPTURE_POLL_SCHEMA,
        errors=COMMON_ERRORS + CAPTURE_ERRORS,
        permissions=("desktop.observe",),
    ),
    "capture_stop": _contract(
        "Unsubscribe one capture session and release its native handlers.",
        effect="contextual",
        risk_category="observe",
        risk_level="low",
        input_schema={
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "minLength": 1, "maxLength": 64}
            },
            "additionalProperties": False,
        },
        output_schema={
            "type": "object",
            "required": ["session_id", "stopped", "dropped_events"],
            "properties": {
                "session_id": {"type": "string"},
                "stopped": {"type": "boolean"},
                "dropped_events": {"type": "integer", "minimum": 0},
            },
            "additionalProperties": False,
        },
        errors=COMMON_ERRORS + CAPTURE_ERRORS,
        permissions=("desktop.observe",),
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

    def type_text(
        self,
        native: Any,
        text: str,
        *,
        window_handle: int,
        deadline: float,
    ) -> Any: ...

    def pointer_click(
        self,
        native: Any,
        *,
        target_process_id: int,
        window_handle: int,
        x: int,
        y: int,
        deadline: float,
    ) -> Any: ...

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool: ...

    def subscribe(
        self,
        window: Mapping[str, Any],
        sink: "CaptureSink",
        *,
        deadline: float,
    ) -> Any: ...

    def unsubscribe(self, subscription: Any, *, deadline: float) -> None: ...


class CaptureSink:
    """Thread-safe buffer between native event callbacks and the request loop.

    In the driver's MTA apartment, UIA delivers callbacks on COM RPC worker
    threads (measured: callbacks arrived on a thread distinct from the main
    one), concurrently with request handling.  Every field is therefore guarded
    by the lock.

    The buffer is bounded.  When it overflows the oldest events are discarded
    and counted, because a silently dropped event is indistinguishable from the
    user not interacting at all -- the exact failure mode this design has to
    avoid.
    """

    __slots__ = ("_lock", "_events", "_dropped", "_sequence", "_limit")

    def __init__(self, limit: int = MAX_CAPTURE_EVENTS) -> None:
        self._lock = threading.Lock()
        self._events: deque[dict[str, Any]] = deque()
        self._dropped = 0
        self._sequence = 0
        self._limit = limit

    def emit(self, kind: str, element: Mapping[str, Any] | None) -> None:
        """Record one observed interaction.

        Called from native callback threads.  Must never raise into the COM
        caller: a handler that throws can tear down the subscription.
        """

        try:
            record = {
                "kind": kind,
                "element": _capture_element(element),
            }
        except Exception:
            # An element that vanished mid-callback must not kill the session.
            record = {"kind": kind, "element": _capture_element(None)}
        with self._lock:
            record["sequence"] = self._sequence
            self._sequence += 1
            self._events.append(record)
            while len(self._events) > self._limit:
                self._events.popleft()
                self._dropped += 1

    def drain(self, max_events: int) -> tuple[list[dict[str, Any]], int]:
        with self._lock:
            taken = [self._events.popleft()
                     for _ in range(min(max_events, len(self._events)))]
            dropped = self._dropped
            self._dropped = 0
        return taken, dropped

    @property
    def dropped(self) -> int:
        with self._lock:
            return self._dropped


def _capture_element(element: Mapping[str, Any] | None) -> dict[str, Any]:
    """Normalize an event element to identity fields only.

    Value is deliberately absent.  It is readable -- measured: value changes
    raise TextChanged plus a Value property change, and the new text can be read
    back -- but persisting it would store whatever the user typed.  The recorder
    keeps only what a locator needs.
    """

    source: Mapping[str, Any] = element if isinstance(element, Mapping) else {}

    def field(name: str) -> Any:
        value = source.get(name)
        if isinstance(value, str):
            return value[:MAX_FIELD_CHARS]
        if isinstance(value, int) and not isinstance(value, bool):
            return value
        return None

    return {
        "role_id": field("role_id"),
        "name": field("name"),
        "class_name": field("class_name"),
        "automation_id": field("automation_id"),
        "framework_id": field("framework_id"),
        "process_id": field("process_id"),
    }


@dataclass(slots=True)
class _CaptureSession:
    session_id: str
    window: dict[str, Any]
    sink: CaptureSink
    subscription: Any
    expires_at: float
    active: bool = True


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


def _type_text(value: Any) -> str:
    if not isinstance(value, str):
        _fail("DRIVER.INVALID_REQUEST", "text must be a string")
    if not value:
        _fail("DRIVER.INVALID_REQUEST", "text must not be empty")
    if len(value) > MAX_TYPE_TEXT_CHARS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"text exceeds {MAX_TYPE_TEXT_CHARS} characters",
        )
    for index, character in enumerate(value):
        category = unicodedata.category(character)
        if category.startswith("C"):
            _fail(
                "DRIVER.INVALID_REQUEST",
                "text must not contain Unicode control or non-character code points",
                index=index,
                category=category,
            )
    try:
        encoded = value.encode("utf-16-le", errors="strict")
    except UnicodeEncodeError as exc:
        _fail(
            "DRIVER.INVALID_REQUEST",
            "text must be encodable as well-formed UTF-16",
            index=exc.start,
        )
    units = len(encoded) // 2
    if units > MAX_TYPE_TEXT_UTF16_UNITS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"text exceeds {MAX_TYPE_TEXT_UTF16_UNITS} UTF-16 code units",
        )
    return value


def _pointer_button(value: Any) -> str:
    if value is None:
        return "left"
    if not isinstance(value, str) or value not in POINTER_BUTTONS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"pointer_click.button only supports {sorted(POINTER_BUTTONS)!r}",
        )
    return value


def _pointer_position(value: Any) -> str:
    if value is None:
        return "center"
    if not isinstance(value, str) or value not in POINTER_POSITIONS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"pointer_click.position only supports {sorted(POINTER_POSITIONS)!r}",
        )
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


def _center_point(bounds: Mapping[str, Any]) -> tuple[int, int]:
    try:
        x = int(bounds["x"])
        y = int(bounds["y"])
        width = int(bounds["width"])
        height = int(bounds["height"])
    except (KeyError, TypeError, ValueError, OverflowError) as exc:
        raise DriverError(
            "DRIVER.ACTION_UNSUPPORTED",
            "pointer_click target lacks valid bounds",
        ) from exc
    if width <= 0 or height <= 0:
        raise DriverError(
            "DRIVER.ACTION_UNSUPPORTED",
            "pointer_click requires positive-area target bounds",
            data={"bounds": _json_safe(dict(bounds))},
        )
    return (x + width // 2, y + height // 2)


class UnicodeSendInputAdapter:
    """Bounded Win32 keyboard and mouse SendInput helpers."""

    def __init__(
        self,
        send_input: Any = None,
        get_last_error: Any = None,
        get_system_metrics: Any = None,
    ) -> None:
        if send_input is None:
            if sys.platform != "win32":
                raise DriverError(
                    "DRIVER.UNAVAILABLE",
                    "Win32 SendInput requires Windows",
                    data={"reason": "platform", "platform": sys.platform},
                )
            user32 = ctypes.WinDLL("user32", use_last_error=True)
            user32.SendInput.argtypes = [
                ctypes.c_uint,
                ctypes.POINTER(_Input),
                ctypes.c_int,
            ]
            user32.SendInput.restype = ctypes.c_uint
            user32.GetForegroundWindow.argtypes = []
            user32.GetForegroundWindow.restype = ctypes.c_void_p
            user32.GetWindowThreadProcessId.argtypes = [
                ctypes.c_void_p,
                ctypes.POINTER(ctypes.c_uint32),
            ]
            user32.GetWindowThreadProcessId.restype = ctypes.c_uint32
            user32.GetSystemMetrics.argtypes = [ctypes.c_int]
            user32.GetSystemMetrics.restype = ctypes.c_int
            send_input = user32.SendInput
            get_last_error = ctypes.get_last_error
            get_system_metrics = user32.GetSystemMetrics
            self._user32 = user32
        self._send_input = send_input
        self._get_last_error = get_last_error or (lambda: 0)
        self._get_system_metrics = get_system_metrics

    def foreground_window_identity(self) -> tuple[int, int]:
        user32 = getattr(self, "_user32", None)
        if user32 is None:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "foreground-window verification is unavailable",
                retryable=False,
                data={
                    "operation": "GetForegroundWindow",
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                    "events_submitted": 0,
                },
            )
        try:
            hwnd = user32.GetForegroundWindow()
            process_id = ctypes.c_uint32()
            thread_id = (
                int(user32.GetWindowThreadProcessId(hwnd, ctypes.byref(process_id)))
                if hwnd
                else 0
            )
        except Exception as exc:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "foreground-window verification failed before SendInput",
                retryable=False,
                data={
                    "operation": "GetForegroundWindow",
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                    "events_submitted": 0,
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        if not hwnd or thread_id <= 0 or process_id.value <= 0:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "Windows did not report a valid foreground window process",
                retryable=False,
                data={
                    "operation": "GetForegroundWindow",
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                    "events_submitted": 0,
                },
            )
        return int(hwnd), int(process_id.value)

    def virtual_screen_metrics(self) -> tuple[int, int, int, int]:
        get_system_metrics = self._get_system_metrics
        if get_system_metrics is None:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "virtual desktop metrics are unavailable",
                retryable=False,
                data={
                    "operation": "GetSystemMetrics",
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                    "events_submitted": 0,
                },
            )
        try:
            left = int(get_system_metrics(SM_XVIRTUALSCREEN))
            top = int(get_system_metrics(SM_YVIRTUALSCREEN))
            width = int(get_system_metrics(SM_CXVIRTUALSCREEN))
            height = int(get_system_metrics(SM_CYVIRTUALSCREEN))
        except Exception as exc:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "virtual desktop metrics lookup failed before SendInput",
                retryable=False,
                data={
                    "operation": "GetSystemMetrics",
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                    "events_submitted": 0,
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        if width <= 0 or height <= 0:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "Windows did not report a valid virtual desktop",
                retryable=False,
                data={
                    "operation": "GetSystemMetrics",
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                    "events_submitted": 0,
                },
            )
        return left, top, width, height

    @staticmethod
    def _utf16_units(text: str) -> list[int]:
        encoded = text.encode("utf-16-le", errors="strict")
        return [
            int.from_bytes(encoded[index : index + 2], "little")
            for index in range(0, len(encoded), 2)
        ]

    @staticmethod
    def _keyboard_event(unit: int, *, key_up: bool) -> _Input:
        flags = KEYEVENTF_UNICODE | (KEYEVENTF_KEYUP if key_up else 0)
        return _Input(
            type=INPUT_KEYBOARD,
            payload=_InputPayload(
                ki=_KeyboardInput(
                    wVk=0,
                    wScan=unit,
                    dwFlags=flags,
                    time=0,
                    dwExtraInfo=0,
                )
            ),
        )

    @staticmethod
    def _absolute_coordinate(value: int, offset: int, span: int) -> int:
        if span <= 1:
            return 0
        if value <= offset:
            return 0
        if value >= offset + span - 1:
            return 65535
        return int(round(((value - offset) * 65535) / (span - 1)))

    @classmethod
    def _mouse_event(
        cls,
        x: int,
        y: int,
        *,
        flags: int,
        virtual_screen: tuple[int, int, int, int],
    ) -> _Input:
        left, top, width, height = virtual_screen
        return _Input(
            type=INPUT_MOUSE,
            payload=_InputPayload(
                mi=_MouseInput(
                    dx=cls._absolute_coordinate(x, left, width),
                    dy=cls._absolute_coordinate(y, top, height),
                    mouseData=0,
                    dwFlags=flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time=0,
                    dwExtraInfo=0,
                )
            ),
        )

    def send_text(
        self,
        text: str,
        *,
        before_batch: Callable[[int], None] | None = None,
        deadline: float,
    ) -> dict[str, Any]:
        try:
            text = _type_text(text)
            scalar_units = [self._utf16_units(character) for character in text]
            event_batches = [
                [
                    event
                    for unit in units
                    for event in (
                        self._keyboard_event(unit, key_up=False),
                        self._keyboard_event(unit, key_up=True),
                    )
                ]
                for units in scalar_units
            ]
        except DriverError:
            raise
        except Exception as exc:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "Unicode INPUT preparation failed before dispatch",
                retryable=False,
                data={
                    "operation": "SendInput",
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                    "events_submitted": 0,
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        events_requested = sum(len(batch) for batch in event_batches)
        events_submitted = 0
        for batch_index, events in enumerate(event_batches):
            try:
                _check_deadline(deadline, post_dispatch=events_submitted > 0)
                if before_batch is not None:
                    before_batch(events_submitted)
                event_array = (_Input * len(events))(*events)
                submitted = int(
                    self._send_input(
                        len(events),
                        event_array,
                        ctypes.sizeof(_Input),
                    )
                )
            except DriverError as exc:
                if events_submitted <= 0:
                    raise
                details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                details.update(
                    {
                        "operation": "SendInput",
                        "phase": "post_dispatch",
                        "effect": "unknown",
                        "events_submitted": events_submitted,
                        "events_requested": events_requested,
                        "batch_index": batch_index,
                        "cause_code": exc.code,
                    }
                )
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "keyboard target context changed after INPUT submission",
                    retryable=False,
                    data=details,
                ) from exc
            except Exception as exc:
                details = {
                    "operation": "SendInput",
                    "events_submitted": events_submitted,
                    "events_requested": events_requested,
                    "batch_index": batch_index,
                    "exception_type": type(exc).__name__,
                }
                if events_submitted <= 0:
                    raise DriverError(
                        "DRIVER.ACTION_FAILED",
                        "SendInput failed before any INPUT event was submitted",
                        retryable=False,
                        data={
                            **details,
                            "phase": "before_dispatch",
                            "effect": "not_applied",
                        },
                    ) from exc
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "SendInput failed after INPUT events were submitted",
                    retryable=False,
                    data={
                        **details,
                        "phase": "post_dispatch",
                        "effect": "unknown",
                    },
                ) from exc
            submitted = max(0, submitted)
            events_submitted += submitted
            if submitted != len(events):
                try:
                    win32_error = int(self._get_last_error())
                except Exception:
                    win32_error = 0
                details = {
                    "operation": "SendInput",
                    "events_submitted": events_submitted,
                    "events_requested": events_requested,
                    "batch_events_requested": len(events),
                    "batch_events_submitted": submitted,
                    "batch_index": batch_index,
                    "win32_error": win32_error,
                }
                if events_submitted <= 0:
                    raise DriverError(
                        "DRIVER.ACTION_FAILED",
                        (
                            "SendInput submitted no INPUT events; Windows may have "
                            "blocked injection at an integrity or desktop boundary"
                        ),
                        retryable=False,
                        data={
                            **details,
                            "phase": "before_dispatch",
                            "effect": "not_applied",
                        },
                    )
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "SendInput submitted only part of a Unicode scalar event batch",
                    retryable=False,
                    data={
                        **details,
                        "phase": "post_dispatch",
                        "effect": "unknown",
                    },
                )
        _check_deadline(deadline, post_dispatch=True)
        return {
            "native_pattern": "SendInput",
            "input_mode": "unicode",
            "unicode_scalars": len(scalar_units),
            "utf16_units": sum(len(units) for units in scalar_units),
            "events_submitted": events_submitted,
        }

    def send_pointer_click(
        self,
        x: int,
        y: int,
        *,
        before_dispatch: Callable[[int], None] | None = None,
        deadline: float,
    ) -> dict[str, Any]:
        if isinstance(x, bool) or not isinstance(x, int):
            _fail("DRIVER.INVALID_REQUEST", "pointer_click x must be an integer")
        if isinstance(y, bool) or not isinstance(y, int):
            _fail("DRIVER.INVALID_REQUEST", "pointer_click y must be an integer")
        virtual_screen = self.virtual_screen_metrics()
        left, top, width, height = virtual_screen
        if not (left <= x < left + width and top <= y < top + height):
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                "pointer_click center point is outside the virtual desktop",
                data={
                    "point": {"x": x, "y": y},
                    "virtual_desktop": {
                        "x": left,
                        "y": top,
                        "width": width,
                        "height": height,
                    },
                },
            )
        events = [
            self._mouse_event(
                x,
                y,
                flags=MOUSEEVENTF_MOVE,
                virtual_screen=virtual_screen,
            ),
            self._mouse_event(
                x,
                y,
                flags=MOUSEEVENTF_LEFTDOWN,
                virtual_screen=virtual_screen,
            ),
            self._mouse_event(
                x,
                y,
                flags=MOUSEEVENTF_LEFTUP,
                virtual_screen=virtual_screen,
            ),
        ]
        try:
            _check_deadline(deadline)
            if before_dispatch is not None:
                before_dispatch(0)
            event_array = (_Input * len(events))(*events)
            submitted = int(
                self._send_input(
                    len(events),
                    event_array,
                    ctypes.sizeof(_Input),
                )
            )
        except DriverError:
            raise
        except Exception as exc:
            raise DriverError(
                "DRIVER.UNKNOWN_EFFECT",
                "SendInput pointer click outcome is unknown after dispatch",
                retryable=False,
                data={
                    "operation": "SendInput",
                    "phase": "post_dispatch",
                    "effect": "unknown",
                    "events_requested": len(events),
                    "events_submitted": 0,
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        submitted = max(0, submitted)
        if submitted != len(events):
            try:
                win32_error = int(self._get_last_error())
            except Exception:
                win32_error = 0
            details = {
                "operation": "SendInput",
                "events_requested": len(events),
                "events_submitted": submitted,
                "win32_error": win32_error,
            }
            if submitted <= 0:
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    (
                        "SendInput submitted no pointer INPUT events; Windows may have "
                        "blocked injection at an integrity or desktop boundary"
                    ),
                    retryable=False,
                    data={
                        **details,
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                    },
                )
            raise DriverError(
                "DRIVER.UNKNOWN_EFFECT",
                "SendInput submitted only part of the pointer click batch",
                retryable=False,
                data={
                    **details,
                    "phase": "post_dispatch",
                    "effect": "unknown",
                },
            )
        _check_deadline(deadline, post_dispatch=True)
        return {
            "native_pattern": "SendInput",
            "input_mode": "mouse",
            "submitted": True,
            "button": "left",
            "position": "center",
            "events_submitted": submitted,
            "point": {"x": x, "y": y},
            "virtual_desktop": {
                "x": left,
                "y": top,
                "width": width,
                "height": height,
            },
        }


class WindowsUIADriver:
    """Snapshot-scoped UIA semantics over an injected native backend."""

    def __init__(self, backend: UIABackend | None = None) -> None:
        self.backend: UIABackend = backend if backend is not None else create_default_backend()
        self.generation = uuid.uuid4().hex
        self._revision = 0
        self._current: _SnapshotRecord | None = None
        # Capture sessions are keyed by id.  Guarded because capture_poll may
        # run while a native callback thread is appending to a sink.
        self._captures: dict[str, _CaptureSession] = {}
        self._capture_lock = threading.Lock()

    def execute(self, action: str, args: Any, *, deadline: float) -> Any:
        try:
            _check_deadline(deadline)
        except DriverError as exc:
            if action in {"type_text", "pointer_click"} and exc.code == "DRIVER.TIMEOUT":
                exc.retryable = False
            raise
        if action in {"type_text", "pointer_click"} and isinstance(self.backend, UnavailableBackend):
            self.backend._raise()
        values = {} if args is None else _object(args, "args")
        if action == "list_windows":
            return self._list_windows(values, deadline)
        if action == "snapshot":
            return self._snapshot(values, deadline)
        if action == "find":
            return self._find(values, deadline)
        if action == "capture_start":
            return self._capture_start(values, deadline)
        if action == "capture_poll":
            return self._capture_poll(values, deadline)
        if action == "capture_stop":
            return self._capture_stop(values, deadline)
        if action in WRITE_ACTIONS:
            try:
                return self._write(action, values, deadline)
            except DriverError as exc:
                # Input injection is never automatically retryable.  A timeout
                # after a submitted INPUT is already normalized to UNKNOWN_EFFECT;
                # this covers every remaining pre-dispatch timeout.
                if action in {"type_text", "pointer_click"} and exc.code == "DRIVER.TIMEOUT":
                    exc.retryable = False
                raise
        _fail("DRIVER.INVALID_REQUEST", f"unknown action: {action}", action=action)

    def _capture_start(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, {"window"}, "args")
        window = _object(args.get("window"), "window")
        if not window:
            _fail("DRIVER.INVALID_REQUEST", "window must not be empty")

        self._expire_captures()
        with self._capture_lock:
            if len(self._captures) >= MAX_CAPTURE_SESSIONS:
                _fail(
                    "DRIVER.CAPTURE_LIMIT",
                    "too many concurrent capture sessions",
                    active_sessions=len(self._captures),
                    limit=MAX_CAPTURE_SESSIONS,
                )

        subscribe = getattr(self.backend, "subscribe", None)
        if subscribe is None:
            _fail(
                "DRIVER.CAPTURE_UNSUPPORTED",
                "this backend cannot observe accessibility events",
                backend=getattr(self.backend, "name", "unknown"),
            )

        sink = CaptureSink()
        subscription = subscribe(window, sink, deadline=deadline)
        session = _CaptureSession(
            session_id=uuid.uuid4().hex,
            window=_json_safe(window),
            sink=sink,
            subscription=subscription,
            expires_at=time.monotonic() + CAPTURE_SESSION_SECONDS,
        )
        with self._capture_lock:
            self._captures[session.session_id] = session
        return {
            "session_id": session.session_id,
            "window": session.window,
            # Reported up front so the UI can warn before recording, rather
            # than leaving the operator to wonder why an interaction vanished.
            "blind_spots": [dict(item) for item in CAPTURE_BLIND_SPOTS],
        }

    def _capture_poll(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, {"session_id", "max_events"}, "args")
        session = self._capture_session(args.get("session_id"))
        max_events = args.get("max_events", MAX_CAPTURE_POLL_EVENTS)
        if isinstance(max_events, bool) or not isinstance(max_events, int):
            _fail("DRIVER.INVALID_REQUEST", "max_events must be an integer")
        if not 1 <= max_events <= MAX_CAPTURE_POLL_EVENTS:
            _fail(
                "DRIVER.INVALID_REQUEST",
                "max_events is out of range",
                minimum=1,
                maximum=MAX_CAPTURE_POLL_EVENTS,
            )
        events, dropped = session.sink.drain(max_events)
        return {
            "session_id": session.session_id,
            "events": events,
            # Never hidden: a dropped event and an idle user look identical
            # unless the loss is reported explicitly.
            "dropped_events": dropped,
            "active": session.active,
        }

    def _capture_stop(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, {"session_id"}, "args")
        session = self._capture_session(args.get("session_id"))
        dropped = self._release_capture(session, deadline)
        return {
            "session_id": session.session_id,
            "stopped": True,
            "dropped_events": dropped,
        }

    def _capture_session(self, value: Any) -> _CaptureSession:
        if not isinstance(value, str) or not value:
            _fail("DRIVER.INVALID_REQUEST", "session_id must be a non-empty string")
        self._expire_captures()
        with self._capture_lock:
            session = self._captures.get(value)
        if session is None:
            _fail("DRIVER.CAPTURE_NOT_FOUND", "unknown capture session")
        return session

    def _release_capture(self, session: _CaptureSession, deadline: float) -> int:
        with self._capture_lock:
            self._captures.pop(session.session_id, None)
        session.active = False
        remaining, dropped = session.sink.drain(MAX_CAPTURE_EVENTS)
        unsubscribe = getattr(self.backend, "unsubscribe", None)
        if unsubscribe is not None and session.subscription is not None:
            try:
                unsubscribe(session.subscription, deadline=deadline)
            except DriverError:
                raise
            except Exception as exc:  # native teardown must not leak
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    "capture teardown failed",
                    data={"exception_type": type(exc).__name__},
                ) from exc
        return dropped + len(remaining)

    def _expire_captures(self) -> None:
        """Drop sessions past their deadline so a forgotten session cannot
        hold native handlers forever."""

        now = time.monotonic()
        with self._capture_lock:
            stale = [s for s in self._captures.values() if s.expires_at <= now]
            for session in stale:
                self._captures.pop(session.session_id, None)
        for session in stale:
            session.active = False
            unsubscribe = getattr(self.backend, "unsubscribe", None)
            if unsubscribe is not None and session.subscription is not None:
                try:
                    unsubscribe(session.subscription, deadline=now + 5.0)
                except Exception:
                    # Expiry is best effort; a failure here must not block the
                    # request that triggered the sweep.
                    pass

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
            if provenance.get("value_redacted") is True:
                actions = [
                    action
                    for action in actions
                    if action not in {"pointer_click", "set_value", "type_text"}
                ]
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
        payload_field = {"set_value": "value", "type_text": "text"}.get(action)
        allowed = {"target", "locator"} | ({payload_field} if payload_field else set())
        if action == "pointer_click":
            allowed |= {"button", "position"}
        _only_keys(args, allowed, "args")
        if "target" not in args or "locator" not in args:
            _fail("DRIVER.INVALID_REQUEST", "target and locator are required")
        value: str | None = None
        pointer_button: str | None = None
        pointer_position: str | None = None
        if action == "set_value":
            if "value" not in args:
                _fail("DRIVER.INVALID_REQUEST", "value is required for set_value")
            value = _text(args["value"], "value")
        elif action == "type_text":
            if "text" not in args:
                _fail("DRIVER.INVALID_REQUEST", "text is required for type_text")
            value = _type_text(args["text"])
        elif action == "pointer_click":
            pointer_button = _pointer_button(args.get("button"))
            pointer_position = _pointer_position(args.get("position"))
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
        provenance = resolved.get("provenance", {})
        protected = isinstance(provenance, Mapping) and (
            provenance.get("value_redacted") is True
        )
        protection_unknown = action in {"type_text", "pointer_click"} and (
            not isinstance(provenance, Mapping)
            or provenance.get("value_redacted") is not False
        )
        if action in {"set_value", "type_text", "pointer_click"} and (
            protected or protection_unknown
        ):
            raise DriverError(
                "DRIVER.PROTECTED_ELEMENT",
                f"{action} is disabled for password or protected elements",
                data={
                    "action": action,
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                },
            )
        if action == "pointer_click":
            states = resolved.get("states")
            if not isinstance(states, Mapping):
                states = {}
            if states.get("enabled") is not True or states.get("offscreen") is not False:
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "pointer_click only supports enabled, on-screen targets",
                    data={"action": action, "states": _json_safe(dict(states))},
                )
            if action not in resolved["actions"]:
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "target does not advertise pointer_click support",
                    data={"action": action, "available_actions": resolved["actions"]},
                )
        elif action not in resolved["actions"]:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                f"target does not support native {action}",
                data={"action": action, "available_actions": resolved["actions"]},
            )
        _check_deadline(deadline)
        native = fresh.handles[fresh_node_id]
        window_handle: int | None = None
        target_process_id: int | None = None
        click_point: tuple[int, int] | None = None
        if action in {"type_text", "pointer_click"}:
            raw_window_handle = fresh.public["window"].get("handle")
            if (
                isinstance(raw_window_handle, bool)
                or not isinstance(raw_window_handle, int)
                or raw_window_handle <= 0
            ):
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    "fresh snapshot lacks a valid top-level window handle",
                    retryable=False,
                    data={
                        "action": action,
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": 0,
                    },
                )
            window_handle = raw_window_handle
        if action == "pointer_click":
            raw_window_process_id = fresh.public["window"].get("process_id")
            raw_target_process_id = provenance.get("process_id") if isinstance(provenance, Mapping) else None
            if (
                isinstance(raw_window_process_id, bool)
                or not isinstance(raw_window_process_id, int)
                or raw_window_process_id <= 0
                or isinstance(raw_target_process_id, bool)
                or not isinstance(raw_target_process_id, int)
                or raw_target_process_id <= 0
                or raw_target_process_id != raw_window_process_id
            ):
                raise DriverError(
                    "DRIVER.STALE_SNAPSHOT",
                    "pointer_click cannot prove the target process and window identity",
                    data={
                        "window_process_id": None
                        if isinstance(raw_window_process_id, bool)
                        or not isinstance(raw_window_process_id, int)
                        else raw_window_process_id,
                        "target_process_id": None
                        if isinstance(raw_target_process_id, bool)
                        or not isinstance(raw_target_process_id, int)
                        else raw_target_process_id,
                    },
                )
            bounds = resolved.get("bounds")
            if not isinstance(bounds, Mapping):
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "pointer_click target lacks clickable bounds",
                )
            click_point = _center_point(bounds)
            target_process_id = raw_target_process_id
        dispatched = False
        try:
            dispatched = True
            if action == "focus":
                backend_result = self.backend.focus(native, deadline=deadline)
            elif action == "invoke":
                backend_result = self.backend.invoke(native, deadline=deadline)
            elif action == "set_value":
                assert value is not None
                backend_result = self.backend.set_value(native, value, deadline=deadline)
            elif action == "pointer_click":
                assert (
                    window_handle is not None
                    and target_process_id is not None
                    and click_point is not None
                    and pointer_button == "left"
                    and pointer_position == "center"
                )
                backend_result = self.backend.pointer_click(
                    native,
                    target_process_id=target_process_id,
                    window_handle=window_handle,
                    x=click_point[0],
                    y=click_point[1],
                    deadline=deadline,
                )
            else:
                assert (
                    action == "type_text"
                    and value is not None
                    and window_handle is not None
                )
                backend_result = self.backend.type_text(
                    native,
                    value,
                    window_handle=window_handle,
                    deadline=deadline,
                )
            _check_deadline(deadline, post_dispatch=True)
        except DriverError as exc:
            details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
            if (
                action in {"type_text", "pointer_click"}
                and exc.code in {"DRIVER.ACTION_FAILED", "DRIVER.TIMEOUT"}
                and details.get("phase") == "before_dispatch"
                and details.get("effect") == "not_applied"
            ):
                details.setdefault("action", action)
                raise DriverError(
                    exc.code,
                    exc.message,
                    retryable=False,
                    data=details,
                ) from exc
            if dispatched and exc.code in {"DRIVER.ACTION_FAILED", "DRIVER.TIMEOUT"}:
                details.setdefault("action", action)
                details["phase"] = "post_dispatch"
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

    def type_text(
        self,
        native: Any,
        text: str,
        *,
        window_handle: int,
        deadline: float,
    ) -> Any:
        self._raise()

    def pointer_click(
        self,
        native: Any,
        *,
        target_process_id: int,
        window_handle: int,
        x: int,
        y: int,
        deadline: float,
    ) -> Any:
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
            self.input_adapter = UnicodeSendInputAdapter()
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

    def _element_from_point(self, x: int, y: int) -> Any:
        try:
            # comtypes generates IUIAutomation.ElementFromPoint with the exact
            # ctypes.wintypes.tagPOINT argument type. A distinct Structure with
            # identical fields is not assignment-compatible at the COM call.
            return self.automation.ElementFromPoint(wintypes.POINT(x=x, y=y))
        except Exception as exc:
            raise self._native_failure("ElementFromPoint", exc) from exc

    def _point_hits_target(self, target: Any, x: int, y: int, *, deadline: float) -> bool:
        _check_deadline(deadline)
        hit = self._element_from_point(x, y)
        _check_deadline(deadline)
        try:
            return bool(self.automation.CompareElements(hit, target))
        except Exception as exc:
            raise self._native_failure("CompareElements", exc) from exc

    def subscribe(
        self, window: Mapping[str, Any], sink: CaptureSink, *, deadline: float
    ) -> Any:
        selected = self._resolve_window(window, deadline=deadline)
        hwnd = selected["window"]["handle"]
        try:
            element = self.automation.ElementFromHandle(hwnd)
        except Exception as exc:
            raise self._native_failure("ElementFromHandle", exc) from exc
        try:
            return _install_capture_handlers(self, element, sink)
        except Exception as exc:
            raise self._native_failure("AddEventHandler", exc) from exc

    def unsubscribe(self, subscription: Any, *, deadline: float) -> None:
        if not isinstance(subscription, Mapping):
            return
        element = subscription.get("element")
        failures = []
        try:
            self.automation.RemoveFocusChangedEventHandler(subscription["focus"])
        except Exception as exc:
            failures.append(exc)
        for event_id in _CAPTURE_EVENT_KINDS:
            try:
                self.automation.RemoveAutomationEventHandler(
                    event_id, element, subscription["automation"]
                )
            except Exception as exc:
                failures.append(exc)
        try:
            self.automation.RemovePropertyChangedEventHandler(
                element, subscription["property"]
            )
        except Exception as exc:
            failures.append(exc)
        if failures:
            raise self._native_failure("RemoveEventHandler", failures[0])

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
        password_unknown = object()
        raw_is_password = self._property(
            element, "CurrentIsPassword", password_unknown
        )
        # Failing to read the protection bit must never enable text writes.
        password_clear = raw_is_password is False or (
            isinstance(raw_is_password, int) and raw_is_password == 0
        )
        is_password = not password_clear
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
        bounds = self._bounds(self._property(element, "CurrentBoundingRectangle"))
        process_id = self._property(element, "CurrentProcessId")
        pointer_clickable = (
            enabled
            and not is_password
            and not bool(self._property(element, "CurrentIsOffscreen", False))
            and isinstance(process_id, int)
            and process_id > 0
            and isinstance(bounds, dict)
            and bounds["width"] > 0
            and bounds["height"] > 0
        )
        actions: list[str] = []
        if enabled and focusable:
            actions.append("focus")
            if not is_password:
                actions.append("type_text")
        if pointer_clickable:
            actions.append("pointer_click")
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
            bounds=bounds,
            actions=actions,
            provenance={
                "control_type_id": control_type_id,
                "automation_id": _safe_text(self._property(element, "CurrentAutomationId")),
                "class_name": _safe_text(self._property(element, "CurrentClassName")),
                "framework_id": _safe_text(self._property(element, "CurrentFrameworkId")),
                "process_id": process_id,
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

    def type_text(
        self,
        native: Any,
        text: str,
        *,
        window_handle: int,
        deadline: float,
    ) -> Any:
        text = _type_text(text)
        if isinstance(window_handle, bool) or not isinstance(window_handle, int) or window_handle <= 0:
            _fail("DRIVER.INVALID_REQUEST", "window_handle must be a positive integer")
        _check_deadline(deadline)
        focus_attempted = False
        try:
            focus_attempted = True
            native.SetFocus()
        except Exception as exc:
            failure = self._native_failure("SetFocus before SendInput", exc)
            details = dict(failure.data) if isinstance(failure.data, Mapping) else {}
            details.update(
                {
                    "phase": "before_dispatch",
                    "effect": "not_applied",
                    "events_submitted": 0,
                    "focus_may_have_changed": focus_attempted,
                }
            )
            raise DriverError(
                failure.code,
                failure.message,
                retryable=False,
                data=details,
            ) from exc
        def verify_target_context(events_submitted: int) -> None:
            if not bool(self._property(native, "CurrentHasKeyboardFocus", False)):
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    "target did not acquire keyboard focus before SendInput",
                    retryable=False,
                    data={
                        "operation": "SetFocus before SendInput",
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": events_submitted,
                    },
                )
            target_process_id = self._property(native, "CurrentProcessId")
            try:
                target_process_id = int(target_process_id)
            except (TypeError, ValueError, OverflowError):
                target_process_id = 0
            foreground_window_handle, foreground_process_id = (
                self.input_adapter.foreground_window_identity()
            )
            if (
                target_process_id <= 0
                or foreground_process_id != target_process_id
                or foreground_window_handle != window_handle
            ):
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    "focused UIA target is not in the expected foreground window",
                    retryable=False,
                    data={
                        "operation": "foreground target verification",
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": events_submitted,
                        "target_process_id": target_process_id or None,
                        "foreground_process_id": foreground_process_id,
                        "expected_window_handle": window_handle,
                        "foreground_window_handle": foreground_window_handle,
                    },
                )

        try:
            return self.input_adapter.send_text(
                text, before_batch=verify_target_context, deadline=deadline
            )
        except DriverError as exc:
            details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
            details.setdefault("focus_may_have_changed", True)
            raise DriverError(
                exc.code,
                exc.message,
                retryable=exc.retryable,
                data=details,
            ) from exc

    def pointer_click(
        self,
        native: Any,
        *,
        target_process_id: int,
        window_handle: int,
        x: int,
        y: int,
        deadline: float,
    ) -> Any:
        if (
            isinstance(target_process_id, bool)
            or not isinstance(target_process_id, int)
            or target_process_id <= 0
        ):
            _fail("DRIVER.INVALID_REQUEST", "target_process_id must be a positive integer")
        if isinstance(window_handle, bool) or not isinstance(window_handle, int) or window_handle <= 0:
            _fail("DRIVER.INVALID_REQUEST", "window_handle must be a positive integer")
        if isinstance(x, bool) or not isinstance(x, int):
            _fail("DRIVER.INVALID_REQUEST", "x must be an integer")
        if isinstance(y, bool) or not isinstance(y, int):
            _fail("DRIVER.INVALID_REQUEST", "y must be an integer")
        _check_deadline(deadline)

        def verify_target_context(events_submitted: int) -> None:
            if not bool(self._property(native, "CurrentIsEnabled", False)):
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "pointer_click target is disabled before SendInput",
                    retryable=False,
                    data={
                        "operation": "pointer target verification",
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": events_submitted,
                    },
                )
            if bool(self._property(native, "CurrentIsOffscreen", True)):
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "pointer_click target is offscreen before SendInput",
                    retryable=False,
                    data={
                        "operation": "pointer target verification",
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": events_submitted,
                    },
                )
            try:
                hit_matches = self._point_hits_target(native, x, y, deadline=deadline)
            except DriverError as exc:
                details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                details.update(
                    {
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": events_submitted,
                        "point": {"x": x, "y": y},
                    }
                )
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    "pointer_click hit-test failed before SendInput",
                    retryable=False,
                    data=details,
                ) from exc
            if not hit_matches:
                raise DriverError(
                    "DRIVER.STALE_SNAPSHOT",
                    "pointer_click hit-test did not match the target element",
                    retryable=False,
                    data={
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": events_submitted,
                        "point": {"x": x, "y": y},
                    },
                )
            raw_target_process_id = self._property(native, "CurrentProcessId")
            try:
                current_target_process_id = int(raw_target_process_id)
            except (TypeError, ValueError, OverflowError):
                current_target_process_id = 0
            foreground_window_handle, foreground_process_id = (
                self.input_adapter.foreground_window_identity()
            )
            if (
                current_target_process_id <= 0
                or current_target_process_id != target_process_id
                or foreground_process_id != target_process_id
                or foreground_window_handle != window_handle
            ):
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    "pointer_click target is not in the expected foreground window",
                    retryable=False,
                    data={
                        "operation": "foreground target verification",
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": events_submitted,
                        "target_process_id": current_target_process_id or None,
                        "expected_target_process_id": target_process_id,
                        "foreground_process_id": foreground_process_id,
                        "expected_window_handle": window_handle,
                        "foreground_window_handle": foreground_window_handle,
                    },
                )

        return self.input_adapter.send_pointer_click(
            x,
            y,
            before_dispatch=verify_target_context,
            deadline=deadline,
        )

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool:
        _check_deadline(deadline)
        try:
            return bool(self.automation.CompareElements(previous, current))
        except Exception as exc:
            raise self._native_failure("CompareElements", exc) from exc


def _install_capture_handlers(backend: Any, element: Any, sink: CaptureSink) -> dict:
    """Register UIA event handlers for one window subtree.

    Returns the handler objects so they can be removed later and, critically,
    so they stay referenced: a comtypes COMObject that goes out of scope can be
    collected while UIA still holds a raw pointer to it.
    """

    import comtypes

    UIA = backend.UIA
    automation = backend.automation

    def describe(sender: Any) -> dict:
        # Property reads on a dying element can fail; identity is best effort.
        def read(name: str) -> Any:
            try:
                return getattr(sender, name)
            except Exception:
                return None

        return {
            "role_id": read("CurrentControlType"),
            "name": read("CurrentName"),
            "class_name": read("CurrentClassName"),
            "automation_id": read("CurrentAutomationId"),
            "framework_id": read("CurrentFrameworkId"),
            "process_id": read("CurrentProcessId"),
        }

    class _FocusHandler(comtypes.COMObject):
        _com_interfaces_ = [UIA.IUIAutomationFocusChangedEventHandler]

        def IUIAutomationFocusChangedEventHandler_HandleFocusChangedEvent(self, sender):
            sink.emit("focus_changed", describe(sender))
            return 0

    class _AutomationHandler(comtypes.COMObject):
        _com_interfaces_ = [UIA.IUIAutomationEventHandler]

        def IUIAutomationEventHandler_HandleAutomationEvent(self, sender, event_id):
            kind = _CAPTURE_EVENT_KINDS.get(event_id)
            if kind is not None:
                sink.emit(kind, describe(sender))
            return 0

    class _PropertyHandler(comtypes.COMObject):
        _com_interfaces_ = [UIA.IUIAutomationPropertyChangedEventHandler]

        def IUIAutomationPropertyChangedEventHandler_HandlePropertyChangedEvent(
            self, sender, property_id, new_value
        ):
            # new_value carries the typed text.  Deliberately discarded: the
            # recorder stores that a value changed, never what it became.
            del new_value
            if property_id == UIA.UIA_ValueValuePropertyId:
                sink.emit("value_changed", describe(sender))
            return 0

    focus_handler = _FocusHandler()
    automation_handler = _AutomationHandler()
    property_handler = _PropertyHandler()

    automation.AddFocusChangedEventHandler(None, focus_handler)
    for event_id in _CAPTURE_EVENT_KINDS:
        automation.AddAutomationEventHandler(
            event_id, element, UIA.TreeScope_Subtree, None, automation_handler
        )
    automation.AddPropertyChangedEventHandler(
        element,
        UIA.TreeScope_Subtree,
        None,
        property_handler,
        [UIA.UIA_ValueValuePropertyId],
    )
    return {
        "element": element,
        "focus": focus_handler,
        "automation": automation_handler,
        "property": property_handler,
    }


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


def _wire_deadline(value: Any, *, retryable: bool = True) -> float:
    if value is None:
        return time.monotonic() + DEFAULT_REQUEST_SECONDS
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        _fail("DRIVER.INVALID_REQUEST", "deadline_ms must be a Unix timestamp in milliseconds")
    remaining = float(value) / 1000.0 - time.time()
    if not math.isfinite(remaining) or remaining <= 0:
        raise DriverError(
            "DRIVER.TIMEOUT",
            "request deadline elapsed before dispatch",
            retryable=retryable,
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
        action_name = ACTION_NAMES[action_id]
        deadline = _wire_deadline(
            request.get("deadline_ms"),
            retryable=action_name not in {"type_text", "pointer_click"},
        )
        result = driver.execute(
            action_name, request.get("args"), deadline=deadline
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
