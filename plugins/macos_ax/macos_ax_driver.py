#!/usr/bin/env python3
"""macOS Accessibility (AX) process driver.

The public worker speaks the repository's UTF-8 NDJSON protocol.  Snapshot,
locator, stale-target and effect semantics live in this platform-independent
module and can be tested with an injected backend on any host.  Production AX
calls are delegated only to the separately built and code-signed Swift helper;
there is deliberately no PyObjC, AppleScript, pointer, or implicit keyboard
fallback.  Keyboard input exists only as the explicit ``type_text`` action.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import plistlib
import select
import subprocess
import sys
import time
from typing import Any, NoReturn, Protocol
import unicodedata
import uuid


PLUGIN_NAME = "desktop.macos_ax"
PLUGIN_VERSION = "0.1.0"
MAX_REQUEST_BYTES = 1024 * 1024
MAX_RESPONSE_BYTES = 8 * 1024 * 1024 - 1
MAX_HELPER_RESPONSE_BYTES = 8 * 1024 * 1024 - 1
MAX_FIELD_CHARS = 4096
MAX_TYPE_TEXT_CHARS = 1024
MAX_TYPE_TEXT_UTF16_UNITS = MAX_TYPE_TEXT_CHARS * 2
DEFAULT_REQUEST_SECONDS = 30.0
DEFAULT_MAX_DEPTH = 32
DEFAULT_MAX_NODES = 1000
MAX_DEPTH = 128
MAX_NODES = 5000
HELPER_PROTOCOL_VERSION = 2
HELPER_BUNDLE_ID = "dev.ai-auto-desktop.macos-ax-helper"
HELPER_BUNDLE_NAME = "MacOSAXHelper.app"
HELPER_EXECUTABLE_NAME = "MacOSAXHelper"

ACTION_IDS = {
    name: f"{PLUGIN_NAME}.{name}@1"
    for name in (
        "list_apps",
        "snapshot",
        "find",
        "focus",
        "invoke",
        "set_value",
        "type_text",
    )
}
ACTION_NAMES = {full_name: short_name for short_name, full_name in ACTION_IDS.items()}
WRITE_ACTIONS = frozenset({"focus", "invoke", "set_value", "type_text"})
NODE_ACTIONS = WRITE_ACTIONS
STATE_NAMES = ("enabled", "focused", "focusable", "editable", "protected")
APP_SELECTOR_FIELDS = frozenset({"process_id", "bundle_id", "name"})
TYPE_TEXT_ROLES = frozenset({"AXTextField", "AXTextArea", "AXComboBox"})


LOCATOR_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "role": {"type": "string", "maxLength": 256},
        "subrole": {"type": ["string", "null"], "maxLength": 256},
        "name": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS},
        "description": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS},
        "value": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS},
        "identifier": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "states": {
            "type": "object",
            "minProperties": 1,
            "properties": {name: {"type": ["boolean", "null"]} for name in STATE_NAMES},
            "additionalProperties": False,
        },
        "actions": {
            "type": "array",
            "minItems": 1,
            "items": {"enum": sorted(NODE_ACTIONS)},
            "uniqueItems": True,
        },
        "match": {"const": "exact"},
    },
    "additionalProperties": False,
    "minProperties": 1,
}

APP_SELECTOR_SCHEMA: dict[str, Any] = {
    "type": "object",
    "minProperties": 1,
    "properties": {
        "process_id": {"type": "integer", "minimum": 1},
        "bundle_id": {"type": "string", "minLength": 1, "maxLength": MAX_FIELD_CHARS},
        "name": {"type": "string", "minLength": 1, "maxLength": MAX_FIELD_CHARS},
    },
    "additionalProperties": False,
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
    ("DRIVER.INVALID_REQUEST", "动作参数无效。", False),
    ("DRIVER.UNAVAILABLE", "macOS AX helper 或 Accessibility 权限不可用。", False),
    ("DRIVER.ACTION_FAILED", "原生 AX 观察操作失败。", False),
    ("DRIVER.TIMEOUT", "请求截止时间已到。", True),
    ("DRIVER.OUTPUT_TOO_LARGE", "规范化响应超过线路限制。", False),
)
LOCATOR_ERRORS = (
    ("DRIVER.NOT_FOUND", "定位器没有匹配节点。", False),
    ("DRIVER.AMBIGUOUS", "定位器匹配多个节点。", False),
    ("DRIVER.STALE_SNAPSHOT", "快照目标已不再是当前版本。", False),
    ("DRIVER.SNAPSHOT_TRUNCATED", "有界快照无法证明唯一性。", False),
)
ACTION_ERRORS = (
    ("DRIVER.ACTION_UNSUPPORTED", "目标不支持所需原生 AX 属性或动作。", False),
    ("DRIVER.PROTECTED_ELEMENT", "目标暴露受保护内容。", False),
    ("DRIVER.UNKNOWN_EFFECT", "原生动作可能已生效。", False),
)


def _error_contracts(entries: Sequence[tuple[str, str, bool]]) -> list[dict[str, Any]]:
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
        "errors": _error_contracts(errors),
    }


SNAPSHOT_OUTPUT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["snapshot_id", "revision", "backend", "app", "nodes", "truncated"],
    "properties": {
        "snapshot_id": {"type": "string"},
        "revision": {"type": "integer", "minimum": 1},
        "backend": {"type": "string"},
        "app": {"type": "object"},
        "nodes": {"type": "array", "items": {"type": "object"}},
        "truncated": {"type": "boolean"},
        "helper_security": {"type": "object"},
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
    "list_apps": _contract(
        "通过完整性校验后的 Swift helper 枚举当前 Aqua 会话的运行中应用。",
        effect="read_only",
        risk_category="observe",
        risk_level="low",
        input_schema={"type": "object", "additionalProperties": False},
        output_schema={
            "type": "object",
            "required": ["backend", "accessibility_trusted", "apps"],
            "properties": {
                "backend": {"type": "string"},
                "accessibility_trusted": {"type": "boolean"},
                "apps": {"type": "array", "items": {"type": "object"}},
                "helper_security": {"type": "object"},
            },
            "additionalProperties": False,
        },
        errors=COMMON_ERRORS,
        permissions=("desktop.observe",),
    ),
    "snapshot": _contract(
        "抓取一个精确应用选择器对应的有界 macOS AX 树。",
        effect="read_only",
        risk_category="observe",
        risk_level="low",
        input_schema={
            "type": "object",
            "required": ["app"],
            "properties": {
                "app": APP_SELECTOR_SCHEMA,
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
        "在当前完整快照中解析仅支持精确匹配的定位器。",
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
        "重新验证目标后设置原生 AXFocused 属性。",
        effect="contextual",
        risk_category="navigate",
        risk_level="medium",
        input_schema=COMMON_WRITE_INPUT,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "invoke": _contract(
        "重新验证目标后执行原生 AXPress 动作。",
        effect="non_idempotent",
        risk_category="modify",
        risk_level="high",
        input_schema=COMMON_WRITE_INPUT,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "set_value": _contract(
        "重新验证目标后设置原生 AXValue 属性。",
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
        "重新验证并聚焦非受保护文本目标后，显式发送有界 Unicode 键盘输入。",
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
                },
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
        "description": "macOS 原生 Accessibility API 进程驱动（完整性校验的 Swift helper）。",
    },
    "actions": ACTION_CONTRACTS,
    "runtime": {
        "kind": "process",
        "protocol": "ndjson-stdio-v1",
        "entrypoint": "./run.sh",
        "platforms": ["macos"],
    },
}


class DriverError(Exception):
    """Stable structured error returned over either process boundary."""

    def __init__(
        self, code: str, message: str, *, retryable: bool = False, data: Any = None
    ) -> None:
        super().__init__(message)
        self.code = str(code)
        self.message = str(message)
        self.retryable = bool(retryable)
        self.data = data


@dataclass(slots=True)
class BackendNode:
    """One AX node; ``native`` is opaque and never crosses public NDJSON."""

    native: Any
    parent_index: int | None
    role: str
    subrole: str | None = None
    name: str | None = None
    description: str | None = None
    value: str | None = None
    states: Mapping[str, bool | None] | None = None
    bounds: Mapping[str, int] | None = None
    actions: Sequence[str] = ()
    provenance: Mapping[str, Any] | None = None


@dataclass(slots=True)
class BackendSnapshot:
    app: Mapping[str, Any]
    nodes: Sequence[BackendNode]
    truncated: bool = False


class AXBackend(Protocol):
    name: str

    def list_apps(self, *, deadline: float) -> Mapping[str, Any]: ...

    def capture(
        self,
        app: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot: ...

    def focus(self, native: Any, *, deadline: float) -> Any: ...

    def invoke(self, native: Any, *, deadline: float) -> Any: ...

    def set_value(self, native: Any, value: str, *, deadline: float) -> Any: ...

    def type_text(self, native: Any, text: str, *, deadline: float) -> Any: ...

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool: ...


@dataclass(slots=True)
class _SnapshotRecord:
    public: dict[str, Any]
    handles: dict[str, Any]
    fingerprints: dict[str, str]
    app_selector: dict[str, Any]
    max_depth: int
    max_nodes: int


def _fail(code: str, message: str, **data: Any) -> NoReturn:
    raise DriverError(code, message, data=data or None)


def _check_deadline(deadline: float, *, post_dispatch: bool = False) -> None:
    if time.monotonic() >= deadline:
        raise DriverError(
            "DRIVER.TIMEOUT",
            "请求截止时间已到",
            retryable=not post_dispatch,
            data={
                "phase": "post_dispatch" if post_dispatch else "before_dispatch",
                "effect": "unknown" if post_dispatch else "not_applied",
            },
        )


def _object(value: Any, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail("DRIVER.INVALID_REQUEST", f"{name} 必须是对象")
    return value


def _only_keys(value: Mapping[str, Any], allowed: set[str], name: str) -> None:
    unknown = sorted(set(value) - allowed)
    if unknown:
        _fail("DRIVER.INVALID_REQUEST", f"{name} 包含不支持的字段", fields=unknown)


def _bounded_integer(value: Any, name: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"{name} 必须是 {minimum} 到 {maximum} 之间的整数",
        )
    return value


def _text(
    value: Any, name: str, *, nullable: bool = False, non_empty: bool = False
) -> str | None:
    if value is None and nullable:
        return None
    if not isinstance(value, str):
        _fail("DRIVER.INVALID_REQUEST", f"{name} 必须是字符串")
    if non_empty and not value:
        _fail("DRIVER.INVALID_REQUEST", f"{name} 不能为空")
    if len(value) > MAX_FIELD_CHARS:
        _fail("DRIVER.INVALID_REQUEST", f"{name} 超过 {MAX_FIELD_CHARS} 字符")
    return value


def _safe_text(value: Any) -> str | None:
    if value is None:
        return None
    try:
        return str(value)[:MAX_FIELD_CHARS]
    except Exception:
        return None


def _keyboard_text(value: Any) -> str:
    if not isinstance(value, str):
        _fail("DRIVER.INVALID_REQUEST", "text 必须是字符串")
    if not value or len(value) > MAX_TYPE_TEXT_CHARS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"text 必须包含 1 到 {MAX_TYPE_TEXT_CHARS} 个字符",
        )
    text = value
    for index, character in enumerate(text):
        codepoint = ord(character)
        if unicodedata.category(character) == "Cc":
            _fail(
                "DRIVER.INVALID_REQUEST",
                "text 不允许包含 NUL 或控制字符",
                character_index=index,
            )
        if 0xD800 <= codepoint <= 0xDFFF:
            _fail(
                "DRIVER.INVALID_REQUEST",
                "text 必须是有效的 Unicode 标量序列",
                character_index=index,
            )
    utf16_units = len(text.encode("utf-16-le")) // 2
    if utf16_units > MAX_TYPE_TEXT_UTF16_UNITS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"text 的 UTF-16 长度超过 {MAX_TYPE_TEXT_UTF16_UNITS}",
            limit_utf16_units=MAX_TYPE_TEXT_UTF16_UNITS,
        )
    return text


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


def _backend_name(backend: Any) -> str:
    return _safe_text(getattr(backend, "name", None)) or "unknown"


class MacOSAXDriver:
    """Snapshot-scoped AX semantics over an injected native backend."""

    def __init__(self, backend: AXBackend | None = None) -> None:
        self.backend: AXBackend = backend if backend is not None else create_default_backend()
        self.generation = uuid.uuid4().hex
        self._revision = 0
        self._current: _SnapshotRecord | None = None

    def execute(self, action: str, args: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        values = {} if args is None else _object(args, "args")
        if action == "list_apps":
            return self._list_apps(values, deadline)
        if action == "snapshot":
            return self._snapshot(values, deadline)
        if action == "find":
            return self._find(values, deadline)
        if action in WRITE_ACTIONS:
            try:
                return self._write(action, values, deadline)
            except DriverError as exc:
                if action != "type_text" or exc.code == "DRIVER.UNKNOWN_EFFECT":
                    raise
                details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                details["effect"] = (
                    "contextual"
                    if details.get("focus_changed") is True
                    else "not_applied"
                )
                raise DriverError(
                    exc.code, exc.message, retryable=exc.retryable, data=details
                ) from exc
        _fail("DRIVER.INVALID_REQUEST", f"未知动作：{action}", action=action)

    def _list_apps(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, set(), "args")
        raw = self.backend.list_apps(deadline=deadline)
        _check_deadline(deadline)
        if not isinstance(raw, Mapping):
            raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效应用列表")
        apps = raw.get("apps")
        trusted = raw.get("accessibility_trusted")
        if (
            isinstance(apps, (str, bytes))
            or not isinstance(apps, Sequence)
            or not isinstance(trusted, bool)
        ):
            raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效应用列表")
        normalized_apps = []
        for app in apps:
            _check_deadline(deadline)
            if not isinstance(app, Mapping):
                raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效应用记录")
            normalized_app = _json_safe(app)
            if not isinstance(normalized_app, dict):
                raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效应用记录")
            # Process paths are unnecessary for exact app selection and may
            # expose usernames or private installation layouts.
            normalized_app.pop("executable", None)
            normalized_apps.append(normalized_app)
        result = {
            "backend": _backend_name(self.backend),
            "accessibility_trusted": trusted,
            "apps": normalized_apps,
        }
        security_info = getattr(self.backend, "security_info", None)
        if callable(security_info):
            security = _json_safe(security_info())
            if not isinstance(security, dict):
                raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效 helper 安全信息")
            result["helper_security"] = security
        return result

    def _app_selector(self, raw: Any) -> dict[str, Any]:
        selector = _object(raw, "app")
        _only_keys(selector, set(APP_SELECTOR_FIELDS), "app")
        if not selector:
            _fail("DRIVER.INVALID_REQUEST", "app 必须包含精确选择器")
        normalized: dict[str, Any] = {}
        if "process_id" in selector:
            process_id = selector["process_id"]
            if isinstance(process_id, bool) or not isinstance(process_id, int) or process_id < 1:
                _fail("DRIVER.INVALID_REQUEST", "app.process_id 必须是正整数")
            normalized["process_id"] = process_id
        for name in ("bundle_id", "name"):
            if name in selector:
                normalized[name] = _text(
                    selector[name], f"app.{name}", non_empty=True
                )
        return normalized

    def _snapshot(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, {"app", "max_depth", "max_nodes"}, "args")
        app = self._app_selector(args.get("app"))
        max_depth = _bounded_integer(
            args.get("max_depth", DEFAULT_MAX_DEPTH), "max_depth", 0, MAX_DEPTH
        )
        max_nodes = _bounded_integer(
            args.get("max_nodes", DEFAULT_MAX_NODES), "max_nodes", 1, MAX_NODES
        )
        record = self._capture(
            app, max_depth=max_depth, max_nodes=max_nodes, deadline=deadline
        )
        return copy.deepcopy(record.public)

    def _capture(
        self,
        app: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> _SnapshotRecord:
        _check_deadline(deadline)
        raw = self.backend.capture(
            app, max_depth=max_depth, max_nodes=max_nodes, deadline=deadline
        )
        _check_deadline(deadline)
        if not isinstance(raw, BackendSnapshot):
            raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效快照")
        if len(raw.nodes) > max_nodes:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "后端超过请求的节点限制",
                data={"max_nodes": max_nodes, "actual": len(raw.nodes)},
            )
        self._revision += 1
        revision = self._revision
        snapshot_id = f"{self.generation}:{revision}"
        nodes: list[dict[str, Any]] = []
        handles: dict[str, Any] = {}
        fingerprints: dict[str, str] = {}
        for index, backend_node in enumerate(raw.nodes):
            _check_deadline(deadline)
            if not isinstance(backend_node, BackendNode):
                raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效节点")
            parent_id: str | None = None
            if backend_node.parent_index is not None:
                parent = backend_node.parent_index
                if (
                    isinstance(parent, bool)
                    or not isinstance(parent, int)
                    or parent < 0
                    or parent >= index
                ):
                    raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效父子关系")
                parent_id = f"n{parent}"
            node_id = f"n{index}"
            states = {name: None for name in STATE_NAMES}
            if backend_node.states is not None:
                for name in STATE_NAMES:
                    state = backend_node.states.get(name)
                    states[name] = state if isinstance(state, bool) or state is None else None
            protected = states["protected"] is True
            actions = sorted(
                {str(item) for item in backend_node.actions if str(item) in NODE_ACTIONS}
            )
            if protected:
                actions = [
                    action for action in actions if action not in {"set_value", "type_text"}
                ]
            provenance = _json_safe(dict(backend_node.provenance or {}))
            if not isinstance(provenance, dict):
                provenance = {}
            provenance["backend"] = _backend_name(self.backend)
            if protected:
                provenance["value_redacted"] = True
            node = {
                "node_id": node_id,
                "parent_id": parent_id,
                "role": _safe_text(backend_node.role) or "unknown",
                "subrole": _safe_text(backend_node.subrole),
                "name": _safe_text(backend_node.name),
                "description": _safe_text(backend_node.description),
                "value": None if protected else _safe_text(backend_node.value),
                "states": states,
                "bounds": _normalize_bounds(backend_node.bounds),
                "actions": actions,
                "provenance": provenance,
            }
            nodes.append(node)
            handles[node_id] = backend_node.native
            fingerprints[node_id] = self._fingerprint(node)
        normalized_app = _json_safe(raw.app)
        if not isinstance(normalized_app, dict):
            raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效应用身份")
        normalized_app.pop("executable", None)
        public = {
            "snapshot_id": snapshot_id,
            "revision": revision,
            "backend": _backend_name(self.backend),
            "app": normalized_app,
            "nodes": nodes,
            "truncated": bool(raw.truncated),
        }
        security_info = getattr(self.backend, "security_info", None)
        if callable(security_info):
            security = _json_safe(security_info())
            if not isinstance(security, dict):
                raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效 helper 安全信息")
            public["helper_security"] = security
        record = _SnapshotRecord(
            public=public,
            handles=handles,
            fingerprints=fingerprints,
            app_selector=copy.deepcopy(dict(app)),
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
            "subrole": node.get("subrole"),
            "name": node.get("name"),
            "identifier": provenance.get("identifier"),
            "process_id": provenance.get("process_id"),
            "bundle_id": provenance.get("bundle_id"),
        }
        encoded = json.dumps(
            identity, ensure_ascii=False, allow_nan=False, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        return "sha256:" + hashlib.sha256(encoded).hexdigest()

    def _record(self, snapshot_id: Any, revision: Any) -> _SnapshotRecord:
        if not isinstance(snapshot_id, str) or not snapshot_id:
            _fail("DRIVER.INVALID_REQUEST", "snapshot_id 必须是非空字符串")
        if isinstance(revision, bool) or not isinstance(revision, int) or revision < 1:
            _fail("DRIVER.INVALID_REQUEST", "revision 必须是正整数")
        record = self._current
        if (
            record is None
            or record.public["snapshot_id"] != snapshot_id
            or record.public["revision"] != revision
        ):
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "快照不是驱动当前版本",
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
                "截断快照无法证明定位器唯一",
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
            "subrole",
            "name",
            "description",
            "value",
            "identifier",
            "states",
            "actions",
            "match",
        }
        _only_keys(locator, allowed, "locator")
        if not set(locator) - {"match"}:
            _fail("DRIVER.INVALID_REQUEST", "locator 必须包含至少一个选择条件")
        if locator.get("match", "exact") != "exact":
            _fail("DRIVER.INVALID_REQUEST", "当前版本只支持精确定位")
        normalized: dict[str, Any] = {"match": "exact"}
        if "role" in locator:
            normalized["role"] = _text(locator["role"], "locator.role")
        for name in ("subrole", "name", "description", "value"):
            if name in locator:
                normalized[name] = _text(
                    locator[name], f"locator.{name}", nullable=True
                )
        if "identifier" in locator:
            normalized["identifier"] = _text(
                locator["identifier"], "locator.identifier"
            )
        if "states" in locator:
            states = _object(locator["states"], "locator.states")
            _only_keys(states, set(STATE_NAMES), "locator.states")
            if not states:
                _fail("DRIVER.INVALID_REQUEST", "locator.states 不能为空")
            normalized_states: dict[str, bool | None] = {}
            for name, state in states.items():
                if not isinstance(state, bool) and state is not None:
                    _fail(
                        "DRIVER.INVALID_REQUEST",
                        f"locator.states.{name} 必须是 boolean 或 null",
                    )
                normalized_states[name] = state
            normalized["states"] = normalized_states
        if "actions" in locator:
            actions = locator["actions"]
            if isinstance(actions, (str, bytes)) or not isinstance(actions, list) or not actions:
                _fail("DRIVER.INVALID_REQUEST", "locator.actions 必须是非空数组")
            normalized_actions: list[str] = []
            for action in actions:
                if not isinstance(action, str) or action not in NODE_ACTIONS:
                    _fail("DRIVER.INVALID_REQUEST", "locator.actions 包含不支持的动作")
                if action in normalized_actions:
                    _fail("DRIVER.INVALID_REQUEST", "locator.actions 必须唯一")
                normalized_actions.append(action)
            normalized["actions"] = normalized_actions
        return normalized

    def _matches(self, node: Mapping[str, Any], locator: Mapping[str, Any]) -> bool:
        for name in ("role", "subrole", "name", "description", "value"):
            if name in locator and node.get(name) != locator[name]:
                return False
        provenance = node.get("provenance")
        if not isinstance(provenance, Mapping):
            provenance = {}
        if "identifier" in locator and provenance.get("identifier") != locator["identifier"]:
            return False
        states = locator.get("states", {})
        node_states = node.get("states", {})
        if not isinstance(node_states, Mapping):
            return False
        if any(node_states.get(name) is not expected for name, expected in states.items()):
            return False
        return set(locator.get("actions", ())).issubset(set(node.get("actions", ())))

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
                "定位器没有匹配节点",
                data={"locator": dict(locator), "snapshot_id": record.public["snapshot_id"]},
            )
        if len(candidates) > 1:
            raise DriverError(
                "DRIVER.AMBIGUOUS",
                "定位器匹配多个节点",
                data={
                    "candidate_count": len(candidates),
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
        payload_field = (
            "value"
            if action == "set_value"
            else "text"
            if action == "type_text"
            else None
        )
        allowed = {"target", "locator"} | ({payload_field} if payload_field else set())
        _only_keys(args, allowed, "args")
        if "target" not in args or "locator" not in args:
            _fail("DRIVER.INVALID_REQUEST", "target 和 locator 为必填字段")
        value: str | None = None
        text: str | None = None
        if action == "set_value":
            if "value" not in args:
                _fail("DRIVER.INVALID_REQUEST", "set_value 必须提供 value")
            value = _text(args["value"], "value")
        elif action == "type_text":
            if "text" not in args:
                _fail("DRIVER.INVALID_REQUEST", "type_text 必须提供 text")
            text = _keyboard_text(args["text"])
        target = _object(args["target"], "target")
        _only_keys(target, {"snapshot_id", "revision", "node_id"}, "target")
        node_id = target.get("node_id")
        if not isinstance(node_id, str) or not node_id:
            _fail("DRIVER.INVALID_REQUEST", "target.node_id 必须是非空字符串")
        record = self._record(target.get("snapshot_id"), target.get("revision"))
        if record.public.get("truncated"):
            raise DriverError("DRIVER.SNAPSHOT_TRUNCATED", "截断快照不能用于写动作")
        locator = self._locator(args["locator"])
        expected = self._resolve(record, locator, deadline)
        if expected["node_id"] != node_id or node_id not in record.handles:
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "target 与该快照中的定位结果不一致",
                data={"node_id": node_id, "resolved_node_id": expected["node_id"]},
            )
        if action in {"set_value", "type_text"} and expected["states"].get("protected") is True:
            raise DriverError(
                "DRIVER.PROTECTED_ELEMENT", f"受保护元素不允许 {action}"
            )
        expected_fingerprint = record.fingerprints[node_id]
        previous_native = record.handles[node_id]
        _check_deadline(deadline)
        try:
            fresh = self._capture(
                record.app_selector,
                max_depth=record.max_depth,
                max_nodes=record.max_nodes,
                deadline=deadline,
            )
        except DriverError as exc:
            if isinstance(exc.data, Mapping) and exc.data.get("helper_channel_failure") is True:
                self._current = None
            raise
        try:
            resolved = self._resolve(fresh, locator, deadline)
        except DriverError as exc:
            if exc.code in {"DRIVER.NOT_FOUND", "DRIVER.AMBIGUOUS"}:
                details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                raise DriverError(
                    "DRIVER.STALE_SNAPSHOT",
                    "定位器不再解析到原唯一目标",
                    data={"reason": exc.code, **details},
                ) from exc
            raise
        if fresh.public.get("truncated"):
            raise DriverError("DRIVER.SNAPSHOT_TRUNCATED", "派发前快照已截断")
        fresh_node_id = resolved["node_id"]
        try:
            same = self.backend.same_element(
                previous_native, fresh.handles[fresh_node_id], deadline=deadline
            )
        except DriverError as exc:
            if isinstance(exc.data, Mapping) and exc.data.get("helper_channel_failure") is True:
                self._current = None
            if exc.code == "DRIVER.UNAVAILABLE":
                raise
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "无法验证原生 AX 目标身份",
                data={"reason": exc.code},
            ) from exc
        except Exception as exc:
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "无法验证原生 AX 目标身份",
                data={"exception_type": type(exc).__name__},
            ) from exc
        if same is not True:
            raise DriverError("DRIVER.STALE_SNAPSHOT", "定位器解析到了不同的原生 AX 元素")
        if fresh.fingerprints[fresh_node_id] != expected_fingerprint:
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "定位器解析到了不同的语义目标",
                data={
                    "previous_snapshot_id": record.public["snapshot_id"],
                    "current_snapshot_id": fresh.public["snapshot_id"],
                },
            )
        if action in {"set_value", "type_text"} and resolved["states"].get("protected") is True:
            raise DriverError(
                "DRIVER.PROTECTED_ELEMENT", f"受保护元素不允许 {action}"
            )
        if action == "type_text" and (
            resolved.get("role") not in TYPE_TEXT_ROLES
            or resolved["states"].get("protected") is not False
        ):
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                "type_text 只支持可确认非受保护的文本输入目标",
                data={"action": action, "role": resolved.get("role")},
            )
        if action not in resolved["actions"]:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                f"目标不支持原生 {action}",
                data={"action": action, "available_actions": resolved["actions"]},
            )
        _check_deadline(deadline)
        native = fresh.handles[fresh_node_id]
        # Generic injected backends cross their dispatch boundary when their
        # method is entered.  The Swift transport can override this with the
        # precise ``helper_request_dispatched`` bit: a complete newline-
        # terminated helper frame is its dispatch boundary.
        dispatched = False
        channel_failed = False
        try:
            dispatched = True
            if action == "focus":
                backend_result = self.backend.focus(native, deadline=deadline)
            elif action == "invoke":
                backend_result = self.backend.invoke(native, deadline=deadline)
            elif action == "set_value":
                assert value is not None
                backend_result = self.backend.set_value(native, value, deadline=deadline)
            else:
                assert action == "type_text" and text is not None
                backend_result = self.backend.type_text(native, text, deadline=deadline)
            _check_deadline(deadline, post_dispatch=True)
        except DriverError as exc:
            details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
            helper_dispatched = details.get("helper_request_dispatched")
            if isinstance(helper_dispatched, bool):
                dispatched = helper_dispatched
            channel_failure = details.get("helper_channel_failure") is True
            channel_failed = channel_failure
            if exc.code == "DRIVER.UNKNOWN_EFFECT":
                if (
                    action == "type_text"
                    and details.get("keyboard_dispatch_started") is True
                ):
                    self._terminate_backend_after_unknown_effect()
                raise
            if action == "type_text":
                keyboard_started = details.get("keyboard_dispatch_started") is True
                if keyboard_started:
                    self._terminate_backend_after_unknown_effect()
                    details.setdefault("action", action)
                    details["effect"] = "unknown"
                    raise DriverError(
                        "DRIVER.UNKNOWN_EFFECT",
                        "显式键盘输入派发后的结果未知",
                        data=details,
                    ) from exc
                details["effect"] = (
                    "contextual"
                    if details.get("focus_changed") is True
                    else "not_applied"
                )
                raise DriverError(
                    exc.code, exc.message, retryable=exc.retryable, data=details
                ) from exc
            if dispatched and (
                channel_failure or exc.code in {"DRIVER.ACTION_FAILED", "DRIVER.TIMEOUT"}
            ):
                details.setdefault("action", action)
                details["effect"] = "unknown"
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "原生 AX 动作派发后的结果未知",
                    data=details,
                ) from exc
            raise
        except Exception as exc:
            raise DriverError(
                "DRIVER.UNKNOWN_EFFECT",
                f"原生 {action} 派发后的结果未知",
                data={
                    "action": action,
                    "effect": "unknown" if dispatched else "not_applied",
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        finally:
            if dispatched or channel_failed:
                self._current = None
        return {
            "ok": True,
            "action": action,
            "resolved": self._target(fresh, fresh_node_id),
            "backend_result": _json_safe(backend_result),
        }

    def _terminate_backend_after_unknown_effect(self) -> None:
        terminate = getattr(self.backend, "terminate_after_unknown_effect", None)
        if callable(terminate):
            terminate()


class UnavailableBackend:
    """Explicit fail-closed backend for unsupported production hosts."""

    name = "macos_ax_unavailable"

    def __init__(self, reason: str, **details: Any) -> None:
        self.reason = reason
        self.details = details

    def _raise(self) -> NoReturn:
        raise DriverError(
            "DRIVER.UNAVAILABLE",
            "macOS AX Swift helper 不可用",
            data={"reason": self.reason, **_json_safe(self.details)},
        )

    def list_apps(self, *, deadline: float) -> Mapping[str, Any]:
        self._raise()

    def capture(
        self,
        app: Mapping[str, Any],
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

    def type_text(self, native: Any, text: str, *, deadline: float) -> Any:
        self._raise()

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool:
        self._raise()


def _helper_app_for_executable(executable: Path) -> Path | None:
    resolved = executable.resolve()
    for parent in resolved.parents:
        if parent.suffix == ".app" and parent.name == HELPER_BUNDLE_NAME:
            expected = parent / "Contents" / "MacOS" / HELPER_EXECUTABLE_NAME
            if resolved == expected.resolve():
                return parent
    return None


def _validate_helper_bundle(app_bundle: Path) -> None:
    info_path = app_bundle / "Contents" / "Info.plist"
    try:
        with info_path.open("rb") as stream:
            info = plistlib.load(stream)
    except (OSError, plistlib.InvalidFileException) as exc:
        raise DriverError(
            "DRIVER.UNAVAILABLE",
            "AX helper Info.plist 缺失或无效",
            data={
                "reason": "invalid_helper_bundle",
                "exception_type": type(exc).__name__,
            },
        ) from exc
    if not isinstance(info, dict):
        raise DriverError(
            "DRIVER.UNAVAILABLE",
            "AX helper Info.plist 不是字典",
            data={"reason": "invalid_helper_bundle"},
        )
    expected = {
        "CFBundleIdentifier": HELPER_BUNDLE_ID,
        "CFBundleExecutable": HELPER_EXECUTABLE_NAME,
        "CFBundlePackageType": "APPL",
    }
    mismatches = {
        name: {"expected": value, "actual": info.get(name)}
        for name, value in expected.items()
        if info.get(name) != value
    }
    if mismatches:
        raise DriverError(
            "DRIVER.UNAVAILABLE",
            "AX helper bundle identity 不符合预期",
            data={"reason": "helper_bundle_identity_mismatch", "fields": mismatches},
        )


class SwiftHelperBackend:
    """RPC adapter for an integrity-checked native Swift AX helper.

    ``codesign --verify`` detects bundle modification; it does not authenticate
    who supplied the bundle.  An explicitly configured helper is consequently
    marked as an untrusted custom source even when its integrity is valid.
    """

    name = "macos_ax_swift_helper"

    def __init__(self, executable: str | os.PathLike[str] | None = None) -> None:
        if sys.platform != "darwin":
            raise DriverError("DRIVER.UNAVAILABLE", "macOS AX helper 只能在 macOS 运行")
        environment_helper = os.environ.get("AI_AUTO_DESKTOP_MACOS_AX_HELPER")
        configured = environment_helper if executable is None else os.fspath(executable)
        self.helper_source = "custom_untrusted" if configured else "default_build"
        self.source_authenticated = False
        self.integrity_verified = False
        self._closed = False
        self._process: subprocess.Popen[bytes] | None = None
        helper = (
            Path(configured)
            if configured
            else Path(__file__).resolve().parent
            / ".build"
            / "MacOSAXHelper.app"
            / "Contents"
            / "MacOS"
            / "MacOSAXHelper"
        )
        if not helper.is_file() or not os.access(helper, os.X_OK):
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "尚未构建 macOS AX Swift helper",
                data={
                    "reason": "helper_missing",
                    "helper_source": self.helper_source,
                    "source_authenticated": False,
                },
            )
        app_bundle = _helper_app_for_executable(helper)
        if app_bundle is None:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "AX helper 必须位于预期的签名 .app bundle 中",
                data={
                    "reason": "invalid_helper_layout",
                    "helper_source": self.helper_source,
                    "source_authenticated": False,
                },
            )
        try:
            _validate_helper_bundle(app_bundle)
        except DriverError as exc:
            details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
            details["helper_source"] = self.helper_source
            details["source_authenticated"] = False
            raise DriverError(
                exc.code, exc.message, retryable=exc.retryable, data=details
            ) from exc
        try:
            verification = subprocess.run(
                ["/usr/bin/codesign", "--verify", "--strict", "--deep", "--verbose=2", str(app_bundle)],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=10,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "无法验证 AX helper 代码签名完整性",
                data={
                    "reason": "signature_check_failed",
                    "exception_type": type(exc).__name__,
                    "helper_source": self.helper_source,
                    "source_authenticated": False,
                },
            ) from exc
        if verification.returncode != 0:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "AX helper 代码签名验证失败",
                data={
                    "reason": "signature_invalid",
                    "codesign_exit_code": verification.returncode,
                    "helper_source": self.helper_source,
                    "source_authenticated": False,
                },
            )
        self.integrity_verified = True
        try:
            self._process = subprocess.Popen(
                [str(helper)],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=None,
                bufsize=0,
                start_new_session=True,
            )
        except OSError as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "无法启动签名 AX helper",
                data={
                    "reason": "helper_start_failed",
                    "exception_type": type(exc).__name__,
                    "helper_source": self.helper_source,
                    "source_authenticated": False,
                },
            ) from exc
        self._buffer = bytearray()
        self._request_number = 0
        try:
            status = self._rpc("status", {}, deadline=time.monotonic() + 5.0)
            if (
                not isinstance(status, Mapping)
                or status.get("protocol_version") != HELPER_PROTOCOL_VERSION
                or status.get("implementation") != "native_accessibility_api"
            ):
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 握手不兼容",
                    request_dispatched=True,
                    data={"reason": "helper_protocol_mismatch"},
                )
        except Exception:
            self.close()
            raise

    def _close_process(self, *, force: bool) -> None:
        self._closed = True
        process = getattr(self, "_process", None)
        if process is None:
            return
        try:
            try:
                running = process.poll() is None
            except OSError:
                running = True
            if running:
                try:
                    if force:
                        process.kill()
                    else:
                        process.terminate()
                except OSError:
                    pass
                try:
                    process.wait(timeout=1.0)
                except (OSError, subprocess.TimeoutExpired):
                    try:
                        process.kill()
                        process.wait(timeout=1.0)
                    except (OSError, subprocess.TimeoutExpired):
                        pass
        finally:
            for stream in (process.stdin, process.stdout):
                if stream is not None:
                    try:
                        stream.close()
                    except (OSError, ValueError):
                        pass

    def close(self) -> None:
        self._close_process(force=False)

    def terminate_after_unknown_effect(self) -> None:
        self._close_process(force=True)

    def security_info(self) -> Mapping[str, Any]:
        return {
            "source": self.helper_source,
            "integrity_verified": self.integrity_verified,
            # codesign validity proves integrity relative to a signature; no
            # signer identity is pinned or authenticated by this adapter.
            "source_authenticated": self.source_authenticated,
        }

    def _channel_error(
        self,
        code: str,
        message: str,
        *,
        request_dispatched: bool,
        retryable: bool = False,
        data: Mapping[str, Any] | None = None,
    ) -> DriverError:
        details = dict(data or {})
        details["helper_channel_failure"] = True
        details["helper_request_dispatched"] = request_dispatched
        details["effect"] = "unknown" if request_dispatched else "not_applied"
        self._close_process(force=True)
        return DriverError(code, message, retryable=retryable, data=details)

    def _readline(
        self, deadline: float, *, request_dispatched: bool = True,
        keyboard_dispatch_started: bool = False, focus_changed: bool = False,
    ) -> bytes:
        process = self._process
        stdout = None if process is None else process.stdout
        channel_effect = (
            "unknown"
            if keyboard_dispatch_started
            else "contextual"
            if focus_changed
            else "not_applied"
        )
        if stdout is None:
            raise self._channel_error(
                "DRIVER.UNAVAILABLE",
                "AX helper stdout 不可用",
                    request_dispatched=request_dispatched,
                    data={
                        "reason": "helper_stdout_missing",
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                        "effect": channel_effect,
                    },
            )
        while True:
            # Check the accumulated frame before accepting a newline.  A
            # single read may contain both an oversized frame and its newline.
            if len(self._buffer) > MAX_HELPER_RESPONSE_BYTES:
                raise self._channel_error(
                    "DRIVER.OUTPUT_TOO_LARGE",
                    "AX helper 响应超过限制",
                    request_dispatched=request_dispatched,
                    data={
                        "limit_bytes": MAX_HELPER_RESPONSE_BYTES,
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                        "effect": channel_effect,
                    },
                )
            newline = self._buffer.find(b"\n")
            if newline >= 0:
                line = bytes(self._buffer[:newline])
                del self._buffer[: newline + 1]
                return line
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise self._channel_error(
                    "DRIVER.TIMEOUT",
                    "等待 AX helper 响应超时",
                    request_dispatched=request_dispatched,
                    retryable=True,
                    data={
                        "phase": "helper_response",
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                        "effect": channel_effect,
                    },
                )
            try:
                ready, _, _ = select.select([stdout.fileno()], [], [], remaining)
            except (OSError, ValueError) as exc:
                raise self._channel_error(
                    "DRIVER.UNAVAILABLE",
                    "读取 AX helper 响应失败",
                    request_dispatched=request_dispatched,
                    data={
                        "reason": "helper_read_failed",
                        "exception_type": type(exc).__name__,
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                        "effect": channel_effect,
                    },
                ) from exc
            if not ready:
                continue
            try:
                chunk = os.read(stdout.fileno(), 65536)
            except OSError as exc:
                raise self._channel_error(
                    "DRIVER.UNAVAILABLE",
                    "读取 AX helper 响应失败",
                    request_dispatched=request_dispatched,
                    data={
                        "reason": "helper_read_failed",
                        "exception_type": type(exc).__name__,
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                        "effect": channel_effect,
                    },
                ) from exc
            if not chunk:
                raise self._channel_error(
                    "DRIVER.UNAVAILABLE",
                    "AX helper 在响应前退出",
                    request_dispatched=request_dispatched,
                    data={
                        "reason": "helper_eof",
                        "exit_code": process.poll(),
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                        "effect": channel_effect,
                    },
                )
            self._buffer.extend(chunk)
            # Do not return a newline discovered in this chunk until the next
            # loop iteration has enforced the complete-buffer limit.

    def _rpc(
        self, operation: str, args: Mapping[str, Any], *, deadline: float,
        require_keyboard_dispatch_state: bool = False,
    ) -> Any:
        try:
            _check_deadline(deadline)
        except DriverError as exc:
            details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
            details["helper_request_dispatched"] = False
            details["effect"] = "not_applied"
            raise DriverError(
                exc.code, exc.message, retryable=exc.retryable, data=details
            ) from exc
        process = self._process
        stdin = None if process is None else process.stdin
        if self._closed or stdin is None or process.poll() is not None:
            raise self._channel_error(
                "DRIVER.UNAVAILABLE",
                "AX helper 未运行",
                request_dispatched=False,
                data={"reason": "helper_exited" if not self._closed else "helper_closed"},
            )
        self._request_number += 1
        request_id = f"h{self._request_number}"
        remaining = max(0.0, deadline - time.monotonic())
        request = {
            "id": request_id,
            "operation": operation,
            "args": dict(args),
            "deadline_ms": int((time.time() + remaining) * 1000),
        }
        try:
            encoded = (
                json.dumps(
                    request, ensure_ascii=True, allow_nan=False, separators=(",", ":")
                )
                + "\n"
            ).encode("utf-8")
        except (TypeError, ValueError) as exc:
            raise DriverError(
                "DRIVER.INVALID_REQUEST",
                "无法编码 AX helper 请求",
                data={
                    "helper_request_dispatched": False,
                    "effect": "not_applied",
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        if len(encoded) > MAX_REQUEST_BYTES:
            raise DriverError(
                "DRIVER.INVALID_REQUEST",
                "发送给 AX helper 的请求超过限制",
                data={
                    "helper_request_dispatched": False,
                    "effect": "not_applied",
                    "limit_bytes": MAX_REQUEST_BYTES,
                },
            )
        written = 0
        try:
            while written < len(encoded):
                count = os.write(stdin.fileno(), encoded[written:])
                if count <= 0:
                    raise BrokenPipeError("zero-byte helper pipe write")
                written += count
        except (BrokenPipeError, OSError, ValueError) as exc:
            raise self._channel_error(
                "DRIVER.UNAVAILABLE",
                "无法写入 AX helper",
                request_dispatched=False,
                data={
                    "reason": "helper_pipe_failed",
                    "bytes_written": written,
                    "frame_bytes": len(encoded),
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        keyboard_dispatch_started = False
        focus_changed = False
        while True:
            try:
                line = self._readline(
                    deadline,
                    request_dispatched=True,
                    keyboard_dispatch_started=keyboard_dispatch_started,
                    focus_changed=focus_changed,
                )
            except DriverError as exc:
                details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                details["keyboard_dispatch_started"] = keyboard_dispatch_started
                details["focus_changed"] = focus_changed
                details["effect"] = (
                    "unknown"
                    if keyboard_dispatch_started
                    else "contextual"
                    if focus_changed
                    else "not_applied"
                )
                raise DriverError(
                    exc.code, exc.message, retryable=exc.retryable, data=details
                ) from exc
            try:
                response = json.loads(line.decode("utf-8", errors="strict"))
            except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 返回了无效协议帧",
                    request_dispatched=True,
                    data={
                        "reason": "helper_protocol_error",
                        "exception_type": type(exc).__name__,
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                    },
                ) from exc
            if (
                isinstance(response, dict)
                and response.get("id") == request_id
                and set(response) == {"id", "progress"}
            ):
                progress = response.get("progress")
                if (
                    operation != "type_text"
                    or not isinstance(progress, Mapping)
                    or not set(progress).issubset(
                        {"phase", "keyboard_dispatch_started", "focus_changed"}
                    )
                    or progress.get("phase")
                    not in {"focus_changed", "keyboard_dispatch"}
                    or not isinstance(
                        progress.get("keyboard_dispatch_started"), bool
                    )
                    or not isinstance(progress.get("focus_changed"), bool)
                    or (
                        progress.get("phase") == "focus_changed"
                        and (
                            progress.get("keyboard_dispatch_started") is not False
                            or progress.get("focus_changed") is not True
                        )
                    )
                    or (
                        progress.get("phase") == "keyboard_dispatch"
                        and progress.get("keyboard_dispatch_started") is not True
                    )
                ):
                    raise self._channel_error(
                        "DRIVER.ACTION_FAILED",
                        "AX helper 返回了无效进度帧",
                        request_dispatched=True,
                        data={
                            "reason": "helper_protocol_error",
                            "keyboard_dispatch_started": keyboard_dispatch_started,
                            "focus_changed": focus_changed,
                        },
                    )
                keyboard_dispatch_started = progress["keyboard_dispatch_started"]
                focus_changed = progress["focus_changed"]
                continue
            break
        if (
            not isinstance(response, dict)
            or response.get("id") != request_id
            or not set(response).issubset({"id", "result", "error"})
        ):
            raise self._channel_error(
                "DRIVER.ACTION_FAILED",
                "AX helper 响应 ID 不匹配",
                request_dispatched=True,
                data={
                    "reason": "helper_protocol_error",
                    "keyboard_dispatch_started": keyboard_dispatch_started,
                    "focus_changed": focus_changed,
                },
            )
        has_error = "error" in response
        has_result = "result" in response
        if has_error == has_result:
            raise self._channel_error(
                "DRIVER.ACTION_FAILED",
                "AX helper 响应必须且只能包含 result 或 error",
                request_dispatched=True,
                data={
                    "reason": "helper_protocol_error",
                    "keyboard_dispatch_started": keyboard_dispatch_started,
                    "focus_changed": focus_changed,
                },
            )
        error = response.get("error")
        if has_error:
            if not isinstance(error, Mapping):
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 返回了无效错误帧",
                    request_dispatched=True,
                    data={
                        "reason": "helper_protocol_error",
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                    },
                )
            code = error.get("code")
            message = error.get("message")
            retryable = error.get("retryable", False)
            raw_data = error.get("data", {})
            if (
                not isinstance(code, str)
                or not code
                or len(code) > 256
                or not (code.startswith("DRIVER.") or code.startswith("PROTOCOL."))
                or not isinstance(message, str)
                or len(message) > MAX_FIELD_CHARS
                or not isinstance(retryable, bool)
                or not isinstance(raw_data, Mapping)
                or not set(error).issubset({"code", "message", "retryable", "data"})
            ):
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 返回了无效错误帧",
                    request_dispatched=True,
                    data={
                        "reason": "helper_protocol_error",
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                    },
                )
            details = _json_safe(raw_data)
            if not isinstance(details, dict):
                details = {}
            # These keys describe the Python transport boundary and cannot be
            # asserted by an untrusted helper response.
            details.pop("helper_channel_failure", None)
            details.pop("helper_request_dispatched", None)
            details.pop("effect", None)
            declared_keyboard_dispatch = details.get("keyboard_dispatch_started")
            if declared_keyboard_dispatch is not None and not isinstance(
                declared_keyboard_dispatch, bool
            ):
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 返回了无效派发状态",
                    request_dispatched=True,
                    data={
                        "reason": "helper_protocol_error",
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                    },
                )
            if declared_keyboard_dispatch is not None:
                keyboard_dispatch_started = declared_keyboard_dispatch
            declared_focus_changed = details.get("focus_changed")
            if declared_focus_changed is not None and not isinstance(declared_focus_changed, bool):
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 返回了无效焦点状态",
                    request_dispatched=True,
                    data={
                        "reason": "helper_protocol_error",
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                    },
                )
            if declared_focus_changed is not None:
                focus_changed = declared_focus_changed
            details["keyboard_dispatch_started"] = keyboard_dispatch_started
            details["focus_changed"] = focus_changed
            details["effect"] = (
                "unknown"
                if keyboard_dispatch_started
                else "contextual"
                if focus_changed
                else "not_applied"
            )
            details["helper_request_dispatched"] = True
            if require_keyboard_dispatch_state and declared_keyboard_dispatch is None:
                # Protocol v2 emits progress before native dispatch.  With no
                # observed marker, a missing final-state field is a fatal
                # protocol error but is not evidence that text was submitted.
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 错误缺少键盘派发状态",
                    request_dispatched=True,
                    data={
                        "reason": "helper_protocol_error",
                        "keyboard_dispatch_started": keyboard_dispatch_started,
                        "focus_changed": focus_changed,
                    },
                )
            if code.startswith("PROTOCOL."):
                details["helper_error_code"] = code
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 报告内部协议错误",
                    request_dispatched=True,
                    retryable=False,
                    data=details,
                )
            if code in {"DRIVER.OUTPUT_TOO_LARGE", "DRIVER.TIMEOUT"}:
                raise self._channel_error(
                    code,
                    message,
                    request_dispatched=True,
                    retryable=retryable,
                    data=details,
                )
            raise DriverError(
                code,
                message,
                retryable=retryable,
                data=details,
            )
        result = response["result"]
        if require_keyboard_dispatch_state:
            result_keyboard_started = (
                result.get("keyboard_dispatch_started")
                if isinstance(result, Mapping)
                else None
            )
            result_focus_changed = (
                result.get("focus_changed")
                if isinstance(result, Mapping)
                else None
            )
            if (
                not isinstance(result_keyboard_started, bool)
                or not isinstance(result_focus_changed, bool)
                or result_keyboard_started is not keyboard_dispatch_started
                or result_focus_changed is not focus_changed
            ):
                effective_keyboard_started = (
                    keyboard_dispatch_started or result_keyboard_started is True
                )
                effective_focus_changed = (
                    focus_changed or result_focus_changed is True
                )
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 结果与已观察的派发状态不一致",
                    request_dispatched=True,
                    data={
                        "reason": "helper_protocol_error",
                        "keyboard_dispatch_started": effective_keyboard_started,
                        "focus_changed": effective_focus_changed,
                        "effect": (
                            "unknown"
                            if effective_keyboard_started
                            else "contextual"
                            if effective_focus_changed
                            else "not_applied"
                        ),
                    },
                )
        return result

    def list_apps(self, *, deadline: float) -> Mapping[str, Any]:
        result = self._rpc("list_apps", {}, deadline=deadline)
        if (
            not isinstance(result, Mapping)
            or not set(result).issubset({"accessibility_trusted", "apps"})
            or not isinstance(result.get("accessibility_trusted"), bool)
            or isinstance(result.get("apps"), (str, bytes))
            or not isinstance(result.get("apps"), Sequence)
            or any(not isinstance(item, Mapping) for item in result.get("apps", ()))
        ):
            raise self._channel_error(
                "DRIVER.ACTION_FAILED",
                "AX helper 返回了无效应用列表",
                request_dispatched=True,
                data={"reason": "helper_protocol_error"},
            )
        for item in result["apps"]:
            allowed_app_fields = {
                "process_id",
                "bundle_id",
                "name",
                "active",
                "hidden",
                "activation_policy",
            }
            if not set(item).issubset(allowed_app_fields):
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 应用记录包含不允许的字段",
                    request_dispatched=True,
                    data={
                        "reason": "helper_protocol_error",
                        "fields": sorted(set(item) - allowed_app_fields),
                    },
                )
        return result

    def capture(
        self,
        app: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot:
        result = self._rpc(
            "snapshot",
            {"app": dict(app), "max_depth": max_depth, "max_nodes": max_nodes},
            deadline=deadline,
        )
        if (
            not isinstance(result, Mapping)
            or not set(result).issubset({"app", "nodes", "truncated"})
            or not isinstance(result.get("app"), Mapping)
            or not isinstance(result.get("truncated"), bool)
        ):
            raise self._channel_error(
                "DRIVER.ACTION_FAILED",
                "AX helper 返回了无效快照",
                request_dispatched=True,
                data={"reason": "helper_protocol_error"},
            )
        allowed_app_fields = {
            "process_id",
            "bundle_id",
            "name",
            "active",
            "hidden",
            "activation_policy",
        }
        if not set(result["app"]).issubset(allowed_app_fields):
            raise self._channel_error(
                "DRIVER.ACTION_FAILED",
                "AX helper 快照应用身份包含不允许的字段",
                request_dispatched=True,
                data={"reason": "helper_protocol_error"},
            )
        raw_nodes = result.get("nodes")
        if isinstance(raw_nodes, (str, bytes)) or not isinstance(raw_nodes, Sequence):
            raise self._channel_error(
                "DRIVER.ACTION_FAILED",
                "AX helper 返回了无效节点数组",
                request_dispatched=True,
                data={"reason": "helper_protocol_error"},
            )
        nodes: list[BackendNode] = []
        for item in raw_nodes:
            if (
                not isinstance(item, Mapping)
                or not set(item).issubset(
                    {
                        "native_token",
                        "parent_index",
                        "role",
                        "subrole",
                        "name",
                        "description",
                        "value",
                        "states",
                        "bounds",
                        "actions",
                        "provenance",
                    }
                )
                or not isinstance(item.get("role"), str)
                or (
                    item.get("parent_index") is not None
                    and (
                        isinstance(item.get("parent_index"), bool)
                        or not isinstance(item.get("parent_index"), int)
                    )
                )
            ):
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 返回了无效节点",
                    request_dispatched=True,
                    data={"reason": "helper_protocol_error"},
                )
            native = item.get("native_token")
            if not isinstance(native, str) or not native:
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 节点缺少原生 token",
                    request_dispatched=True,
                    data={"reason": "helper_protocol_error"},
                )
            nodes.append(
                BackendNode(
                    native=native,
                    parent_index=item.get("parent_index"),
                    role=_safe_text(item.get("role")) or "unknown",
                    subrole=_safe_text(item.get("subrole")),
                    name=_safe_text(item.get("name")),
                    description=_safe_text(item.get("description")),
                    value=_safe_text(item.get("value")),
                    states=item.get("states") if isinstance(item.get("states"), Mapping) else None,
                    bounds=item.get("bounds") if isinstance(item.get("bounds"), Mapping) else None,
                    actions=item.get("actions") if isinstance(item.get("actions"), list) else (),
                    provenance=item.get("provenance") if isinstance(item.get("provenance"), Mapping) else None,
                )
            )
        return BackendSnapshot(
            app=result["app"], nodes=nodes, truncated=result.get("truncated") is True
        )

    def focus(self, native: Any, *, deadline: float) -> Any:
        return self._write_rpc("focus", {"native_token": native}, deadline=deadline)

    def invoke(self, native: Any, *, deadline: float) -> Any:
        return self._write_rpc("invoke", {"native_token": native}, deadline=deadline)

    def set_value(self, native: Any, value: str, *, deadline: float) -> Any:
        return self._write_rpc(
            "set_value", {"native_token": native, "value": value}, deadline=deadline
        )

    def type_text(self, native: Any, text: str, *, deadline: float) -> Any:
        return self._write_rpc(
            "type_text", {"native_token": native, "text": text}, deadline=deadline
        )

    def _write_rpc(
        self, operation: str, args: Mapping[str, Any], *, deadline: float
    ) -> Mapping[str, Any]:
        try:
            result = self._rpc(
                operation, args, deadline=deadline,
                require_keyboard_dispatch_state=operation == "type_text",
            )
            expected_flag = "submitted" if operation == "type_text" else "accepted"
            allowed_result_fields = {expected_flag, "native_operation"}
            if operation == "type_text":
                allowed_result_fields.update(
                    {"keyboard_dispatch_started", "focus_changed", "phase"}
                )
            if (
                not isinstance(result, Mapping)
                or not set(result).issubset(allowed_result_fields)
                or result.get(expected_flag) is not True
                or not isinstance(result.get("native_operation"), str)
                or (
                    operation == "type_text"
                    and (
                        result.get("keyboard_dispatch_started") is not True
                        or not isinstance(result.get("focus_changed"), bool)
                        or result.get("phase") != "submitted"
                    )
                )
            ):
                raise self._channel_error(
                    "DRIVER.ACTION_FAILED",
                    "AX helper 返回了无效写动作结果",
                    request_dispatched=True,
                    data={"reason": "helper_protocol_error", "operation": operation},
                )
        except DriverError as exc:
            details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
            if (
                operation == "type_text"
                and details.get("keyboard_dispatch_started") is True
            ):
                # The helper emits this marker immediately before the first
                # keyDown post.  Only that native boundary makes text effect
                # unknown; a complete request frame alone is insufficient.
                self._close_process(force=True)
                details["effect"] = "unknown"
                details["helper_terminated"] = True
                details.setdefault("operation", operation)
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "AX helper 键盘输入请求派发后的结果未知",
                    data=details,
                ) from exc
            if operation == "type_text":
                details["effect"] = (
                    "contextual" if details.get("focus_changed") is True else "not_applied"
                )
                raise DriverError(
                    exc.code, exc.message, retryable=exc.retryable, data=details
                ) from exc
            if (
                details.get("helper_channel_failure") is True
                and details.get("helper_request_dispatched") is True
            ):
                details["effect"] = "unknown"
                details.setdefault("operation", operation)
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "AX helper 写请求完成后通道结果未知",
                    data=details,
                ) from exc
            raise
        return result

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool:
        result = self._rpc(
            "same_element",
            {"previous_token": previous, "current_token": current},
            deadline=deadline,
        )
        if (
            not isinstance(result, Mapping)
            or set(result) != {"same"}
            or not isinstance(result.get("same"), bool)
        ):
            raise self._channel_error(
                "DRIVER.ACTION_FAILED",
                "AX helper 返回了无效身份比较结果",
                request_dispatched=True,
                data={"reason": "helper_protocol_error"},
            )
        return result["same"]


def create_default_backend() -> AXBackend:
    if sys.platform != "darwin":
        return UnavailableBackend("platform", platform=sys.platform, required_platform="darwin")
    try:
        return SwiftHelperBackend()
    except DriverError as exc:
        return UnavailableBackend(
            "helper_initialization",
            cause_code=exc.code,
            cause_data=exc.data if isinstance(exc.data, dict) else {},
        )


def _wire_deadline(value: Any) -> float:
    if value is None:
        return time.monotonic() + DEFAULT_REQUEST_SECONDS
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        _fail("DRIVER.INVALID_REQUEST", "deadline_ms 必须是 Unix 毫秒时间戳")
    remaining = float(value) / 1000.0 - time.time()
    if not math.isfinite(remaining) or remaining <= 0:
        raise DriverError(
            "DRIVER.TIMEOUT",
            "请求在派发前已超过截止时间",
            retryable=True,
            data={"phase": "before_dispatch", "effect": "not_applied"},
        )
    return time.monotonic() + remaining


def _encode(message: Mapping[str, Any]) -> bytes:
    return (
        json.dumps(message, ensure_ascii=True, allow_nan=False, separators=(",", ":"))
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
                    "message": "规范化响应超过 NDJSON 帧限制",
                    "retryable": False,
                    "data": {"limit_bytes": MAX_RESPONSE_BYTES},
                },
            }
        )
    sys.stdout.buffer.write(encoded)
    sys.stdout.buffer.flush()


def debug(message: str) -> None:
    sys.stderr.buffer.write(
        f"[{PLUGIN_NAME}] {message}\n".encode("utf-8", errors="replace")
    )
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


def handle_request(request: Any, driver: MacOSAXDriver) -> None:
    request_id: Any = request.get("id") if isinstance(request, dict) else None
    try:
        if not isinstance(request, dict):
            raise DriverError("PROTOCOL.INVALID_REQUEST", "request 必须是 JSON 对象")
        if not isinstance(request_id, str) or not request_id or len(request_id) > 256:
            raise DriverError("PROTOCOL.INVALID_REQUEST", "request.id 必须是有界非空字符串")
        request_type = request.get("type")
        if request_type == "manifest":
            emit({"id": request_id, "result": MANIFEST})
            return
        action_id = request.get("action")
        if request_type != "invoke" or action_id not in ACTION_NAMES:
            raise DriverError(
                "PROTOCOL.ACTION_NOT_FOUND",
                f"未知动作：{action_id}",
                data={"action": action_id, "available_actions": list(ACTION_NAMES)},
            )
        deadline = _wire_deadline(request.get("deadline_ms"))
        result = driver.execute(ACTION_NAMES[action_id], request.get("args"), deadline=deadline)
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
                "macOS AX driver 遇到内部错误",
                data={"exception_type": type(exc).__name__},
            ),
        )


def _discard_until_newline(stream: Any) -> None:
    while True:
        chunk = stream.readline(MAX_REQUEST_BYTES + 1)
        if not chunk or chunk.endswith(b"\n"):
            return


def serve(driver: MacOSAXDriver, stream: Any = None) -> None:
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
                    "请求超过 NDJSON 帧限制",
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
                    "请求必须是有效 UTF-8",
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
                    "请求行不是有效 JSON",
                    data={"line": exc.lineno, "column": exc.colno},
                ),
            )
            continue
        handle_request(request, driver)


def main() -> int:
    driver = MacOSAXDriver()
    if "--manifest" in sys.argv[1:]:
        emit({"type": "manifest", "manifest": MANIFEST})
    debug(f"started pid={os.getpid()} platform={sys.platform} backend={_backend_name(driver.backend)}")
    try:
        serve(driver)
    finally:
        close = getattr(driver.backend, "close", None)
        if callable(close):
            close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
