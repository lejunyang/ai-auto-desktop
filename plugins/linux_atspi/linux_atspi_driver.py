#!/usr/bin/env python3
"""Linux AT-SPI 进程驱动。

进程边界使用仓库约定的 UTF-8 NDJSON 协议。驱动核心不依赖 PyGObject，
因此可以在任意平台注入 fake backend 验证定位、快照与过期目标规则。
除显式 ``type_text`` 的固定路径 XTest helper 外，本驱动只调用 AT-SPI 原生语义接口；
不注入指针或坐标事件。
"""

from __future__ import annotations

from collections import deque
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import re
import selectors
import stat
import struct
import subprocess
import sys
import time
from typing import Any, NoReturn, Protocol
import uuid

from ai_auto_desktop.artifact_ipc import (
    ArtifactIPCError,
    WorkerArtifactInvocation,
)


PLUGIN_NAME = "desktop.linux_atspi"
PLUGIN_VERSION = "0.1.0"
MAX_REQUEST_BYTES = 1024 * 1024
MAX_RESPONSE_BYTES = 8 * 1024 * 1024 - 1
MAX_FIELD_CHARS = 4096
MAX_TYPE_TEXT_CHARS = 1024
MAX_TYPE_TEXT_BYTES = 4096
DEFAULT_REQUEST_SECONDS = 30.0
DEFAULT_MAX_DEPTH = 32
DEFAULT_MAX_NODES = 1000
MAX_DEPTH = 128
MAX_NODES = 5000
MAX_CANDIDATE_SUMMARIES = 10
XTEST_HELPER_PATH = Path(__file__).resolve().parent / ".build" / "x11_xtest_helper"
XTEST_HELPER_MAX_OUTPUT_BYTES = 64 * 1024
X11_CAPTURE_HELPER_PATH = (
    Path(__file__).resolve().parent / ".build" / "x11_capture_helper"
)
CAPTURE_MAX_BYTES = 64 * 1024 * 1024
CAPTURE_HELPER_MAX_METADATA_BYTES = 64 * 1024
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
TYPE_TEXT_ROLES = frozenset({"entry", "text"})
# D-Bus 在返回 ``a(so)`` 后才交给 Python 解包；该上限不能避免总线已传输的
# 数据，但会在解包后立即拒绝异常 fan-out，避免继续复制或遍历无界列表。
MAX_DBUS_CHILDREN_PER_CALL = MAX_NODES
GTK3_NATIVE_ACTION_NAMES = {
    "toggle": "click",
    "expand": "activate",
    "collapse": "activate",
}
QT5_NATIVE_ACTION_NAMES = {
    "invoke": "Press",
}
POINTER_BUTTONS = frozenset({"left"})
POINTER_POSITIONS = frozenset({"center"})

ACTION_IDS = {
    name: f"{PLUGIN_NAME}.{name}@1"
    for name in (
        "inspect_session",
        "list_applications",
        "snapshot",
        "find",
        "capture_target",
        "focus",
        "invoke",
        "pointer_click",
        "set_text",
        "type_text",
        "toggle",
        "expand",
        "collapse",
    )
}
ACTION_NAMES = {full_name: short_name for short_name, full_name in ACTION_IDS.items()}
WRITE_ACTIONS = frozenset(
    {
        "focus",
        "invoke",
        "pointer_click",
        "set_text",
        "type_text",
        "toggle",
        "expand",
        "collapse",
    }
)
NODE_ACTIONS = WRITE_ACTIONS
STATE_NAMES = (
    "enabled",
    "visible",
    "showing",
    "focusable",
    "focused",
    "editable",
    "sensitive",
    "protected",
    "checked",
    "expandable",
    "expanded",
    "selectable",
    "selected",
)
APPLICATION_SELECTOR_FIELDS = frozenset(
    {"bus_name", "name", "process_id", "toolkit_name"}
)


LOCATOR_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "role": {"type": "string", "maxLength": 256},
        "name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "description": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS},
        "value": {"type": ["string", "null"], "maxLength": MAX_FIELD_CHARS},
        "bus_name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "object_path": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "toolkit_name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "attributes": {
            "type": "object",
            "minProperties": 1,
            "additionalProperties": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        },
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
    ("DRIVER.UNAVAILABLE", "Linux AT-SPI 后端不可用。", False),
    ("DRIVER.ACTION_FAILED", "AT-SPI 原生操作失败。", False),
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
    ("DRIVER.ACTION_UNSUPPORTED", "目标缺少所需 AT-SPI 接口。", False),
    ("DRIVER.PROTECTED_ELEMENT", "目标是受保护元素。", False),
    ("DRIVER.UNKNOWN_EFFECT", "原生动作可能已生效。", False),
)
CAPTURE_ERRORS = (
    ("DRIVER.CAPTURE_FAILED", "X11 目标截图失败。", False),
    ("DRIVER.ARTIFACT_IPC", "截图 artifact 传输失败。", False),
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
    sensitivity: Mapping[str, str] | None = None,
    durability: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    contract: dict[str, Any] = {
        "contract_major": 1,
        "description": description,
        "effect": {"default_class": effect},
        "risk": {"category": risk_category, "level": risk_level},
        "permissions": list(permissions),
        "input_schema": input_schema,
        "output_schema": output_schema,
        "errors": _error_contracts(errors),
    }
    if sensitivity is not None:
        contract["sensitivity"] = dict(sensitivity)
    if durability is not None:
        contract["durability"] = copy.deepcopy(dict(durability))
    return contract


APPLICATION_SELECTOR_SCHEMA: dict[str, Any] = {
    "type": "object",
    "minProperties": 1,
    "properties": {
        "bus_name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
        "process_id": {"type": "integer", "minimum": 0},
        "toolkit_name": {"type": "string", "maxLength": MAX_FIELD_CHARS},
    },
    "additionalProperties": False,
}

SNAPSHOT_OUTPUT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": [
        "snapshot_id",
        "revision",
        "session",
        "backend",
        "application",
        "nodes",
        "truncated",
    ],
    "properties": {
        "snapshot_id": {"type": "string"},
        "revision": {"type": "integer", "minimum": 1},
        "session": {"type": "object"},
        "backend": {"type": "string"},
        "application": {"type": "object"},
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
CAPTURE_INPUT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["target", "locator", "format"],
    "properties": {
        "target": TARGET_SCHEMA,
        "locator": LOCATOR_SCHEMA,
        "format": {"const": "png"},
    },
    "additionalProperties": False,
}
CAPTURE_PROVENANCE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": [
        "capture_method",
        "snapshot_id",
        "revision",
        "node_id",
        "application_process_id",
        "format",
        "mime_type",
        "target_process_id",
        "target_window",
        "target_top_level_window",
        "root_window",
        "bounds",
        "root_size",
        "cursor_included",
        "occlusion_checked",
        "same_euid_verified",
        "scene_stable",
    ],
    "properties": {
        "capture_method": {"const": "x11_root_xgetimage"},
        "snapshot_id": {"type": "string", "minLength": 1},
        "revision": {"type": "integer", "minimum": 1},
        "node_id": {"type": "string", "minLength": 1},
        "application_process_id": {"type": "integer", "minimum": 1},
        "format": {"const": "png"},
        "mime_type": {"const": "image/png"},
        "target_process_id": {"type": "integer", "minimum": 1},
        "target_window": {"type": "integer", "minimum": 1},
        "target_top_level_window": {"type": "integer", "minimum": 1},
        "root_window": {"type": "integer", "minimum": 1},
        "bounds": {
            "type": "object",
            "required": ["x", "y", "width", "height"],
            "properties": {
                "x": {"type": "integer"},
                "y": {"type": "integer"},
                "width": {"type": "integer", "minimum": 1},
                "height": {"type": "integer", "minimum": 1},
            },
            "additionalProperties": False,
        },
        "root_size": {
            "type": "object",
            "required": ["width", "height"],
            "properties": {
                "width": {"type": "integer", "minimum": 1},
                "height": {"type": "integer", "minimum": 1},
            },
            "additionalProperties": False,
        },
        "cursor_included": {"const": False},
        "occlusion_checked": {"const": True},
        "same_euid_verified": {"const": True},
        "scene_stable": {"const": True},
    },
    "additionalProperties": False,
}
CAPTURE_OUTPUT_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["frame", "provenance"],
    "properties": {
        "frame": {"type": "object"},
        "provenance": CAPTURE_PROVENANCE_SCHEMA,
    },
    "additionalProperties": False,
}

ACTION_CONTRACTS: dict[str, dict[str, Any]] = {
    "inspect_session": _contract(
        "返回不含应用内容的当前 AT-SPI backend 与桌面会话类型。",
        effect="read_only",
        risk_category="observe",
        risk_level="low",
        input_schema={"type": "object", "additionalProperties": False},
        output_schema={
            "type": "object",
            "required": ["backend", "session_type", "desktop"],
            "properties": {
                "backend": {"type": "string", "maxLength": 128},
                "session_type": {"type": ["string", "null"], "maxLength": 128},
                "desktop": {"type": ["string", "null"], "maxLength": 256},
            },
            "additionalProperties": False,
        },
        errors=COMMON_ERRORS,
        permissions=("desktop.observe",),
        sensitivity={
            "input": "public", "output": "public", "error": "public"
        },
        durability={
            "checkpoint_fields": {
                "backend": {
                    "pointer": "/backend",
                    "schema": {"type": "string", "maxLength": 128},
                },
                "session_type": {
                    "pointer": "/session_type",
                    "schema": {"type": ["string", "null"], "maxLength": 128},
                },
                "desktop": {
                    "pointer": "/desktop",
                    "schema": {"type": ["string", "null"], "maxLength": 256},
                },
            }
        },
    ),
    "list_applications": _contract(
        "通过 AT-SPI desktop 根节点枚举当前会话的桌面应用。",
        effect="read_only",
        risk_category="observe",
        risk_level="low",
        input_schema={"type": "object", "additionalProperties": False},
        output_schema={
            "type": "object",
            "required": ["session", "backend", "applications"],
            "properties": {
                "session": {"type": "object"},
                "backend": {"type": "string"},
                "applications": {"type": "array", "items": {"type": "object"}},
            },
            "additionalProperties": False,
        },
        errors=COMMON_ERRORS,
        permissions=("desktop.observe",),
    ),
    "snapshot": _contract(
        "抓取一个精确应用选择器对应的有界 AT-SPI 可访问性树。",
        effect="read_only",
        risk_category="observe",
        risk_level="low",
        input_schema={
            "type": "object",
            "required": ["application"],
            "properties": {
                "application": APPLICATION_SELECTOR_SCHEMA,
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
    "capture_target": {
        **_contract(
            (
                "重新验证精确 AT-SPI 目标后，经固定 X11 helper 截取其 fresh "
                "screen bounds；仅输出 host 托管 PNG artifact 与可审计 provenance。"
            ),
            effect="read_only",
            risk_category="observe",
            risk_level="medium",
            input_schema=CAPTURE_INPUT_SCHEMA,
            output_schema=CAPTURE_OUTPUT_SCHEMA,
            errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS[:2] + CAPTURE_ERRORS,
            permissions=("desktop.observe", "desktop.capture"),
            sensitivity={"input": "public", "output": "sensitive", "error": "public"},
        ),
        "artifacts": {
            "outputs": {
                "frame": {
                    "pointer": "/frame",
                    "media_types": ["image/png"],
                    "max_size_bytes": CAPTURE_MAX_BYTES,
                }
            }
        },
    },
    "focus": _contract(
        "重新验证目标后调用 AT-SPI Component.grab_focus。",
        effect="contextual",
        risk_category="navigate",
        risk_level="medium",
        input_schema=COMMON_WRITE_INPUT,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "invoke": _contract(
        "重新验证目标后调用 AT-SPI Action.do_action。",
        effect="non_idempotent",
        risk_category="modify",
        risk_level="high",
        input_schema=COMMON_WRITE_INPUT,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "pointer_click": _contract(
        (
            "重新验证目标后，仅在明确的 KDE/X11 会话中，通过固定路径 XTest helper "
            "按目标 bounds 中心点显式执行左键单击。"
        ),
        effect="non_idempotent",
        risk_category="modify",
        risk_level="high",
        input_schema=POINTER_CLICK_INPUT_SCHEMA,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "set_text": _contract(
        "重新验证目标后调用 AT-SPI EditableText.set_text_contents。",
        effect="contextual",
        risk_category="input",
        risk_level="high",
        input_schema={
            "type": "object",
            "required": ["target", "locator", "text"],
            "properties": {
                "target": TARGET_SCHEMA,
                "locator": LOCATOR_SCHEMA,
                "text": {"type": "string", "maxLength": MAX_FIELD_CHARS},
            },
            "additionalProperties": False,
        },
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "type_text": _contract(
        "重新验证并聚焦目标后，通过受限 KDE/X11 XTest helper 显式输入普通 UTF-8 文本。",
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
    "toggle": _contract(
        "重新验证 GTK3 目标及 checked 状态后，精确调用名为 click 的 AT-SPI 动作。",
        effect="non_idempotent",
        risk_category="modify",
        risk_level="high",
        input_schema=COMMON_WRITE_INPUT,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "expand": _contract(
        "重新验证 GTK3 目标后，在需要时精确调用名为 activate 的 AT-SPI 动作以展开。",
        effect="idempotent",
        risk_category="modify",
        risk_level="medium",
        input_schema=COMMON_WRITE_INPUT,
        output_schema=WRITE_OUTPUT_SCHEMA,
        errors=COMMON_ERRORS + LOCATOR_ERRORS + ACTION_ERRORS,
        permissions=("desktop.observe", "desktop.input"),
    ),
    "collapse": _contract(
        "重新验证 GTK3 目标后，在需要时精确调用名为 activate 的 AT-SPI 动作以折叠。",
        effect="idempotent",
        risk_category="modify",
        risk_level="medium",
        input_schema=COMMON_WRITE_INPUT,
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
        "description": "Linux 原生 AT-SPI 语义桌面驱动。",
    },
    "actions": ACTION_CONTRACTS,
    "runtime": {
        "kind": "process",
        "protocol": "ndjson-stdio-v1",
        "entrypoint": "./run.sh",
        "platforms": ["linux"],
    },
}


class DriverError(Exception):
    """通过进程协议返回的稳定错误。"""

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
    """后端节点；``native`` 永远不会跨越 NDJSON 边界。"""

    native: Any
    parent_index: int | None
    role: str
    name: str | None = None
    description: str | None = None
    value: str | None = None
    attributes: Mapping[str, str] | None = None
    states: Mapping[str, bool | None] | None = None
    bounds: Mapping[str, int] | None = None
    actions: Sequence[str] = ()
    provenance: Mapping[str, Any] | None = None


@dataclass(slots=True)
class BackendSnapshot:
    """后端返回的单应用可访问性树。"""

    application: Mapping[str, Any]
    nodes: Sequence[BackendNode]
    truncated: bool = False


class AtspiBackend(Protocol):
    """驱动核心使用的最小原生边界。"""

    name: str

    def session_info(self) -> Mapping[str, Any]: ...

    def list_applications(self, *, deadline: float) -> Sequence[Mapping[str, Any]]: ...

    def capture(
        self,
        application: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot: ...

    def focus(self, native: Any, *, deadline: float) -> Any: ...

    def invoke(self, native: Any, *, deadline: float) -> Any: ...

    def set_text(self, native: Any, text: str, *, deadline: float) -> Any: ...

    def toggle(self, native: Any, *, deadline: float) -> Any: ...

    def expand(self, native: Any, *, deadline: float) -> Any: ...

    def collapse(self, native: Any, *, deadline: float) -> Any: ...

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool: ...

    def accessible_at_point(
        self, root: Any, x: int, y: int, *, deadline: float
    ) -> Any | None: ...


@dataclass(slots=True)
class _SnapshotRecord:
    public: dict[str, Any]
    handles: dict[str, Any]
    fingerprints: dict[str, str]
    application_selector: dict[str, Any]
    max_depth: int
    max_nodes: int


@dataclass(slots=True)
class _FreshTarget:
    previous: _SnapshotRecord
    fresh: _SnapshotRecord
    previous_node_id: str
    node_id: str
    node: dict[str, Any]


class XTestHelper:
    """固定路径、单次进程的 X11/XTest 键盘注入边界。"""

    def __init__(self, path: Path = XTEST_HELPER_PATH) -> None:
        self.path = path

    @staticmethod
    def _qualified_environment() -> dict[str, str]:
        session = _environment_session_info()
        session_type = str(session.get("session_type") or "").strip().lower()
        desktop_entries = {
            item.upper()
            for item in re.split(r"[:;]", str(session.get("desktop") or ""))
            if item
        }
        if session_type != "x11" or not session.get("display") or "KDE" not in desktop_entries:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "type_text 仅支持明确的 KDE/X11/DISPLAY 会话",
                data={
                    "reason": "unsupported_session",
                    "required_session_type": "x11",
                    "required_desktop": "KDE",
                    "session": session,
                },
            )
        allowed = (
            "DISPLAY",
            "XAUTHORITY",
            "XDG_SESSION_TYPE",
            "XDG_CURRENT_DESKTOP",
            "DESKTOP_SESSION",
        )
        return {name: os.environ[name] for name in allowed if os.environ.get(name)}

    def _validated_path(self) -> str:
        try:
            details = self.path.lstat()
        except OSError as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "XTest helper 尚未构建",
                data={
                    "reason": "helper_missing",
                    "build_command": "plugins/linux_atspi/build_x11_xtest_helper.sh",
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        unsafe_mode = bool(details.st_mode & (stat.S_IWGRP | stat.S_IWOTH))
        if (
            not stat.S_ISREG(details.st_mode)
            or details.st_uid != os.geteuid()
            or unsafe_mode
            or not os.access(self.path, os.X_OK)
        ):
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "XTest helper 身份或权限不可信",
                data={
                    "reason": "helper_untrusted",
                    "regular": stat.S_ISREG(details.st_mode),
                    "owner_matches": details.st_uid == os.geteuid(),
                    "group_or_world_writable": unsafe_mode,
                    "executable": os.access(self.path, os.X_OK),
                },
            )
        return str(self.path)

    def preflight(self) -> None:
        self._qualified_environment()
        self._validated_path()

    @staticmethod
    def _parse_output(output: bytes) -> tuple[bool, dict[str, Any] | None]:
        if len(output) > XTEST_HELPER_MAX_OUTPUT_BYTES:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "XTest helper 输出超过限制",
                data={"reason": "helper_output_too_large"},
            )
        dispatch_started = False
        result: dict[str, Any] | None = None
        for raw_line in output.splitlines():
            try:
                decoded = json.loads(raw_line.decode("utf-8", errors="strict"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if not isinstance(decoded, dict):
                continue
            if decoded.get("event") == "dispatch_started":
                dispatch_started = True
            elif "ok" in decoded:
                result = decoded
                dispatch_started = dispatch_started or decoded.get("dispatch_started") is True
        return dispatch_started, result

    def type_text(
        self, text: str, *, expected_process_id: int, deadline: float
    ) -> dict[str, Any]:
        environment = self._qualified_environment()
        executable = self._validated_path()
        _check_deadline(deadline)
        command = [
            executable,
            "type-text",
            "--expected-pid",
            str(expected_process_id),
            "--deadline-monotonic-ns",
            str(int(deadline * 1_000_000_000)),
        ]
        try:
            process = subprocess.Popen(
                command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=str(self.path.parent),
                env=environment,
                close_fds=True,
            )
        except OSError as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "无法启动固定路径 XTest helper",
                data={"reason": "helper_start_failed", "exception_type": type(exc).__name__},
            ) from exc
        encoded = text.encode("utf-8", errors="strict")
        try:
            stdout, _stderr = process.communicate(
                input=encoded, timeout=max(0.001, deadline - time.monotonic())
            )
        except subprocess.TimeoutExpired as exc:
            partial = exc.output if isinstance(exc.output, bytes) else b""
            process.kill()
            tail, _stderr = process.communicate()
            stdout = partial + tail
            dispatch_started, _result = self._parse_output(stdout)
            if dispatch_started:
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "XTest helper 在首个输入事件后超时",
                    retryable=False,
                    data={
                        "phase": "keyboard_dispatch",
                        "dispatch_started": True,
                        "effect": "unknown",
                    },
                ) from exc
            raise DriverError(
                "DRIVER.TIMEOUT",
                "XTest helper 在首个输入事件前超时",
                retryable=True,
                data={
                    "phase": "before_input_dispatch",
                    "dispatch_started": False,
                    "effect": "not_applied",
                },
            ) from exc
        dispatch_started, result = self._parse_output(stdout)
        if process.returncode == 0 and result is not None and result.get("ok") is True:
            if not dispatch_started:
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    "XTest helper 未证明输入事件已派发",
                    data={"reason": "missing_dispatch_marker"},
                )
            events = result.get("events")
            codepoints = result.get("codepoints")
            if (
                isinstance(events, bool)
                or not isinstance(events, int)
                or events < 2
                or isinstance(codepoints, bool)
                or not isinstance(codepoints, int)
                or codepoints < 1
            ):
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "XTest helper 在输入派发后返回无效结果",
                    data={
                        "phase": "post_dispatch",
                        "dispatch_started": True,
                        "effect": "unknown",
                    },
                )
            return {
                "native_interface": "XTEST",
                "synthetic_input": True,
                "submitted": True,
                "events": events,
                "codepoints": codepoints,
                "expected_process_id": expected_process_id,
            }
        helper_code = result.get("code") if isinstance(result, dict) else None
        helper_phase = result.get("phase") if isinstance(result, dict) else None
        details = {
            "helper_exit_code": process.returncode,
            "helper_code": helper_code,
            "phase": helper_phase or ("keyboard_dispatch" if dispatch_started else "pre_dispatch"),
            "dispatch_started": dispatch_started,
            "effect": "unknown" if dispatch_started else "not_applied",
        }
        if dispatch_started or process.returncode == 70:
            raise DriverError(
                "DRIVER.UNKNOWN_EFFECT",
                "XTest helper 在首个输入事件后失败",
                retryable=False,
                data=details,
            )
        if process.returncode == 69:
            raise DriverError("DRIVER.UNAVAILABLE", "X11/XTest 不可用", data=details)
        if process.returncode == 74:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED", "当前 X11 键盘映射不支持完整文本", data=details
            )
        if process.returncode == 75:
            raise DriverError(
                "DRIVER.TIMEOUT", "XTest helper 在输入派发前超时", retryable=True, data=details
            )
        raise DriverError(
            "DRIVER.ACTION_FAILED",
            "XTest helper 未派发输入并失败关闭",
            data=details,
        )

    def pointer_click(
        self,
        *,
        expected_process_id: int,
        x: int,
        y: int,
        deadline: float,
    ) -> dict[str, Any]:
        environment = self._qualified_environment()
        executable = self._validated_path()
        _check_deadline(deadline)
        command = [
            executable,
            "pointer-click",
            "--expected-pid",
            str(expected_process_id),
            "--x",
            str(x),
            "--y",
            str(y),
            "--deadline-monotonic-ns",
            str(int(deadline * 1_000_000_000)),
        ]
        try:
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=str(self.path.parent),
                env=environment,
                close_fds=True,
            )
        except OSError as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "无法启动固定路径 XTest helper",
                data={"reason": "helper_start_failed", "exception_type": type(exc).__name__},
            ) from exc
        try:
            stdout, _stderr = process.communicate(timeout=max(0.001, deadline - time.monotonic()))
        except subprocess.TimeoutExpired as exc:
            partial = exc.output if isinstance(exc.output, bytes) else b""
            process.kill()
            tail, _stderr = process.communicate()
            stdout = partial + tail
            dispatch_started, _result = self._parse_output(stdout)
            if dispatch_started:
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "XTest helper 在首个 pointer 事件后超时",
                    retryable=False,
                    data={
                        "phase": "pointer_dispatch",
                        "dispatch_started": True,
                        "effect": "unknown",
                    },
                ) from exc
            raise DriverError(
                "DRIVER.TIMEOUT",
                "XTest helper 在首个 pointer 事件前超时",
                retryable=True,
                data={
                    "phase": "before_pointer_dispatch",
                    "dispatch_started": False,
                    "effect": "not_applied",
                },
            ) from exc
        dispatch_started, result = self._parse_output(stdout)
        if process.returncode == 0 and result is not None and result.get("ok") is True:
            if not dispatch_started:
                raise DriverError(
                    "DRIVER.ACTION_FAILED",
                    "XTest helper 未证明 pointer 事件已派发",
                    data={"reason": "missing_dispatch_marker"},
                )
            events = result.get("events")
            if isinstance(events, bool) or not isinstance(events, int) or events < 3:
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "XTest helper 在 pointer 派发后返回无效结果",
                    data={
                        "phase": "post_dispatch",
                        "dispatch_started": True,
                        "effect": "unknown",
                    },
                )
            return {
                "native_interface": "XTEST",
                "synthetic_input": True,
                "submitted": True,
                "events": events,
                "expected_process_id": expected_process_id,
                "click_point": {"x": x, "y": y},
            }
        helper_code = result.get("code") if isinstance(result, dict) else None
        helper_phase = result.get("phase") if isinstance(result, dict) else None
        details = {
            "helper_exit_code": process.returncode,
            "helper_code": helper_code,
            "phase": helper_phase or ("pointer_dispatch" if dispatch_started else "pre_dispatch"),
            "dispatch_started": dispatch_started,
            "effect": "unknown" if dispatch_started else "not_applied",
        }
        if dispatch_started or process.returncode == 70:
            raise DriverError(
                "DRIVER.UNKNOWN_EFFECT",
                "XTest helper 在首个 pointer 事件后失败",
                retryable=False,
                data=details,
            )
        if process.returncode == 69:
            raise DriverError("DRIVER.UNAVAILABLE", "X11/XTest 不可用", data=details)
        if process.returncode == 75:
            raise DriverError(
                "DRIVER.TIMEOUT",
                "XTest helper 在 pointer 派发前超时",
                retryable=True,
                data=details,
            )
        raise DriverError(
            "DRIVER.ACTION_FAILED",
            "XTest helper 未派发 pointer 并失败关闭",
            data=details,
        )


@dataclass(frozen=True, slots=True)
class CaptureResult:
    """由固定 X11 helper 返回的有界 PNG 与已验证 capture metadata。"""

    png: bytes
    metadata: dict[str, Any]


class X11CaptureHelper:
    """固定路径、单次进程的 X11 target capture 边界。"""

    _SUCCESS_FIELDS = frozenset(
        {
            "ok",
            "schema_version",
            "capture_method",
            "format",
            "mime_type",
            "expected_pid",
            "target_pid",
            "target_window",
            "target_top_level_window",
            "root_window",
            "x",
            "y",
            "width",
            "height",
            "root_width",
            "root_height",
            "png_bytes",
            "cursor_included",
            "occlusion_checked",
            "same_euid_verified",
            "scene_stable",
        }
    )
    _FAILURE_FIELDS = frozenset({"ok", "schema_version", "code", "phase"})
    _FAILURE_CONTEXT_FIELDS = frozenset(
        {"expected_pid", "x", "y", "width", "height"}
    )

    def __init__(self, path: Path = X11_CAPTURE_HELPER_PATH) -> None:
        self.path = path

    @staticmethod
    def _qualified_environment() -> dict[str, str]:
        session = _environment_session_info()
        session_type = str(session.get("session_type") or "").strip().lower()
        if session_type != "x11" or not session.get("display"):
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "capture_target 仅支持明确的 X11/DISPLAY 会话",
                data={
                    "reason": "unsupported_session",
                    "required_session_type": "x11",
                    "session": session,
                },
            )
        allowed = (
            "DISPLAY",
            "XAUTHORITY",
            "XDG_SESSION_TYPE",
            "XDG_CURRENT_DESKTOP",
            "DESKTOP_SESSION",
        )
        return {name: os.environ[name] for name in allowed if os.environ.get(name)}

    def _validated_path(self) -> str:
        try:
            details = self.path.lstat()
        except OSError as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "X11 capture helper 尚未构建",
                data={
                    "reason": "capture_helper_missing",
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        unsafe_mode = bool(details.st_mode & (stat.S_IWGRP | stat.S_IWOTH))
        if (
            not stat.S_ISREG(details.st_mode)
            or details.st_uid != os.geteuid()
            or unsafe_mode
            or not os.access(self.path, os.X_OK)
        ):
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "X11 capture helper 身份或权限不可信",
                data={
                    "reason": "capture_helper_untrusted",
                    "regular": stat.S_ISREG(details.st_mode),
                    "owner_matches": details.st_uid == os.geteuid(),
                    "group_or_world_writable": unsafe_mode,
                    "executable": os.access(self.path, os.X_OK),
                },
            )
        return str(self.path)

    def _open_validated_executable(self) -> int:
        """Open the verified helper inode so pathname replacement cannot race exec."""

        self._validated_path()
        flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        descriptor = -1
        try:
            descriptor = os.open(self.path, flags)
            details = os.fstat(descriptor)
            if (
                not stat.S_ISREG(details.st_mode)
                or details.st_uid != os.geteuid()
                or bool(details.st_mode & (stat.S_IWGRP | stat.S_IWOTH))
                or not bool(details.st_mode & stat.S_IXUSR)
            ):
                raise OSError
            return descriptor
        except (OSError, ValueError):
            if descriptor >= 0:
                os.close(descriptor)
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "X11 capture helper 无法通过已验证描述符启动",
                data={"reason": "capture_helper_untrusted"},
            ) from None

    @staticmethod
    def _metadata(stderr: bytes) -> dict[str, Any]:
        if len(stderr) > CAPTURE_HELPER_MAX_METADATA_BYTES:
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper metadata 超过限制",
                data={"reason": "helper_metadata_too_large"},
            )
        lines = stderr.splitlines()
        if len(lines) != 1:
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper 未返回唯一 metadata 记录",
                data={"reason": "invalid_helper_metadata"},
            )
        def closed_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
            result: dict[str, Any] = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError("duplicate metadata field")
                result[key] = value
            return result

        try:
            value = json.loads(
                lines[0].decode("utf-8", errors="strict"),
                object_pairs_hook=closed_object,
            )
        except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as exc:
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper metadata 无效",
                data={"reason": "invalid_helper_metadata"},
            ) from exc
        if not isinstance(value, dict):
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper metadata 必须是对象",
                data={"reason": "invalid_helper_metadata"},
            )
        return value

    @staticmethod
    def _positive_integer(value: Any) -> bool:
        return not isinstance(value, bool) and isinstance(value, int) and value > 0

    @classmethod
    def _validate_success(
        cls, metadata: dict[str, Any], *, expected_process_id: int, bounds: Mapping[str, int]
    ) -> None:
        if set(metadata) != cls._SUCCESS_FIELDS:
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper success metadata 字段不匹配",
                data={"reason": "invalid_helper_metadata"},
            )
        integer_fields = (
            "expected_pid",
            "target_pid",
            "target_window",
            "target_top_level_window",
            "root_window",
            "root_width",
            "root_height",
            "png_bytes",
        )
        if (
            metadata.get("ok") is not True
            or isinstance(metadata.get("schema_version"), bool)
            or metadata.get("schema_version") != 1
            or metadata.get("capture_method") != "x11_root_xgetimage"
            or metadata.get("format") != "png"
            or metadata.get("mime_type") != "image/png"
            or metadata.get("expected_pid") != expected_process_id
            or metadata.get("target_pid") != expected_process_id
            or metadata.get("cursor_included") is not False
            or metadata.get("occlusion_checked") is not True
            or metadata.get("same_euid_verified") is not True
            or metadata.get("scene_stable") is not True
            or any(not cls._positive_integer(metadata.get(name)) for name in integer_fields)
            or any(
                isinstance(metadata.get(name), bool)
                or not isinstance(metadata.get(name), int)
                for name in ("x", "y", "width", "height")
            )
            or any(metadata.get(name) != bounds[name] for name in ("x", "y", "width", "height"))
            or metadata["x"] < 0
            or metadata["y"] < 0
            or metadata["width"] <= 0
            or metadata["height"] <= 0
            or metadata["x"] + metadata["width"] > metadata["root_width"]
            or metadata["y"] + metadata["height"] > metadata["root_height"]
        ):
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper 未证明精确且未遮挡的目标截图",
                data={"reason": "capture_evidence_mismatch"},
            )

    @staticmethod
    def _validate_png(png: bytes, *, width: int, height: int) -> None:
        if not png or len(png) > CAPTURE_MAX_BYTES:
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper PNG 为空或超过限制",
                data={"reason": "invalid_png_size", "limit_bytes": CAPTURE_MAX_BYTES},
            )
        if len(png) < 24 or png[:8] != PNG_SIGNATURE or png[12:16] != b"IHDR":
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper 未返回 PNG",
                data={"reason": "invalid_png"},
            )
        actual_width, actual_height = struct.unpack(">II", png[16:24])
        if actual_width != width or actual_height != height:
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "PNG 尺寸与目标 bounds 不一致",
                data={
                    "reason": "png_dimensions_mismatch",
                    "expected": {"width": width, "height": height},
                    "actual": {"width": actual_width, "height": actual_height},
                },
            )

    def capture_target(
        self,
        *,
        expected_process_id: int,
        bounds: Mapping[str, int],
        deadline: float,
    ) -> CaptureResult:
        environment = self._qualified_environment()
        executable_fd = self._open_validated_executable()
        executable = f"/proc/self/fd/{executable_fd}"
        _check_deadline(deadline)
        command = [
            executable,
            "capture-target",
            "--expected-pid",
            str(expected_process_id),
            "--x",
            str(bounds["x"]),
            "--y",
            str(bounds["y"]),
            "--width",
            str(bounds["width"]),
            "--height",
            str(bounds["height"]),
            "--deadline-monotonic-ns",
            str(int(deadline * 1_000_000_000)),
        ]
        try:
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=str(self.path.parent),
                env=environment,
                close_fds=True,
                pass_fds=(executable_fd,),
            )
        except OSError as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "无法启动固定路径 X11 capture helper",
                data={
                    "reason": "capture_helper_start_failed",
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        finally:
            os.close(executable_fd)
        stdout, stderr = self._communicate_bounded(process, deadline)
        metadata = self._metadata(stderr)
        if process.returncode != 0:
            valid_failure = (
                process.returncode in {64, 69, 70, 73, 75, 76}
                and
                self._FAILURE_FIELDS.issubset(metadata)
                and set(metadata) <= self._FAILURE_FIELDS | self._FAILURE_CONTEXT_FIELDS
                and metadata.get("ok") is False
                and metadata.get("schema_version") == 1
                and isinstance(metadata.get("code"), str)
                and 0 < len(metadata.get("code")) <= MAX_FIELD_CHARS
                and isinstance(metadata.get("phase"), str)
                and 0 < len(metadata.get("phase")) <= MAX_FIELD_CHARS
                and (
                    "expected_pid" not in metadata
                    or metadata.get("expected_pid") == expected_process_id
                )
                and all(
                    name not in metadata
                    or (
                        not isinstance(metadata.get(name), bool)
                        and isinstance(metadata.get(name), int)
                        and (metadata.get(name) >= 0 if name in {"x", "y"} else metadata.get(name) > 0)
                        and metadata.get(name) == (
                            expected_process_id if name == "expected_pid" else bounds[name]
                        )
                    )
                    for name in self._FAILURE_CONTEXT_FIELDS
                )
                and not stdout
            )
            details = {
                "helper_exit_code": process.returncode,
                "helper_code": metadata.get("code") if valid_failure else None,
                "phase": metadata.get("phase") if valid_failure else "capture",
                "reason": "helper_failed" if valid_failure else "invalid_helper_failure",
                "effect": "not_applied",
            }
            if valid_failure and process.returncode == 69:
                raise DriverError("DRIVER.UNAVAILABLE", "X11 capture 不可用", data=details)
            if valid_failure and process.returncode == 75:
                raise DriverError(
                    "DRIVER.TIMEOUT", "X11 capture helper 超时", retryable=True, data=details
                )
            raise DriverError("DRIVER.CAPTURE_FAILED", "X11 capture helper 失败关闭", data=details)
        self._validate_success(
            metadata, expected_process_id=expected_process_id, bounds=bounds
        )
        self._validate_png(
            stdout, width=int(bounds["width"]), height=int(bounds["height"])
        )
        if metadata.get("png_bytes") != len(stdout):
            raise DriverError(
                "DRIVER.CAPTURE_FAILED",
                "X11 capture helper PNG byte count 不匹配",
                data={"reason": "png_size_mismatch"},
            )
        return CaptureResult(bytes(stdout), metadata)

    @staticmethod
    def _communicate_bounded(
        process: subprocess.Popen[bytes], deadline: float
    ) -> tuple[bytes, bytes]:
        assert process.stdout is not None
        assert process.stderr is not None
        stdout = bytearray()
        stderr = bytearray()
        streams = selectors.DefaultSelector()
        try:
            for stream, kind in ((process.stdout, "stdout"), (process.stderr, "stderr")):
                os.set_blocking(stream.fileno(), False)
                streams.register(stream, selectors.EVENT_READ, kind)
            while streams.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise DriverError(
                        "DRIVER.TIMEOUT", "X11 capture helper 超时", retryable=True,
                        data={"phase": "capture", "effect": "not_applied"},
                    )
                for key, _events in streams.select(min(remaining, 0.05)):
                    try:
                        chunk = os.read(key.fileobj.fileno(), 64 * 1024)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        streams.unregister(key.fileobj)
                        continue
                    target = stdout if key.data == "stdout" else stderr
                    target.extend(chunk)
                    limit = (
                        CAPTURE_MAX_BYTES
                        if key.data == "stdout"
                        else CAPTURE_HELPER_MAX_METADATA_BYTES
                    )
                    if len(target) > limit:
                        raise DriverError(
                            "DRIVER.CAPTURE_FAILED",
                            "X11 capture helper 输出超过限制",
                            data={"stream": key.data, "limit_bytes": limit},
                        )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise DriverError(
                    "DRIVER.TIMEOUT", "X11 capture helper 超时", retryable=True,
                    data={"phase": "capture", "effect": "not_applied"},
                )
            process.wait(timeout=max(0.001, remaining))
            return bytes(stdout), bytes(stderr)
        except (DriverError, subprocess.TimeoutExpired) as exc:
            process.kill()
            process.wait(timeout=1)
            if isinstance(exc, DriverError):
                raise
            raise DriverError(
                "DRIVER.TIMEOUT", "X11 capture helper 超时", retryable=True,
                data={"phase": "capture", "effect": "not_applied"},
            ) from exc
        finally:
            streams.close()
            process.stdout.close()
            process.stderr.close()


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


def _text(value: Any, name: str, *, nullable: bool = False) -> str | None:
    if value is None and nullable:
        return None
    if not isinstance(value, str):
        _fail("DRIVER.INVALID_REQUEST", f"{name} 必须是字符串")
    if len(value) > MAX_FIELD_CHARS:
        _fail("DRIVER.INVALID_REQUEST", f"{name} 超过 {MAX_FIELD_CHARS} 字符")
    return value


def _safe_text(value: Any) -> str | None:
    if value is None:
        return None
    try:
        text = str(value)
    except Exception:
        return None
    return text[:MAX_FIELD_CHARS]


def _ordinary_type_text(value: Any) -> str:
    """Validate the complete payload before any focus or XTest dispatch."""

    if not isinstance(value, str):
        _fail("DRIVER.INVALID_REQUEST", "text 必须是字符串")
    if not value or len(value) > MAX_TYPE_TEXT_CHARS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"type_text 文本必须包含 1 到 {MAX_TYPE_TEXT_CHARS} 个字符",
        )
    try:
        encoded = value.encode("utf-8", errors="strict")
    except UnicodeEncodeError as exc:
        raise DriverError(
            "DRIVER.INVALID_REQUEST",
            "type_text 文本必须是有效 UTF-8",
            data={"reason": "invalid_unicode"},
        ) from exc
    if len(encoded) > MAX_TYPE_TEXT_BYTES:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"type_text UTF-8 载荷超过 {MAX_TYPE_TEXT_BYTES} 字节",
        )
    if any(
        (ord(character) < 0x20 and character != "\n")
        or 0x7F <= ord(character) <= 0x9F
        or 0xD800 <= ord(character) <= 0xDFFF
        or 0xFDD0 <= ord(character) <= 0xFDEF
        or (ord(character) & 0xFFFF) in {0xFFFE, 0xFFFF}
        for character in value
    ):
        _fail(
            "DRIVER.INVALID_REQUEST",
            "type_text 只接受普通文本与换行，不接受控制字符",
        )
    return value


def _pointer_button(value: Any) -> str:
    if value is None:
        return "left"
    if not isinstance(value, str) or value not in POINTER_BUTTONS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"pointer_click.button 仅支持 {sorted(POINTER_BUTTONS)!r}",
        )
    return value


def _pointer_position(value: Any) -> str:
    if value is None:
        return "center"
    if not isinstance(value, str) or value not in POINTER_POSITIONS:
        _fail(
            "DRIVER.INVALID_REQUEST",
            f"pointer_click.position 仅支持 {sorted(POINTER_POSITIONS)!r}",
        )
    return value


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
            "pointer_click 目标缺少有效 bounds",
        ) from exc
    if width <= 0 or height <= 0:
        raise DriverError(
            "DRIVER.ACTION_UNSUPPORTED",
            "pointer_click 只支持正面积目标",
            data={"bounds": _json_safe(dict(bounds))},
        )
    return (x + width // 2, y + height // 2)


def _normalize_role(value: Any) -> str:
    """把 AT-SPI role 名称规范为稳定的蛇形标识。"""

    text = (_safe_text(value) or "unknown").strip().lower()
    return text.replace(" ", "_").replace("-", "_") or "unknown"


def _is_protected_role(role: Any) -> bool:
    return _normalize_role(role) in {"password_text", "password"}


def _backend_name(backend: Any) -> str:
    return _safe_text(getattr(backend, "name", None)) or "unknown"


def _session_info(backend: Any) -> dict[str, Any]:
    try:
        raw = backend.session_info()
    except DriverError:
        raise
    except Exception as exc:
        raise DriverError(
            "DRIVER.ACTION_FAILED",
            "后端无法报告会话信息",
            data={"exception_type": type(exc).__name__},
        ) from exc
    if not isinstance(raw, Mapping):
        raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效的会话信息")
    normalized = _json_safe(raw)
    if not isinstance(normalized, dict):
        raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效的会话信息")
    return normalized


class LinuxAtspiDriver:
    """在可注入原生后端上实现快照限定的 AT-SPI 语义。"""

    def __init__(
        self,
        backend: AtspiBackend | None = None,
        *,
        xtest_helper: XTestHelper | None = None,
        capture_helper: X11CaptureHelper | None = None,
    ) -> None:
        self.backend: AtspiBackend = backend if backend is not None else create_default_backend()
        self.xtest_helper = xtest_helper if xtest_helper is not None else XTestHelper()
        self.capture_helper = (
            capture_helper if capture_helper is not None else X11CaptureHelper()
        )
        self.generation = uuid.uuid4().hex
        self._revision = 0
        self._current: _SnapshotRecord | None = None

    def execute(self, action: str, args: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        values = {} if args is None else _object(args, "args")
        if action == "inspect_session":
            return self._inspect_session(values)
        if action == "list_applications":
            return self._list_applications(values, deadline)
        if action == "snapshot":
            return self._snapshot(values, deadline)
        if action == "find":
            return self._find(values, deadline)
        if action == "capture_target":
            _fail(
                "DRIVER.INVALID_REQUEST",
                "capture_target 必须通过完整 Host artifact invocation 调用",
            )
        if action in WRITE_ACTIONS:
            return self._write(action, values, deadline)
        _fail("DRIVER.INVALID_REQUEST", f"未知动作：{action}", action=action)

    def _inspect_session(self, args: dict[str, Any]) -> dict[str, Any]:
        _only_keys(args, set(), "args")
        session = _session_info(self.backend)
        return {
            "backend": _backend_name(self.backend),
            "session_type": _safe_text(session.get("session_type")),
            "desktop": _safe_text(session.get("desktop")),
        }

    def _list_applications(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, set(), "args")
        session = _session_info(self.backend)
        applications: list[Any] = []
        for item in self.backend.list_applications(deadline=deadline):
            _check_deadline(deadline)
            if not isinstance(item, Mapping):
                raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效的应用")
            applications.append(_json_safe(item))
        return {
            "session": session,
            "backend": _backend_name(self.backend),
            "applications": applications,
        }

    def _application_selector(self, raw: Any) -> dict[str, Any]:
        selector = _object(raw, "application")
        _only_keys(selector, set(APPLICATION_SELECTOR_FIELDS), "application")
        if not selector:
            _fail("DRIVER.INVALID_REQUEST", "application 必须包含精确选择器")
        normalized: dict[str, Any] = {}
        for name in ("bus_name", "name", "toolkit_name"):
            if name in selector:
                normalized[name] = _text(selector[name], f"application.{name}")
        if "process_id" in selector:
            process_id = selector["process_id"]
            if isinstance(process_id, bool) or not isinstance(process_id, int) or process_id < 0:
                _fail("DRIVER.INVALID_REQUEST", "application.process_id 必须是非负整数")
            normalized["process_id"] = process_id
        return normalized

    def _snapshot(self, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        _only_keys(args, {"application", "max_depth", "max_nodes"}, "args")
        application = self._application_selector(args.get("application"))
        max_depth = _bounded_integer(
            args.get("max_depth", DEFAULT_MAX_DEPTH), "max_depth", 0, MAX_DEPTH
        )
        max_nodes = _bounded_integer(
            args.get("max_nodes", DEFAULT_MAX_NODES), "max_nodes", 1, MAX_NODES
        )
        record = self._capture(
            application, max_depth=max_depth, max_nodes=max_nodes, deadline=deadline
        )
        return copy.deepcopy(record.public)

    def _capture(
        self,
        application: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> _SnapshotRecord:
        _check_deadline(deadline)
        session = _session_info(self.backend)
        raw = self.backend.capture(
            application, max_depth=max_depth, max_nodes=max_nodes, deadline=deadline
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
                parent_index = backend_node.parent_index
                if (
                    isinstance(parent_index, bool)
                    or not isinstance(parent_index, int)
                    or parent_index < 0
                    or parent_index >= index
                ):
                    raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效父子关系")
                parent_id = f"n{parent_index}"
            node_id = f"n{index}"
            states = {name: None for name in STATE_NAMES}
            if backend_node.states is not None:
                for name in STATE_NAMES:
                    value = backend_node.states.get(name)
                    states[name] = value if isinstance(value, bool) or value is None else None
            attributes: dict[str, str] = {}
            if isinstance(backend_node.attributes, Mapping):
                for key, value in list(backend_node.attributes.items())[:256]:
                    safe_key = _safe_text(key)
                    safe_value = _safe_text(value)
                    if safe_key is not None and safe_value is not None:
                        attributes[safe_key] = safe_value
            actions = sorted(
                {str(item) for item in backend_node.actions if str(item) in NODE_ACTIONS}
            )
            provenance = _json_safe(dict(backend_node.provenance or {}))
            if not isinstance(provenance, dict):
                provenance = {}
            provenance["backend"] = _backend_name(self.backend)
            role = _normalize_role(backend_node.role)
            protected = (
                states["protected"] is True
                or provenance.get("value_redacted") is True
                or _is_protected_role(role)
            )
            if protected:
                states["protected"] = True
                provenance["value_redacted"] = True
            node = {
                "node_id": node_id,
                "parent_id": parent_id,
                "role": role,
                "name": _safe_text(backend_node.name),
                "description": _safe_text(backend_node.description),
                "value": None if protected else _safe_text(backend_node.value),
                "attributes": attributes,
                "states": states,
                "bounds": _normalize_bounds(backend_node.bounds),
                "actions": [
                    item
                    for item in actions
                    if not (
                        protected
                        and item in {"pointer_click", "set_text", "type_text"}
                    )
                ],
                "provenance": provenance,
            }
            nodes.append(node)
            handles[node_id] = backend_node.native
            fingerprints[node_id] = self._fingerprint(node)
        application_info = _json_safe(raw.application)
        if not isinstance(application_info, dict):
            raise DriverError("DRIVER.ACTION_FAILED", "后端返回了无效应用身份")
        public = {
            "snapshot_id": snapshot_id,
            "revision": revision,
            "session": session,
            "backend": _backend_name(self.backend),
            "application": application_info,
            "nodes": nodes,
            "truncated": bool(raw.truncated),
        }
        record = _SnapshotRecord(
            public=public,
            handles=handles,
            fingerprints=fingerprints,
            application_selector=copy.deepcopy(dict(application)),
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
            "bus_name": provenance.get("bus_name"),
            "object_path": provenance.get("object_path"),
            "accessible_id": provenance.get("accessible_id"),
            "application_name": provenance.get("application_name"),
            "toolkit_name": provenance.get("toolkit_name"),
            "process_id": provenance.get("process_id"),
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
                "截断快照无法证明定位器唯一匹配",
            )
        locator = self._locator(args.get("locator"))
        node = self._resolve(record, locator, deadline)
        return {"target": self._target(record, node["node_id"]), "node": copy.deepcopy(node)}

    def _locator(self, raw: Any) -> dict[str, Any]:
        locator = _object(raw, "locator")
        allowed = {
            "role",
            "name",
            "description",
            "value",
            "bus_name",
            "object_path",
            "toolkit_name",
            "attributes",
            "states",
            "actions",
            "match",
        }
        _only_keys(locator, allowed, "locator")
        if not (set(locator) - {"match"}):
            _fail("DRIVER.INVALID_REQUEST", "locator 至少需要一个选择条件")
        normalized: dict[str, Any] = {"match": locator.get("match", "exact")}
        if normalized["match"] != "exact":
            _fail("DRIVER.INVALID_REQUEST", "当前版本只支持 exact 定位")
        for name in ("role", "name", "bus_name", "object_path", "toolkit_name"):
            if name in locator:
                normalized[name] = _text(locator[name], f"locator.{name}")
        for name in ("description", "value"):
            if name in locator:
                normalized[name] = _text(locator[name], f"locator.{name}", nullable=True)
        if "attributes" in locator:
            attributes = _object(locator["attributes"], "locator.attributes")
            if not attributes:
                _fail("DRIVER.INVALID_REQUEST", "locator.attributes 不能为空")
            normalized_attributes: dict[str, str] = {}
            for key, value in attributes.items():
                if not isinstance(key, str) or not key or len(key) > MAX_FIELD_CHARS:
                    _fail("DRIVER.INVALID_REQUEST", "locator.attributes 的键必须是有界非空字符串")
                normalized_attributes[key] = _text(value, f"locator.attributes.{key}") or ""
            normalized["attributes"] = normalized_attributes
        if "states" in locator:
            states = _object(locator["states"], "locator.states")
            if not states:
                _fail("DRIVER.INVALID_REQUEST", "locator.states 不能为空")
            _only_keys(states, set(STATE_NAMES), "locator.states")
            normalized_states: dict[str, bool | None] = {}
            for name, value in states.items():
                if not isinstance(value, bool) and value is not None:
                    _fail("DRIVER.INVALID_REQUEST", f"locator.states.{name} 必须是布尔值或 null")
                normalized_states[name] = value
            normalized["states"] = normalized_states
        if "actions" in locator:
            actions = locator["actions"]
            if isinstance(actions, (str, bytes)) or not isinstance(actions, list):
                _fail("DRIVER.INVALID_REQUEST", "locator.actions 必须是数组")
            if not actions:
                _fail("DRIVER.INVALID_REQUEST", "locator.actions 不能为空")
            normalized_actions: list[str] = []
            for action in actions:
                if not isinstance(action, str) or action not in NODE_ACTIONS:
                    _fail("DRIVER.INVALID_REQUEST", "locator.actions 包含不支持的动作")
                if action in normalized_actions:
                    _fail("DRIVER.INVALID_REQUEST", "locator.actions 不能重复")
                normalized_actions.append(action)
            normalized["actions"] = normalized_actions
        return normalized

    @staticmethod
    def _matches(node: Mapping[str, Any], locator: Mapping[str, Any]) -> bool:
        provenance = node.get("provenance")
        if not isinstance(provenance, Mapping):
            provenance = {}
        for name in ("role", "name", "description", "value"):
            if name in locator and node.get(name) != locator[name]:
                return False
        for name in ("bus_name", "object_path", "toolkit_name"):
            if name in locator and provenance.get(name) != locator[name]:
                return False
        node_attributes = node.get("attributes")
        if not isinstance(node_attributes, Mapping):
            node_attributes = {}
        if any(node_attributes.get(key) != value for key, value in locator.get("attributes", {}).items()):
            return False
        node_states = node.get("states")
        if not isinstance(node_states, Mapping):
            node_states = {}
        if any(node_states.get(name) is not expected for name, expected in locator.get("states", {}).items()):
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
            summaries = [
                {
                    "node_id": node["node_id"],
                    "role": node["role"],
                    "name": node["name"],
                    "object_path": node.get("provenance", {}).get("object_path"),
                    "actions": node["actions"],
                }
                for node in candidates[:MAX_CANDIDATE_SUMMARIES]
            ]
            raise DriverError(
                "DRIVER.AMBIGUOUS",
                "定位器匹配多个节点",
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

    def _fresh_target(
        self, args: Mapping[str, Any], deadline: float, *, purpose: str
    ) -> _FreshTarget:
        if "target" not in args or "locator" not in args:
            _fail("DRIVER.INVALID_REQUEST", "target 和 locator 是必填字段")
        target = _object(args["target"], "target")
        _only_keys(target, {"snapshot_id", "revision", "node_id"}, "target")
        node_id = target.get("node_id")
        if not isinstance(node_id, str) or not node_id:
            _fail("DRIVER.INVALID_REQUEST", "target.node_id 必须是非空字符串")
        record = self._record(target.get("snapshot_id"), target.get("revision"))
        if record.public.get("truncated"):
            raise DriverError(
                "DRIVER.SNAPSHOT_TRUNCATED", f"截断快照不能用于 {purpose}"
            )
        locator = self._locator(args["locator"])
        expected = self._resolve(record, locator, deadline)
        if expected["node_id"] != node_id or node_id not in record.handles:
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "target 与快照中的定位结果不一致",
                data={"node_id": node_id, "resolved_node_id": expected["node_id"]},
            )
        expected_fingerprint = record.fingerprints[node_id]
        _check_deadline(deadline)
        fresh = self._capture(
            record.application_selector,
            max_depth=record.max_depth,
            max_nodes=record.max_nodes,
            deadline=deadline,
        )
        if fresh.public.get("truncated"):
            raise DriverError(
                "DRIVER.SNAPSHOT_TRUNCATED",
                f"{purpose} 前重新抓取的快照已截断",
            )
        try:
            resolved = self._resolve(fresh, locator, deadline)
        except DriverError as exc:
            if exc.code in {"DRIVER.NOT_FOUND", "DRIVER.AMBIGUOUS"}:
                details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                raise DriverError(
                    "DRIVER.STALE_SNAPSHOT",
                    "定位器已无法解析为原来的唯一目标",
                    data={"reason": exc.code, **details},
                ) from exc
            raise
        fresh_node_id = resolved["node_id"]
        try:
            same = self.backend.same_element(
                record.handles[node_id], fresh.handles[fresh_node_id], deadline=deadline
            )
        except DriverError as exc:
            if exc.code == "DRIVER.TIMEOUT":
                raise
            details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "无法验证原生目标身份",
                data={"reason": exc.code, **details},
            ) from exc
        except Exception as exc:
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "无法验证原生目标身份",
                data={"exception_type": type(exc).__name__},
            ) from exc
        if not same:
            raise DriverError("DRIVER.STALE_SNAPSHOT", "定位器解析到了不同的原生目标")
        if fresh.fingerprints[fresh_node_id] != expected_fingerprint:
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "定位器解析到了不同的语义目标",
                data={
                    "previous_snapshot_id": record.public["snapshot_id"],
                    "current_snapshot_id": fresh.public["snapshot_id"],
                },
            )
        return _FreshTarget(record, fresh, node_id, fresh_node_id, resolved)

    @staticmethod
    def _untrusted_capture_subtree(
        record: _SnapshotRecord, node_id: str
    ) -> dict[str, Any] | None:
        queue = deque([node_id])
        while queue:
            current = queue.popleft()
            node = record.public["nodes"][int(current[1:])]
            states = node.get("states")
            provenance = node.get("provenance")
            if (
                not isinstance(states, Mapping)
                or states.get("protected") is not False
                or not isinstance(provenance, Mapping)
                or provenance.get("value_redacted") is not False
                or _is_protected_role(node.get("role"))
            ):
                return node
            queue.extend(
                candidate["node_id"]
                for candidate in record.public["nodes"]
                if candidate.get("parent_id") == current
            )
        return None

    @staticmethod
    def _rectangles_overlap(
        left: Mapping[str, int], right: Mapping[str, int]
    ) -> bool:
        return (
            left["x"] < right["x"] + right["width"]
            and right["x"] < left["x"] + left["width"]
            and left["y"] < right["y"] + right["height"]
            and right["y"] < left["y"] + left["height"]
        )

    def _validate_accessible_capture_surface(
        self, target: _FreshTarget, bounds: Mapping[str, int], deadline: float
    ) -> None:
        nodes = target.fresh.public["nodes"]
        descendants: set[str] = {target.node_id}
        queue = deque([target.node_id])
        while queue:
            parent_id = queue.popleft()
            for candidate in nodes:
                if candidate.get("parent_id") != parent_id:
                    continue
                child_id = candidate["node_id"]
                if child_id not in descendants:
                    descendants.add(child_id)
                    queue.append(child_id)
        parents = {candidate["node_id"]: candidate.get("parent_id") for candidate in nodes}
        ancestors: list[str] = []
        current = target.node_id
        while current is not None:
            ancestors.append(current)
            current = parents.get(current)
        center = _center_point(bounds)
        hit = None
        for node_id in reversed(ancestors):
            hit = self.backend.accessible_at_point(
                target.fresh.handles[node_id], center[0], center[1], deadline=deadline
            )
            if hit is not None:
                break
        if hit is None or not any(
            self.backend.same_element(
                hit, target.fresh.handles[node_id], deadline=deadline
            )
            for node_id in descendants
        ):
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                "capture_target 中心点未命中 fresh 目标子树",
            )
        safe_ids = descendants | set(ancestors)
        for candidate in nodes:
            if candidate["node_id"] in safe_ids:
                continue
            candidate_bounds = candidate.get("bounds")
            if not isinstance(candidate_bounds, Mapping):
                continue
            try:
                normalized = {
                    name: int(candidate_bounds[name])
                    for name in ("x", "y", "width", "height")
                }
            except (KeyError, TypeError, ValueError, OverflowError):
                continue
            if normalized["width"] <= 0 or normalized["height"] <= 0:
                continue
            states = candidate.get("states")
            if (
                self._rectangles_overlap(bounds, normalized)
                and (not isinstance(states, Mapping)
                     or states.get("visible") is not False)
                and (not isinstance(states, Mapping)
                     or states.get("showing") is not False)
            ):
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "capture_target 区域与目标子树之外的可见节点相交",
                    data={"overlapping_node_id": candidate["node_id"]},
                )

    def _capture_target(
        self,
        args: dict[str, Any],
        invocation: WorkerArtifactInvocation,
        deadline: float,
    ) -> dict[str, Any]:
        _only_keys(args, {"target", "locator", "format"}, "args")
        if args.get("format") != "png":
            _fail("DRIVER.INVALID_REQUEST", "capture_target.format 必须是 png")
        target = self._fresh_target(args, deadline, purpose="capture_target")
        resolved = target.node
        states = resolved.get("states")
        if not isinstance(states, Mapping) or not all(
            states.get(name) is True for name in ("visible", "showing")
        ):
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                "capture_target 只支持 fresh snapshot 中可见且正在显示的目标",
                data={"states": _json_safe(dict(states or {}))},
            )
        protected = self._untrusted_capture_subtree(target.fresh, target.node_id)
        if protected is not None:
            raise DriverError(
                "DRIVER.PROTECTED_ELEMENT",
                "目标或其后代无法明确证明为非受保护内容，禁止截图",
                data={
                    "target_node_id": target.node_id,
                    "protected_node_id": protected.get("node_id"),
                },
            )
        bounds = resolved.get("bounds")
        if not isinstance(bounds, Mapping):
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED", "capture_target 目标缺少 screen bounds"
            )
        try:
            normalized_bounds = {
                name: int(bounds[name]) for name in ("x", "y", "width", "height")
            }
        except (KeyError, TypeError, ValueError, OverflowError) as exc:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED", "capture_target 目标 bounds 无效"
            ) from exc
        if normalized_bounds["width"] <= 0 or normalized_bounds["height"] <= 0:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                "capture_target 只支持正面积目标",
                data={"bounds": normalized_bounds},
            )
        provenance = resolved.get("provenance")
        if not isinstance(provenance, Mapping):
            provenance = {}
        process_id = provenance.get("process_id")
        application_process_id = target.fresh.public.get("application", {}).get(
            "process_id"
        )
        if (
            isinstance(process_id, bool)
            or not isinstance(process_id, int)
            or process_id <= 0
            or application_process_id != process_id
        ):
            raise DriverError(
                "DRIVER.STALE_SNAPSHOT",
                "无法证明 capture_target 目标的应用进程归属",
            )
        if provenance.get("coordinate_space") != "screen":
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                "capture_target 目标 bounds 不是 screen coordinate space",
            )
        self._validate_accessible_capture_surface(
            target, normalized_bounds, deadline
        )
        _check_deadline(deadline)
        captured = self.capture_helper.capture_target(
            expected_process_id=process_id,
            bounds=normalized_bounds,
            deadline=deadline,
        )
        metadata = captured.metadata
        output_provenance = {
            "capture_method": metadata["capture_method"],
            "snapshot_id": target.fresh.public["snapshot_id"],
            "revision": target.fresh.public["revision"],
            "node_id": target.node_id,
            "application_process_id": process_id,
            "format": metadata["format"],
            "mime_type": metadata["mime_type"],
            "target_process_id": metadata["target_pid"],
            "target_window": metadata["target_window"],
            "target_top_level_window": metadata["target_top_level_window"],
            "root_window": metadata["root_window"],
            "bounds": {
                name: metadata[name] for name in ("x", "y", "width", "height")
            },
            "root_size": {
                "width": metadata["root_width"],
                "height": metadata["root_height"],
            },
            "cursor_included": metadata["cursor_included"],
            "occlusion_checked": metadata["occlusion_checked"],
            "same_euid_verified": metadata["same_euid_verified"],
            "scene_stable": metadata["scene_stable"],
        }
        frame = invocation.write_output(
            "frame", captured.png, media_type="image/png"
        )
        invocation.complete_ok()
        return {"frame": frame, "provenance": output_provenance}

    def _write(self, action: str, args: dict[str, Any], deadline: float) -> dict[str, Any]:
        text_actions = {"set_text", "type_text"}
        pointer_action = action == "pointer_click"
        allowed = {"target", "locator"} | ({"text"} if action in text_actions else set())
        pointer_button: str | None = None
        pointer_position: str | None = None
        if pointer_action:
            allowed |= {"button", "position"}
        _only_keys(args, allowed, "args")
        if pointer_action:
            pointer_button = _pointer_button(args.get("button"))
            pointer_position = _pointer_position(args.get("position"))
        fresh_target = self._fresh_target(args, deadline, purpose="写动作")
        record = fresh_target.previous
        fresh = fresh_target.fresh
        node_id = fresh_target.previous_node_id
        fresh_node_id = fresh_target.node_id
        resolved = fresh_target.node
        target_subtree_ids: set[str] = {fresh_node_id}
        target_ancestor_ids: list[str] = [fresh_node_id]
        if pointer_action:
            queue = deque([fresh_node_id])
            while queue:
                parent_id = queue.popleft()
                for candidate in fresh.public["nodes"]:
                    if candidate.get("parent_id") != parent_id:
                        continue
                    child_id = candidate["node_id"]
                    if child_id in target_subtree_ids:
                        continue
                    target_subtree_ids.add(child_id)
                    queue.append(child_id)
            parent_lookup = {
                candidate["node_id"]: candidate.get("parent_id")
                for candidate in fresh.public["nodes"]
            }
            current_ancestor = fresh_node_id
            while parent_lookup.get(current_ancestor) is not None:
                current_ancestor = parent_lookup[current_ancestor]
                target_ancestor_ids.append(current_ancestor)
            target_ancestor_ids.reverse()
        text: str | None = None
        expected_process_id: int | None = None
        click_point: tuple[int, int] | None = None
        if action in text_actions:
            if "text" not in args:
                _fail("DRIVER.INVALID_REQUEST", f"{action} 必须提供 text")
            text = (
                _ordinary_type_text(args["text"])
                if action == "type_text"
                else _text(args["text"], "text")
            )
            provenance = resolved.get("provenance", {})
            states = resolved.get("states", {})
            if (
                isinstance(provenance, Mapping) and provenance.get("value_redacted") is True
            ) or (isinstance(states, Mapping) and states.get("protected") is True):
                raise DriverError(
                    "DRIVER.PROTECTED_ELEMENT", f"受保护文本元素禁止 {action}"
                )
            if action == "type_text":
                if resolved.get("role") not in TYPE_TEXT_ROLES or not all(
                    isinstance(states, Mapping) and states.get(name) is True
                    for name in (
                        "enabled",
                        "visible",
                        "showing",
                        "focusable",
                        "editable",
                        "sensitive",
                    )
                ):
                    raise DriverError(
                        "DRIVER.ACTION_UNSUPPORTED",
                        "type_text 只支持可见、可聚焦且可编辑的普通文本目标",
                        data={"action": action, "role": resolved.get("role")},
                    )
                raw_process_id = (
                    provenance.get("process_id")
                    if isinstance(provenance, Mapping)
                    else None
                )
                if (
                    isinstance(raw_process_id, bool)
                    or not isinstance(raw_process_id, int)
                    or raw_process_id <= 0
                    or fresh.public.get("application", {}).get("process_id")
                    != raw_process_id
                ):
                    raise DriverError(
                        "DRIVER.STALE_SNAPSHOT",
                        "无法证明 type_text 目标的应用进程归属",
                    )
                expected_process_id = raw_process_id
        elif pointer_action:
            provenance = resolved.get("provenance", {})
            if (
                isinstance(provenance, Mapping)
                and provenance.get("value_redacted") is True
            ) or (
                isinstance(resolved.get("states"), Mapping)
                and resolved["states"].get("protected") is True
            ):
                raise DriverError(
                    "DRIVER.PROTECTED_ELEMENT",
                    "受保护元素禁止 pointer_click",
                )
            raw_process_id = (
                provenance.get("process_id")
                if isinstance(provenance, Mapping)
                else None
            )
            if (
                isinstance(raw_process_id, bool)
                or not isinstance(raw_process_id, int)
                or raw_process_id <= 0
                or fresh.public.get("application", {}).get("process_id")
                != raw_process_id
            ):
                raise DriverError(
                    "DRIVER.STALE_SNAPSHOT",
                    "无法证明 pointer_click 目标的应用进程归属",
                )
            states = resolved.get("states")
            if not isinstance(states, Mapping):
                states = {}
            if not all(
                states.get(name) is True
                for name in ("enabled", "visible", "showing", "sensitive")
            ):
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "pointer_click 只支持可见且可交互的目标",
                    data={"action": action, "states": _json_safe(dict(states))},
                )
            bounds = resolved.get("bounds")
            if not isinstance(bounds, Mapping):
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "pointer_click 目标缺少可点击 bounds",
                )
            click_point = _center_point(bounds)
            point_accessible = None
            for candidate_node_id in target_ancestor_ids:
                point_accessible = self.backend.accessible_at_point(
                    fresh.handles[candidate_node_id],
                    click_point[0],
                    click_point[1],
                    deadline=deadline,
                )
                if point_accessible is not None:
                    break
            if point_accessible is None:
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "pointer_click 无法通过 AT-SPI 证明点击点命中目标子树",
                )
            point_hits_target = False
            for candidate_node_id in target_subtree_ids:
                try:
                    if self.backend.same_element(
                        point_accessible,
                        fresh.handles[candidate_node_id],
                        deadline=deadline,
                    ):
                        point_hits_target = True
                        break
                except DriverError as exc:
                    if exc.code == "DRIVER.TIMEOUT":
                        raise
                except Exception:
                    continue
            if not point_hits_target:
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "pointer_click 点击点未命中 fresh target 或其后代",
                    data={
                        "effect": "not_applied",
                        "click_point": {"x": click_point[0], "y": click_point[1]},
                    },
                )
            expected_process_id = raw_process_id
        if isinstance(self.backend, GioAtspiBackend):
            # Gio fallback intentionally has no write surface, including
            # state-preserving expand/collapse calls that would otherwise no-op.
            self.backend._unsupported(action)
        if action not in resolved["actions"]:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                f"目标不支持原生 {action}",
                data={"action": action, "available_actions": resolved["actions"]},
            )
        if action == "type_text":
            # Validate the helper path and desktop profile before changing focus.
            self.xtest_helper.preflight()
        elif pointer_action:
            self.xtest_helper.preflight()
        states = resolved.get("states")
        if not isinstance(states, Mapping):
            states = {}
        if action == "toggle" and not isinstance(states.get("checked"), bool):
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                "目标没有可观察的 checked 状态，不能执行 toggle",
                data={"action": action, "checked": states.get("checked")},
            )
        if action in {"expand", "collapse"}:
            expandable = states.get("expandable")
            expanded = states.get("expanded")
            if expandable is not True or not isinstance(expanded, bool):
                raise DriverError(
                    "DRIVER.ACTION_UNSUPPORTED",
                    f"目标没有可验证的展开状态，不能执行 {action}",
                    data={
                        "action": action,
                        "expandable": expandable,
                        "expanded": expanded,
                    },
                )
            desired = action == "expand"
            if expanded is desired:
                _check_deadline(deadline)
                return {
                    "ok": True,
                    "action": action,
                    "resolved": self._target(fresh, fresh_node_id),
                    "backend_result": {
                        "native_interface": "Action.do_action",
                        "native_action_name": GTK3_NATIVE_ACTION_NAMES[action],
                        "dispatched": False,
                        "no_op": True,
                        "observed_state": {"expanded": expanded},
                    },
                }
        _check_deadline(deadline)
        native = fresh.handles[fresh_node_id]
        dispatched = False
        try:
            if action == "focus":
                dispatched = True
                backend_result = self.backend.focus(native, deadline=deadline)
            elif action == "invoke":
                dispatched = True
                backend_result = self.backend.invoke(native, deadline=deadline)
            elif action == "set_text":
                assert text is not None
                dispatched = True
                backend_result = self.backend.set_text(native, text, deadline=deadline)
            elif action == "type_text":
                assert text is not None and expected_process_id is not None
                dispatched = True
                focus_result = self.backend.focus(native, deadline=deadline)
                # Component.grab_focus is a synchronous D-Bus call, but GTK/Qt
                # commit the toolkit-local widget focus on their event loop.  The
                # X input focus may already belong to the process during that
                # short transition, so give the owned application one bounded
                # iteration before the helper verifies the X focus owner.
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    _check_deadline(deadline, post_dispatch=True)
                time.sleep(min(0.05, remaining))
                _check_deadline(deadline, post_dispatch=True)
                try:
                    input_result = self.xtest_helper.type_text(
                        text,
                        expected_process_id=expected_process_id,
                        deadline=deadline,
                    )
                except DriverError as exc:
                    if exc.code == "DRIVER.UNKNOWN_EFFECT":
                        raise
                    details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                    raise DriverError(
                        "DRIVER.UNKNOWN_EFFECT",
                        "目标聚焦后未能确定 XTest 输入效果",
                        retryable=False,
                        data={
                            "action": action,
                            "reason": exc.code,
                            "phase": details.get("phase", "after_focus"),
                            "dispatch_started": details.get("dispatch_started", False),
                            "effect": "unknown",
                        },
                    ) from exc
                backend_result = {
                    "native_interface": "Component.grab_focus -> XTEST",
                    "focus": _json_safe(focus_result),
                    "input": _json_safe(input_result),
                    "synthetic_input": True,
                }
            elif action == "pointer_click":
                assert expected_process_id is not None and click_point is not None
                focus_changed = False
                focus_result = self.backend.focus(native, deadline=deadline)
                focus_changed = True
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    _check_deadline(deadline, post_dispatch=True)
                time.sleep(min(0.05, remaining))
                _check_deadline(deadline, post_dispatch=True)
                try:
                    input_result = self.xtest_helper.pointer_click(
                        expected_process_id=expected_process_id,
                        x=click_point[0],
                        y=click_point[1],
                        deadline=deadline,
                    )
                except DriverError as exc:
                    if exc.code == "DRIVER.UNKNOWN_EFFECT":
                        raise
                    details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
                    if details.get("dispatch_started") is True:
                        raise DriverError(
                            "DRIVER.UNKNOWN_EFFECT",
                            "目标聚焦后未能确定 XTest pointer_click 效果",
                            retryable=False,
                            data={
                                "action": action,
                                "reason": exc.code,
                                "phase": details.get("phase", "pointer_dispatch"),
                                "dispatch_started": True,
                                "focus_changed": focus_changed,
                                "effect": "unknown",
                            },
                        ) from exc
                    raise DriverError(
                        exc.code,
                        "pointer_click 在点击派发前失败；目标焦点可能已改变",
                        retryable=False,
                        data={
                            "action": action,
                            "reason": exc.code,
                            "phase": details.get("phase", "before_pointer_dispatch"),
                            "dispatch_started": False,
                            "focus_changed": focus_changed,
                            "effect": "contextual" if focus_changed else "not_applied",
                        },
                    ) from exc
                dispatched = True
                backend_result = {
                    "native_interface": "Component.grab_focus -> XTEST",
                    "focus": _json_safe(focus_result),
                    "input": _json_safe(input_result),
                    "synthetic_input": True,
                    "button": pointer_button,
                    "position": pointer_position,
                    "click_point": {"x": click_point[0], "y": click_point[1]},
                    "preflight_evidence": {
                        "fresh_target_resolved": True,
                        "native_identity_matched": True,
                        "semantic_fingerprint_matched": True,
                        "positive_area_bounds": True,
                        "center_derived_from_bounds": True,
                        "atspi_hit_within_target_subtree": True,
                        "target_process_id": expected_process_id,
                        "x11_focus_owner_matched": True,
                        "x11_point_window_process_matched": True,
                    },
                }
            elif action == "toggle":
                backend_result = self.backend.toggle(native, deadline=deadline)
            elif action == "expand":
                backend_result = self.backend.expand(native, deadline=deadline)
            elif action == "collapse":
                backend_result = self.backend.collapse(native, deadline=deadline)
            else:  # WRITE_ACTIONS 与派发分支必须同步；意外分歧时失败关闭。
                raise DriverError(
                    "DRIVER.INVALID_REQUEST", f"不支持的写动作：{action}"
                )
            _check_deadline(deadline, post_dispatch=True)
        except DriverError as exc:
            details = dict(exc.data) if isinstance(exc.data, Mapping) else {}
            if exc.code == "DRIVER.TIMEOUT" and details.get("phase") == "before_dispatch":
                dispatched = False
                raise
            if exc.code in {
                "DRIVER.ACTION_UNSUPPORTED",
                "DRIVER.PROTECTED_ELEMENT",
                "DRIVER.UNAVAILABLE",
                "DRIVER.INVALID_REQUEST",
            }:
                dispatched = False
                raise
            if dispatched and exc.code in {"DRIVER.ACTION_FAILED", "DRIVER.TIMEOUT"}:
                details.setdefault("action", action)
                details["effect"] = "unknown"
                raise DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "原生动作派发后的结果未知",
                    retryable=False,
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
            if dispatched:
                self._current = None
        return {
            "ok": True,
            "action": action,
            "resolved": self._target(fresh, fresh_node_id),
            "backend_result": _json_safe(backend_result),
        }


class UnavailableBackend:
    """平台、依赖、会话或总线不可用时使用的保守后端。"""

    name = "linux_atspi_unavailable"

    def __init__(self, reason: str, **details: Any) -> None:
        self.reason = reason
        self.details = details

    def session_info(self) -> Mapping[str, Any]:
        return _environment_session_info()

    def _raise(self) -> NoReturn:
        raise DriverError(
            "DRIVER.UNAVAILABLE",
            "Linux AT-SPI 后端不可用",
            data={
                "reason": self.reason,
                "backend": self.name,
                "session": _json_safe(self.session_info()),
                **(_json_safe(self.details) if isinstance(_json_safe(self.details), dict) else {}),
            },
        )

    def list_applications(self, *, deadline: float) -> Sequence[Mapping[str, Any]]:
        _check_deadline(deadline)
        self._raise()

    def capture(
        self,
        application: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot:
        _check_deadline(deadline)
        self._raise()

    def focus(self, native: Any, *, deadline: float) -> Any:
        self._raise()

    def invoke(self, native: Any, *, deadline: float) -> Any:
        self._raise()

    def set_text(self, native: Any, text: str, *, deadline: float) -> Any:
        self._raise()

    def toggle(self, native: Any, *, deadline: float) -> Any:
        self._raise()

    def expand(self, native: Any, *, deadline: float) -> Any:
        self._raise()

    def collapse(self, native: Any, *, deadline: float) -> Any:
        self._raise()

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool:
        self._raise()

    def accessible_at_point(
        self, root: Any, x: int, y: int, *, deadline: float
    ) -> Any | None:
        self._raise()


def _environment_session_info() -> dict[str, Any]:
    """只返回诊断证据，不把环境变量当成资格验证结论。"""

    return {
        "session_type": os.environ.get("XDG_SESSION_TYPE"),
        "desktop": os.environ.get("XDG_CURRENT_DESKTOP") or os.environ.get("DESKTOP_SESSION"),
        "display": os.environ.get("DISPLAY"),
        "wayland_display": os.environ.get("WAYLAND_DISPLAY"),
        "session_bus_advertised": bool(os.environ.get("DBUS_SESSION_BUS_ADDRESS")),
        "atspi_bus_advertised": bool(os.environ.get("AT_SPI_BUS_ADDRESS")),
    }


class PyGObjectAtspiBackend:
    """通过可选 PyGObject ``Atspi 2.0`` typelib 访问当前用户会话。"""

    name = "pygobject_atspi"

    def __init__(self) -> None:
        try:
            import gi

            gi.require_version("Atspi", "2.0")
            from gi.repository import Atspi, GLib
        except (ImportError, ValueError, OSError) as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "PyGObject Atspi 2.0 typelib 未安装或无法加载",
                data={
                    "reason": "dependency_missing",
                    "dependency": "PyGObject Atspi 2.0",
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        self.Atspi = Atspi
        self.GLib = GLib
        try:
            status = int(Atspi.init())
            if status not in (0, 1) or not bool(Atspi.is_initialized()):
                raise RuntimeError(f"Atspi.init status={status}")
            if int(Atspi.get_desktop_count()) < 1:
                raise RuntimeError("AT-SPI desktop 不存在")
            desktop = Atspi.get_desktop(0)
            if desktop is None:
                raise RuntimeError("AT-SPI desktop 0 不可访问")
            child_count = int(desktop.get_child_count())
            if child_count < 0:
                raise RuntimeError("AT-SPI desktop child count 无效")
            self.desktop = desktop
        except Exception as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "无法初始化当前会话的 AT-SPI desktop",
                data={
                    "reason": "session_or_bus_unavailable",
                    "exception_type": type(exc).__name__,
                    "session": _environment_session_info(),
                },
            ) from exc

    def session_info(self) -> Mapping[str, Any]:
        return _environment_session_info()

    def _bound_timeout(self, deadline: float) -> None:
        """把 libatspi 的单次同步调用限制在请求剩余预算内。"""

        _check_deadline(deadline)
        remaining_ms = max(1, min(30_000, int(math.ceil((deadline - time.monotonic()) * 1000))))
        try:
            self.Atspi.set_timeout(remaining_ms, remaining_ms)
        except Exception as exc:
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                "无法设置 AT-SPI 调用截止时间",
                data={"exception_type": type(exc).__name__},
            ) from exc

    def _call_default(
        self,
        obj: Any,
        method: str,
        default: Any = None,
        *args: Any,
        deadline: float | None = None,
    ) -> Any:
        if deadline is not None:
            self._bound_timeout(deadline)
        try:
            result = getattr(obj, method)(*args)
        except Exception:
            if deadline is not None:
                _check_deadline(deadline)
            return default
        if deadline is not None:
            _check_deadline(deadline)
        return result

    @staticmethod
    def _property_default(obj: Any, name: str, default: Any = None) -> Any:
        try:
            return getattr(obj, name)
        except Exception:
            return default

    def _native_failure(self, operation: str, exc: BaseException) -> DriverError:
        return DriverError(
            "DRIVER.ACTION_FAILED",
            f"AT-SPI 原生操作 {operation} 失败",
            data={"operation": operation, "exception_type": type(exc).__name__},
        )

    def _identity(self, accessible: Any) -> tuple[str | None, str | None]:
        application = self._property_default(accessible, "app")
        bus_name = self._property_default(application, "bus_name")
        object_path = self._property_default(accessible, "path")
        return _safe_text(bus_name), _safe_text(object_path)

    def _application_info(
        self, application: Any, *, deadline: float
    ) -> dict[str, Any]:
        bus_name, object_path = self._identity(application)
        process_id = self._call_default(
            application, "get_process_id", deadline=deadline
        )
        if isinstance(process_id, bool) or not isinstance(process_id, int) or process_id < 0:
            process_id = None
        return {
            "bus_name": bus_name,
            "object_path": object_path,
            "name": _safe_text(
                self._call_default(application, "get_name", deadline=deadline)
            ),
            "process_id": process_id,
            "toolkit_name": _safe_text(
                self._call_default(
                    application, "get_toolkit_name", deadline=deadline
                )
            ),
            "toolkit_version": _safe_text(
                self._call_default(
                    application, "get_toolkit_version", deadline=deadline
                )
            ),
            "atspi_version": _safe_text(
                self._call_default(
                    application, "get_atspi_version", deadline=deadline
                )
            ),
            "locale": _safe_text(
                self._call_default(
                    application, "get_object_locale", deadline=deadline
                )
            ),
        }

    def _applications(self, *, deadline: float) -> list[tuple[Any, dict[str, Any]]]:
        _check_deadline(deadline)
        self._bound_timeout(deadline)
        try:
            count = int(self.desktop.get_child_count())
        except Exception as exc:
            raise self._native_failure("desktop.get_child_count", exc) from exc
        if count < 0:
            raise DriverError(
                "DRIVER.ACTION_FAILED", "AT-SPI desktop 返回了无效子节点数量"
            )
        result: list[tuple[Any, dict[str, Any]]] = []
        for index in range(count):
            _check_deadline(deadline)
            self._bound_timeout(deadline)
            try:
                application = self.desktop.get_child_at_index(index)
            except Exception:
                # 应用可能在枚举期间退出；跳过单个消失的代理。
                continue
            if application is None:
                continue
            result.append(
                (application, self._application_info(application, deadline=deadline))
            )
        return result

    def list_applications(self, *, deadline: float) -> Sequence[Mapping[str, Any]]:
        return [info for _native, info in self._applications(deadline=deadline)]

    def _resolve_application(
        self, selector: Mapping[str, Any], *, deadline: float
    ) -> tuple[Any, dict[str, Any]]:
        candidates = [
            (native, info)
            for native, info in self._applications(deadline=deadline)
            if all(info.get(key) == value for key, value in selector.items())
        ]
        if not candidates:
            raise DriverError(
                "DRIVER.NOT_FOUND",
                "应用选择器没有匹配 AT-SPI application",
                data={"application": dict(selector)},
            )
        if len(candidates) > 1:
            raise DriverError(
                "DRIVER.AMBIGUOUS",
                "应用选择器匹配多个 AT-SPI application",
                data={
                    "candidate_count": len(candidates),
                    "candidates": [info for _native, info in candidates[:MAX_CANDIDATE_SUMMARIES]],
                },
            )
        return candidates[0]

    def _state(
        self, state_set: Any, state_name: str, *, deadline: float
    ) -> bool | None:
        state = getattr(self.Atspi.StateType, state_name, None)
        if state is None or state_set is None:
            return None
        self._bound_timeout(deadline)
        try:
            result = bool(state_set.contains(state))
        except Exception:
            _check_deadline(deadline)
            return None
        _check_deadline(deadline)
        return result

    def _interface(
        self, accessible: Any, getter: str, *, deadline: float
    ) -> Any | None:
        return self._call_default(
            accessible, getter, deadline=deadline
        )

    def _read_text(
        self, accessible: Any, *, protected: bool, deadline: float
    ) -> str | None:
        if protected:
            return None
        text_iface = self._interface(
            accessible, "get_text_iface", deadline=deadline
        )
        if text_iface is None:
            return None
        raw_count = self._call_default(
            text_iface, "get_character_count", deadline=deadline
        )
        try:
            count = max(0, int(raw_count))
        except (TypeError, ValueError, OverflowError):
            return None
        # 线路字段本身有字符上限；不为超长文档读取无界文本。
        return _safe_text(
            self._call_default(
                text_iface,
                "get_text",
                None,
                0,
                min(count, MAX_FIELD_CHARS),
                deadline=deadline,
            )
        )

    def _bounds(
        self, accessible: Any, *, deadline: float
    ) -> dict[str, int] | None:
        component = self._interface(
            accessible, "get_component_iface", deadline=deadline
        )
        if component is None:
            return None
        self._bound_timeout(deadline)
        try:
            rectangle = component.get_extents(self.Atspi.CoordType.SCREEN)
            _check_deadline(deadline)
            return {
                "x": int(rectangle.x),
                "y": int(rectangle.y),
                "width": max(0, int(rectangle.width)),
                "height": max(0, int(rectangle.height)),
            }
        except Exception:
            _check_deadline(deadline)
            return None

    def accessible_at_point(
        self, root: Any, x: int, y: int, *, deadline: float
    ) -> Any | None:
        coord_type = getattr(getattr(self.Atspi, "CoordType", None), "SCREEN", None)
        if coord_type is None:
            return None

        seen: set[tuple[str | None, str | None]] = set()
        # A provider can return itself, an ancestor, or a short cycle for
        # implementation containers. Keep this lookup independently bounded
        # from the snapshot traversal.
        remaining_hops = MAX_DEPTH + 1

        def descend(accessible: Any, screen_x: int, screen_y: int) -> Any | None:
            nonlocal remaining_hops
            if remaining_hops <= 0:
                return accessible
            remaining_hops -= 1
            identity = self._identity(accessible)
            if all(identity):
                if identity in seen:
                    return accessible
                seen.add(identity)
            # A fresh tree already bounds candidate descendants.  Avoid asking
            # Qt container nodes without a usable on-screen rectangle: some
            # bridges expose Component but block in get_accessible_at_point for
            # hidden/zero-area implementation containers.
            bounds = self._bounds(accessible, deadline=deadline)
            if (
                bounds is None
                or bounds["width"] <= 0
                or bounds["height"] <= 0
                or screen_x < bounds["x"]
                or screen_y < bounds["y"]
                or screen_x >= bounds["x"] + bounds["width"]
                or screen_y >= bounds["y"] + bounds["height"]
            ):
                return None
            component = self._interface(
                accessible, "get_component_iface", deadline=deadline
            )
            if component is None:
                return None
            self._bound_timeout(deadline)
            try:
                hit = component.get_accessible_at_point(
                    int(screen_x), int(screen_y), coord_type
                )
            except Exception:
                _check_deadline(deadline)
                return None
            _check_deadline(deadline)
            if hit is None:
                return None
            if self.same_element(hit, accessible, deadline=deadline):
                return hit
            hit_identity = self._identity(hit)
            if all(hit_identity) and hit_identity in seen:
                return hit
            deeper = descend(hit, screen_x, screen_y)
            return deeper if deeper is not None else hit

        return descend(root, int(x), int(y))

    def _action_metadata(
        self, accessible: Any, *, deadline: float
    ) -> list[dict[str, Any]]:
        action_iface = self._interface(
            accessible, "get_action_iface", deadline=deadline
        )
        if action_iface is None:
            return []
        raw_count = self._call_default(
            action_iface, "get_n_actions", deadline=deadline
        )
        try:
            count = max(0, int(raw_count))
        except (TypeError, ValueError, OverflowError):
            return []
        actions: list[dict[str, Any]] = []
        for index in range(min(count, 64)):
            actions.append(
                {
                    "index": index,
                    "name": _safe_text(
                        self._call_default(
                            action_iface, "get_action_name", None, index, deadline=deadline
                        )
                    ),
                    "localized_name": _safe_text(
                        self._call_default(
                            action_iface, "get_localized_name", None, index, deadline=deadline
                        )
                    ),
                    "description": _safe_text(
                        self._call_default(
                            action_iface, "get_action_description", None, index, deadline=deadline
                        )
                    ),
                    "key_binding": _safe_text(
                        self._call_default(
                            action_iface, "get_key_binding", None, index, deadline=deadline
                        )
                    ),
                }
            )
        return actions

    @staticmethod
    def _exact_native_action(
        metadata: Sequence[Mapping[str, Any]], native_action_name: str
    ) -> dict[str, Any] | None:
        """Return one exact canonical action-name match, never an alias."""

        matches = [
            dict(item) for item in metadata if item.get("name") == native_action_name
        ]
        return matches[0] if len(matches) == 1 else None

    def _read_node(
        self,
        accessible: Any,
        parent_index: int | None,
        application: Mapping[str, Any],
        *,
        deadline: float,
    ) -> BackendNode:
        state_set = self._call_default(
            accessible, "get_state_set", deadline=deadline
        )
        role_name = _safe_text(
            self._call_default(accessible, "get_role_name", deadline=deadline)
        ) or "unknown"
        role_enum = self._call_default(accessible, "get_role", deadline=deadline)
        role_nick = _safe_text(getattr(role_enum, "value_nick", None))
        role = _normalize_role(role_nick or role_name)
        protected_state = self._state(state_set, "PROTECTED", deadline=deadline)
        protected = True if _is_protected_role(role) else protected_state
        editable_iface = self._interface(
            accessible, "get_editable_text_iface", deadline=deadline
        )
        component_iface = self._interface(
            accessible, "get_component_iface", deadline=deadline
        )
        action_metadata = self._action_metadata(accessible, deadline=deadline)
        enabled = self._state(state_set, "ENABLED", deadline=deadline)
        visible = self._state(state_set, "VISIBLE", deadline=deadline)
        showing = self._state(state_set, "SHOWING", deadline=deadline)
        sensitive = self._state(state_set, "SENSITIVE", deadline=deadline)
        focusable = self._state(state_set, "FOCUSABLE", deadline=deadline)
        editable = self._state(state_set, "EDITABLE", deadline=deadline)
        checked = self._state(state_set, "CHECKED", deadline=deadline)
        expandable = self._state(state_set, "EXPANDABLE", deadline=deadline)
        expanded = self._state(state_set, "EXPANDED", deadline=deadline)
        selectable = self._state(state_set, "SELECTABLE", deadline=deadline)
        selected = self._state(state_set, "SELECTED", deadline=deadline)
        actionable = enabled is not False and sensitive is not False
        toolkit_name = _safe_text(application.get("toolkit_name"))
        toolkit_version = _safe_text(application.get("toolkit_version"))
        is_qualified_qt5 = (
            actionable
            and toolkit_name == "Qt"
            and (toolkit_version or "").startswith("5.")
        )
        actions: list[str] = []
        if actionable and focusable is not False and component_iface is not None:
            actions.append("focus")
        # AT-SPI Action 没有统一 default-action 标识。只在恰好一个动作时公开 invoke，
        # 从而避免静默选择一个多义的索引。
        if (
            is_qualified_qt5
            and role == "push_button"
            and self._exact_native_action(
                action_metadata, QT5_NATIVE_ACTION_NAMES["invoke"]
            )
        ):
            actions.append("invoke")
        elif actionable and not is_qualified_qt5 and len(action_metadata) == 1:
            actions.append("invoke")
        if (
            actionable
            and editable is not False
            and editable_iface is not None
            and protected is not True
        ):
            actions.append("set_text")
        process_id = application.get("process_id")
        if (
            enabled is True
            and visible is True
            and showing is True
            and sensitive is True
            and focusable is True
            and editable is True
            and protected is not True
            and component_iface is not None
            and role in TYPE_TEXT_ROLES
            and isinstance(process_id, int)
            and not isinstance(process_id, bool)
            and process_id > 0
        ):
            actions.append("type_text")
        bounds = self._bounds(accessible, deadline=deadline)
        if (
            enabled is True
            and visible is True
            and showing is True
            and sensitive is True
            and focusable is True
            and component_iface is not None
            and protected is not True
            and isinstance(process_id, int)
            and not isinstance(process_id, bool)
            and process_id > 0
            and bounds is not None
            and bounds.get("width", 0) > 0
            and bounds.get("height", 0) > 0
            and type(self).accessible_at_point is not UnavailableBackend.accessible_at_point
        ):
            actions.append("pointer_click")
        native_action_names: dict[str, str] = {}
        if "invoke" in actions and is_qualified_qt5:
            native_action_names["invoke"] = QT5_NATIVE_ACTION_NAMES["invoke"]
        # This mapping is qualified only for GTK3.  Match the canonical AT-SPI
        # action name byte-for-byte; localized labels and descriptions are not
        # capability evidence.
        is_qualified_gtk3 = (
            actionable
            and toolkit_name == "gtk"
            and (toolkit_version or "").startswith("3.")
        )
        if is_qualified_gtk3:
            if (
                role in {"check_box", "toggle_button"}
                and checked is not None
                and self._exact_native_action(
                    action_metadata, GTK3_NATIVE_ACTION_NAMES["toggle"]
                )
            ):
                actions.append("toggle")
                native_action_names["toggle"] = GTK3_NATIVE_ACTION_NAMES["toggle"]
            if (
                expandable is True
                and expanded is not None
                and self._exact_native_action(
                    action_metadata, GTK3_NATIVE_ACTION_NAMES["expand"]
                )
            ):
                actions.extend(("expand", "collapse"))
                native_action_names["expand"] = GTK3_NATIVE_ACTION_NAMES["expand"]
                native_action_names["collapse"] = GTK3_NATIVE_ACTION_NAMES["collapse"]
        raw_attributes = self._call_default(
            accessible, "get_attributes", {}, deadline=deadline
        )
        attributes: dict[str, str] = {}
        if isinstance(raw_attributes, Mapping):
            for key, value in raw_attributes.items():
                safe_key = _safe_text(key)
                safe_value = _safe_text(value)
                if safe_key is not None and safe_value is not None:
                    attributes[safe_key] = safe_value
        bus_name, object_path = self._identity(accessible)
        accessible_id = _safe_text(
            self._call_default(
                accessible, "get_accessible_id", deadline=deadline
            )
        )
        return BackendNode(
            native=accessible,
            parent_index=parent_index,
            role=role,
            name=_safe_text(
                self._call_default(accessible, "get_name", deadline=deadline)
            ),
            description=_safe_text(
                self._call_default(
                    accessible, "get_description", deadline=deadline
                )
            ),
            # 无法确认 role 时也不读取 Text，避免属性读取失败后泄露密码内容。
            value=self._read_text(
                accessible,
                protected=protected is True or role == "unknown",
                deadline=deadline,
            ),
            attributes=attributes,
            states={
                "enabled": enabled,
                "visible": visible,
                "showing": showing,
                "focusable": focusable,
                "focused": self._state(state_set, "FOCUSED", deadline=deadline),
                "editable": editable,
                "sensitive": sensitive,
                "protected": protected,
                "checked": checked,
                "expandable": expandable,
                "expanded": expanded,
                "selectable": selectable,
                "selected": selected,
            },
            bounds=bounds,
            actions=actions,
            provenance={
                "bus_name": bus_name,
                "object_path": object_path,
                "accessible_id": accessible_id,
                "application_name": application.get("name"),
                "toolkit_name": application.get("toolkit_name"),
                "toolkit_version": application.get("toolkit_version"),
                "process_id": application.get("process_id"),
                "value_redacted": protected is True,
                "coordinate_space": "screen",
                "atspi_actions": action_metadata,
                "native_action_name": (
                    next(iter(set(native_action_names.values())))
                    if len(set(native_action_names.values())) == 1
                    else None
                ),
                "native_action_names": native_action_names,
            },
        )

    def capture(
        self,
        application: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot:
        root, info = self._resolve_application(application, deadline=deadline)
        queue: deque[tuple[Any, int | None, int]] = deque([(root, None, 0)])
        nodes: list[BackendNode] = []
        truncated = False
        while queue:
            _check_deadline(deadline)
            self._bound_timeout(deadline)
            if len(nodes) >= max_nodes:
                truncated = True
                break
            accessible, parent_index, depth = queue.popleft()
            current_index = len(nodes)
            nodes.append(
                self._read_node(
                    accessible, parent_index, info, deadline=deadline
                )
            )
            self._bound_timeout(deadline)
            try:
                child_count = int(accessible.get_child_count())
            except Exception:
                # 无法证明没有剩余子树时必须失败关闭。
                truncated = True
                continue
            if child_count < 0:
                truncated = True
                continue
            if depth >= max_depth:
                if child_count > 0:
                    truncated = True
                continue
            for child_index in range(child_count):
                _check_deadline(deadline)
                if len(nodes) + len(queue) >= max_nodes:
                    truncated = True
                    break
                try:
                    self._bound_timeout(deadline)
                    child = accessible.get_child_at_index(child_index)
                except Exception:
                    truncated = True
                    continue
                if child is None:
                    truncated = True
                    continue
                queue.append((child, current_index, depth + 1))
        return BackendSnapshot(application=info, nodes=nodes, truncated=truncated)

    def focus(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        component = self._interface(
            native, "get_component_iface", deadline=deadline
        )
        if component is None:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED", "目标没有 AT-SPI Component 接口"
            )
        _check_deadline(deadline)
        try:
            accepted = bool(component.grab_focus())
        except Exception as exc:
            raise self._native_failure("Component.grab_focus", exc) from exc
        if not accepted:
            raise DriverError("DRIVER.ACTION_FAILED", "Component.grab_focus 未接受请求")
        return {"native_interface": "Component.grab_focus", "accepted": True}

    def invoke(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        action_iface = self._interface(
            native, "get_action_iface", deadline=deadline
        )
        if action_iface is None:
            raise DriverError("DRIVER.ACTION_UNSUPPORTED", "目标没有 AT-SPI Action 接口")
        metadata = self._action_metadata(native, deadline=deadline)
        application = self._property_default(native, "app")
        toolkit_name = _safe_text(
            self._call_default(application, "get_toolkit_name", deadline=deadline)
        )
        toolkit_version = _safe_text(
            self._call_default(application, "get_toolkit_version", deadline=deadline)
        )
        if (toolkit_name is None or toolkit_version is None) and hasattr(self, "desktop"):
            bus_name, _object_path = self._identity(native)
            application_info = next(
                (
                    info
                    for _application, info in self._applications(deadline=deadline)
                    if info.get("bus_name") == bus_name
                ),
                {},
            )
            toolkit_name = _safe_text(application_info.get("toolkit_name"))
            toolkit_version = _safe_text(application_info.get("toolkit_version"))
        role_enum = self._call_default(native, "get_role", deadline=deadline)
        role = _normalize_role(
            getattr(role_enum, "value_nick", None)
            or self._call_default(native, "get_role_name", deadline=deadline)
        )
        selected: dict[str, Any] | None
        if (
            toolkit_name == "Qt"
            and (toolkit_version or "").startswith("5.")
            and role == "push_button"
        ):
            selected = self._exact_native_action(
                metadata, QT5_NATIVE_ACTION_NAMES["invoke"]
            )
        else:
            selected = dict(metadata[0]) if len(metadata) == 1 else None
        if selected is None:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                "AT-SPI invoke 没有唯一且经过资格验证的原生动作",
                data={
                    "native_action_count": len(metadata),
                    "available_native_actions": metadata,
                },
            )
        _check_deadline(deadline)
        try:
            accepted = bool(action_iface.do_action(int(selected["index"])))
        except Exception as exc:
            raise self._native_failure("Action.do_action", exc) from exc
        if not accepted:
            raise DriverError("DRIVER.ACTION_FAILED", "Action.do_action 未接受请求")
        return {
            "native_interface": "Action.do_action",
            "accepted": True,
            "native_action_name": selected["name"],
            "native_action": selected,
        }

    def _do_exact_named_action(
        self, native: Any, action: str, *, deadline: float
    ) -> dict[str, Any]:
        _check_deadline(deadline)
        native_action_name = GTK3_NATIVE_ACTION_NAMES[action]
        action_iface = self._interface(
            native, "get_action_iface", deadline=deadline
        )
        if action_iface is None:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED", "目标没有 AT-SPI Action 接口"
            )
        metadata = self._action_metadata(native, deadline=deadline)
        selected = self._exact_native_action(metadata, native_action_name)
        if selected is None:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED",
                f"目标没有唯一的原生 {native_action_name} 动作",
                data={
                    "action": action,
                    "native_action_name": native_action_name,
                    "available_native_actions": metadata,
                },
            )
        _check_deadline(deadline)
        try:
            accepted = bool(action_iface.do_action(int(selected["index"])))
        except Exception as exc:
            raise self._native_failure("Action.do_action", exc) from exc
        if not accepted:
            raise DriverError(
                "DRIVER.ACTION_FAILED", "Action.do_action 未接受请求"
            )
        return {
            "native_interface": "Action.do_action",
            "native_action_name": native_action_name,
            "native_action": selected,
            "accepted": True,
            "dispatched": True,
            "no_op": False,
        }

    def toggle(self, native: Any, *, deadline: float) -> Any:
        return self._do_exact_named_action(native, "toggle", deadline=deadline)

    def expand(self, native: Any, *, deadline: float) -> Any:
        return self._do_exact_named_action(native, "expand", deadline=deadline)

    def collapse(self, native: Any, *, deadline: float) -> Any:
        return self._do_exact_named_action(native, "collapse", deadline=deadline)

    def set_text(self, native: Any, text: str, *, deadline: float) -> Any:
        _check_deadline(deadline)
        editable = self._interface(
            native, "get_editable_text_iface", deadline=deadline
        )
        if editable is None:
            raise DriverError(
                "DRIVER.ACTION_UNSUPPORTED", "目标没有 AT-SPI EditableText 接口"
            )
        state_set = self._call_default(
            native, "get_state_set", deadline=deadline
        )
        protected = self._state(state_set, "PROTECTED", deadline=deadline)
        role_enum = self._call_default(native, "get_role", deadline=deadline)
        role_nick = getattr(role_enum, "value_nick", None)
        role = _normalize_role(
            role_nick
            or self._call_default(native, "get_role_name", deadline=deadline)
        )
        if protected is True or _is_protected_role(role):
            raise DriverError("DRIVER.PROTECTED_ELEMENT", "受保护文本元素禁止 set_text")
        _check_deadline(deadline)
        try:
            accepted = bool(editable.set_text_contents(text))
        except Exception as exc:
            raise self._native_failure("EditableText.set_text_contents", exc) from exc
        if not accepted:
            raise DriverError(
                "DRIVER.ACTION_FAILED", "EditableText.set_text_contents 未接受请求"
            )
        return {
            "native_interface": "EditableText.set_text_contents",
            "accepted": True,
        }

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool:
        _check_deadline(deadline)
        previous_identity = self._identity(previous)
        current_identity = self._identity(current)
        if not all(previous_identity) or not all(current_identity):
            return False
        previous_accessible_id = _safe_text(
            self._call_default(
                previous, "get_accessible_id", deadline=deadline
            )
        )
        current_accessible_id = _safe_text(
            self._call_default(
                current, "get_accessible_id", deadline=deadline
            )
        )
        if (
            previous_accessible_id
            and current_accessible_id
            and previous_identity == current_identity
        ):
            return previous_accessible_id == current_accessible_id
        # libatspi child proxies do not reliably retain ``app``. Resolve the
        # bus owner through the desktop root; otherwise a proxy whose ``app``
        # property exists but is null would erase the qualified identity.
        applications = {
            info.get("bus_name"): info
            for _native, info in self._applications(deadline=deadline)
        }
        previous_info = applications.get(previous_identity[0], {})
        current_info = applications.get(current_identity[0], {})
        previous_toolkit = _safe_text(previous_info.get("toolkit_name"))
        current_toolkit = _safe_text(current_info.get("toolkit_name"))
        previous_version = _safe_text(previous_info.get("toolkit_version"))
        current_version = _safe_text(current_info.get("toolkit_version"))
        previous_pid = previous_info.get("process_id")
        current_pid = current_info.get("process_id")
        return bool(
            previous_identity == current_identity
            and previous_identity[1]
            and previous_identity[1] != "/org/a11y/atspi/accessible/root"
            and previous_toolkit == current_toolkit == "Qt"
            and (previous_version or "").startswith("5.")
            and previous_version == current_version
            and isinstance(previous_pid, int)
            and not isinstance(previous_pid, bool)
            and previous_pid > 0
            and previous_pid == current_pid
        )

@dataclass(frozen=True, slots=True)
class GioAccessibleRef:
    """Gio fallback 使用的当前 accessibility bus 对象引用。"""

    bus_name: str
    object_path: str


class GioAtspiBackend:
    """无需 Atspi typelib 的只读 Gio D-Bus fallback。

    该后端只连接当前进程的 session bus，并通过 ``org.a11y.Bus.GetAddress``
    取得 accessibility bus；不会扫描进程或连接其他用户会话。
    """

    name = "gio_atspi"
    ACCESSIBLE = "org.a11y.atspi.Accessible"
    APPLICATION = "org.a11y.atspi.Application"
    COMPONENT = "org.a11y.atspi.Component"
    TEXT = "org.a11y.atspi.Text"
    PROPERTIES = "org.freedesktop.DBus.Properties"
    DBUS = "org.freedesktop.DBus"
    REGISTRY = GioAccessibleRef(
        "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root"
    )
    STATE_INDEXES = {
        "checked": 4,
        "editable": 7,
        "enabled": 8,
        "expandable": 9,
        "expanded": 10,
        "focusable": 11,
        "focused": 12,
        "selectable": 22,
        "selected": 23,
        "sensitive": 24,
        "showing": 25,
        "visible": 30,
        "read_only": 43,
    }

    def __init__(self) -> None:
        try:
            import gi

            gi.require_version("Gio", "2.0")
            from gi.repository import Gio, GLib
        except (ImportError, ValueError, OSError) as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "PyGObject Gio 2.0 typelib 未安装或无法加载",
                data={
                    "reason": "dependency_missing",
                    "dependency": "PyGObject Gio 2.0",
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        self.Gio = Gio
        self.GLib = GLib
        try:
            self.session_bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            address_reply = self.session_bus.call_sync(
                "org.a11y.Bus",
                "/org/a11y/bus",
                "org.a11y.Bus",
                "GetAddress",
                None,
                GLib.VariantType.new("(s)"),
                Gio.DBusCallFlags.NONE,
                3000,
                None,
            )
            address = address_reply.unpack()[0]
            if not isinstance(address, str) or not address:
                raise RuntimeError("org.a11y.Bus 返回空地址")
            flags = (
                Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
                | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION
            )
            self.connection = Gio.DBusConnection.new_for_address_sync(
                address, flags, None, None
            )
            # 用真实 registry 读取证明该地址确实是可访问的 AT-SPI bus。
            self._children(self.REGISTRY, deadline=time.monotonic() + 3.0)
        except DriverError as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "当前会话的 AT-SPI accessibility bus 不可用",
                data={
                    "reason": "session_or_bus_unavailable",
                    "cause_code": exc.code,
                    "cause_data": _json_safe(exc.data),
                    "session": _environment_session_info(),
                },
            ) from exc
        except Exception as exc:
            raise DriverError(
                "DRIVER.UNAVAILABLE",
                "无法通过当前 session bus 连接 AT-SPI accessibility bus",
                data={
                    "reason": "session_or_bus_unavailable",
                    "exception_type": type(exc).__name__,
                    "session": _environment_session_info(),
                },
            ) from exc

    def session_info(self) -> Mapping[str, Any]:
        return _environment_session_info()

    def _timeout_ms(self, deadline: float) -> int:
        _check_deadline(deadline)
        remaining = deadline - time.monotonic()
        return max(1, min(30_000, int(math.ceil(remaining * 1000.0))))

    def _variant(self, signature: str, values: tuple[Any, ...]) -> Any:
        return self.GLib.Variant(signature, values)

    def _call(
        self,
        ref: GioAccessibleRef,
        interface: str,
        method: str,
        *,
        parameters: Any = None,
        reply_type: str | None = None,
        deadline: float,
    ) -> tuple[Any, ...]:
        _check_deadline(deadline)
        try:
            reply = self.connection.call_sync(
                ref.bus_name,
                ref.object_path,
                interface,
                method,
                parameters,
                None if reply_type is None else self.GLib.VariantType.new(reply_type),
                self.Gio.DBusCallFlags.NONE,
                self._timeout_ms(deadline),
                None,
            )
            unpacked = reply.unpack()
        except Exception as exc:
            if time.monotonic() >= deadline:
                _check_deadline(deadline)
            raise DriverError(
                "DRIVER.ACTION_FAILED",
                f"AT-SPI D-Bus 调用 {interface}.{method} 失败",
                data={
                    "interface": interface,
                    "method": method,
                    "bus_name": ref.bus_name,
                    "object_path": ref.object_path,
                    "exception_type": type(exc).__name__,
                },
            ) from exc
        _check_deadline(deadline)
        if not isinstance(unpacked, tuple):
            raise DriverError("DRIVER.ACTION_FAILED", "AT-SPI D-Bus 返回了无效响应")
        return unpacked

    def _try_call(
        self,
        ref: GioAccessibleRef,
        interface: str,
        method: str,
        *,
        parameters: Any = None,
        reply_type: str | None = None,
        deadline: float,
    ) -> tuple[Any, ...] | None:
        try:
            return self._call(
                ref,
                interface,
                method,
                parameters=parameters,
                reply_type=reply_type,
                deadline=deadline,
            )
        except DriverError as exc:
            if exc.code == "DRIVER.TIMEOUT":
                raise
            return None

    def _get_all(self, ref: GioAccessibleRef, *, deadline: float) -> dict[str, Any]:
        reply = self._call(
            ref,
            self.PROPERTIES,
            "GetAll",
            parameters=self._variant("(s)", (self.ACCESSIBLE,)),
            reply_type="(a{sv})",
            deadline=deadline,
        )
        properties = reply[0]
        if not isinstance(properties, Mapping):
            raise DriverError("DRIVER.ACTION_FAILED", "Accessible.GetAll 返回无效属性")
        return dict(properties)

    def _get_property(
        self, ref: GioAccessibleRef, interface: str, name: str, *, deadline: float
    ) -> Any:
        reply = self._try_call(
            ref,
            self.PROPERTIES,
            "Get",
            parameters=self._variant("(ss)", (interface, name)),
            reply_type="(v)",
            deadline=deadline,
        )
        return None if reply is None else reply[0]

    @staticmethod
    def _reference(raw: Any) -> GioAccessibleRef | None:
        if not isinstance(raw, (tuple, list)) or len(raw) != 2:
            return None
        bus_name, object_path = raw
        if not isinstance(bus_name, str) or not bus_name.startswith(":"):
            return None
        if not isinstance(object_path, str) or not object_path.startswith("/"):
            return None
        return GioAccessibleRef(bus_name, object_path)

    def _children(self, ref: GioAccessibleRef, *, deadline: float) -> list[GioAccessibleRef]:
        reply = self._call(
            ref, self.ACCESSIBLE, "GetChildren", reply_type="(a(so))", deadline=deadline
        )
        if not isinstance(reply[0], (list, tuple)):
            raise DriverError("DRIVER.ACTION_FAILED", "Accessible.GetChildren 返回无效列表")
        if len(reply[0]) > MAX_DBUS_CHILDREN_PER_CALL:
            raise DriverError(
                "DRIVER.OUTPUT_TOO_LARGE",
                "Accessible.GetChildren 响应超过单次硬上限",
                data={
                    "child_count": len(reply[0]),
                    "limit": MAX_DBUS_CHILDREN_PER_CALL,
                    "bus_name": ref.bus_name,
                    "object_path": ref.object_path,
                },
            )
        children: list[GioAccessibleRef] = []
        for raw in reply[0]:
            child = self._reference(raw)
            if child is not None:
                children.append(child)
        return children

    def _process_id(self, bus_name: str, *, deadline: float) -> int | None:
        reply = self._try_call(
            GioAccessibleRef("org.freedesktop.DBus", "/org/freedesktop/DBus"),
            self.DBUS,
            "GetConnectionUnixProcessID",
            parameters=self._variant("(s)", (bus_name,)),
            reply_type="(u)",
            deadline=deadline,
        )
        if reply is None or isinstance(reply[0], bool) or not isinstance(reply[0], int):
            return None
        return int(reply[0])

    def _application_info(
        self, ref: GioAccessibleRef, *, deadline: float
    ) -> dict[str, Any]:
        properties = self._get_all(ref, deadline=deadline)
        return {
            "bus_name": ref.bus_name,
            "object_path": ref.object_path,
            "name": _safe_text(properties.get("Name")),
            "process_id": self._process_id(ref.bus_name, deadline=deadline),
            "toolkit_name": _safe_text(
                self._get_property(ref, self.APPLICATION, "ToolkitName", deadline=deadline)
            ),
            "toolkit_version": _safe_text(
                self._get_property(ref, self.APPLICATION, "Version", deadline=deadline)
            ),
            "atspi_version": _safe_text(
                self._get_property(ref, self.APPLICATION, "AtspiVersion", deadline=deadline)
            ),
            "application_id": self._get_property(
                ref, self.APPLICATION, "Id", deadline=deadline
            ),
            "locale": _safe_text(properties.get("Locale")),
        }

    def _applications(
        self, *, deadline: float
    ) -> list[tuple[GioAccessibleRef, dict[str, Any]]]:
        result: list[tuple[GioAccessibleRef, dict[str, Any]]] = []
        for ref in self._children(self.REGISTRY, deadline=deadline):
            _check_deadline(deadline)
            try:
                result.append((ref, self._application_info(ref, deadline=deadline)))
            except DriverError as exc:
                if exc.code == "DRIVER.TIMEOUT":
                    raise
                # 应用可能在枚举期间退出；只跳过该失效对象。
                continue
        return result

    def list_applications(self, *, deadline: float) -> Sequence[Mapping[str, Any]]:
        return [info for _ref, info in self._applications(deadline=deadline)]

    def _resolve_application(
        self, selector: Mapping[str, Any], *, deadline: float
    ) -> tuple[GioAccessibleRef, dict[str, Any]]:
        candidates = [
            (ref, info)
            for ref, info in self._applications(deadline=deadline)
            if all(info.get(key) == value for key, value in selector.items())
        ]
        if not candidates:
            raise DriverError(
                "DRIVER.NOT_FOUND",
                "应用选择器没有匹配 AT-SPI application",
                data={"application": dict(selector)},
            )
        if len(candidates) > 1:
            raise DriverError(
                "DRIVER.AMBIGUOUS",
                "应用选择器匹配多个 AT-SPI application",
                data={
                    "candidate_count": len(candidates),
                    "candidates": [info for _ref, info in candidates[:MAX_CANDIDATE_SUMMARIES]],
                },
            )
        return candidates[0]

    def _interfaces(self, ref: GioAccessibleRef, *, deadline: float) -> set[str]:
        reply = self._try_call(
            ref, self.ACCESSIBLE, "GetInterfaces", reply_type="(as)", deadline=deadline
        )
        if reply is None or not isinstance(reply[0], (list, tuple)):
            return set()
        return {str(item) for item in reply[0] if isinstance(item, str)}

    def _state_values(self, ref: GioAccessibleRef, *, deadline: float) -> dict[str, bool | None]:
        reply = self._try_call(
            ref, self.ACCESSIBLE, "GetState", reply_type="(au)", deadline=deadline
        )
        words = None if reply is None else reply[0]
        if not isinstance(words, (list, tuple)):
            return {name: None for name in STATE_NAMES}

        def present(index: int) -> bool:
            word = index // 32
            return word < len(words) and bool(int(words[word]) & (1 << (index % 32)))

        values = {
            name: present(index)
            for name, index in self.STATE_INDEXES.items()
            if name != "read_only"
        }
        values["protected"] = None
        return values

    def _role(self, ref: GioAccessibleRef, *, deadline: float) -> str:
        reply = self._call(
            ref, self.ACCESSIBLE, "GetRoleName", reply_type="(s)", deadline=deadline
        )
        return _normalize_role(reply[0])

    def _bounds(
        self, ref: GioAccessibleRef, interfaces: set[str], *, deadline: float
    ) -> dict[str, int] | None:
        if self.COMPONENT not in interfaces:
            return None
        reply = self._try_call(
            ref,
            self.COMPONENT,
            "GetExtents",
            parameters=self._variant("(u)", (0,)),
            reply_type="((iiii))",
            deadline=deadline,
        )
        if reply is None or not isinstance(reply[0], (tuple, list)) or len(reply[0]) != 4:
            return None
        try:
            x, y, width, height = (int(item) for item in reply[0])
        except (TypeError, ValueError, OverflowError):
            return None
        return {"x": x, "y": y, "width": max(0, width), "height": max(0, height)}

    def _text_value(
        self,
        ref: GioAccessibleRef,
        interfaces: set[str],
        *,
        protected: bool,
        deadline: float,
    ) -> str | None:
        if protected or self.TEXT not in interfaces:
            return None
        count = self._get_property(ref, self.TEXT, "CharacterCount", deadline=deadline)
        if isinstance(count, bool) or not isinstance(count, int) or count < 0:
            return None
        reply = self._try_call(
            ref,
            self.TEXT,
            "GetText",
            parameters=self._variant("(ii)", (0, min(count, MAX_FIELD_CHARS))),
            reply_type="(s)",
            deadline=deadline,
        )
        return None if reply is None else _safe_text(reply[0])

    def _read_node(
        self,
        ref: GioAccessibleRef,
        parent_index: int | None,
        application: Mapping[str, Any],
        *,
        deadline: float,
    ) -> tuple[BackendNode, int | None]:
        properties = self._get_all(ref, deadline=deadline)
        role = self._role(ref, deadline=deadline)
        interfaces = self._interfaces(ref, deadline=deadline)
        states = self._state_values(ref, deadline=deadline)
        protected = _is_protected_role(role)
        states["protected"] = True if protected else None
        attributes_raw = properties.get("Attributes")
        attributes = (
            {str(key): str(value) for key, value in attributes_raw.items()}
            if isinstance(attributes_raw, Mapping)
            else {}
        )
        child_count = properties.get("ChildCount")
        if isinstance(child_count, bool) or not isinstance(child_count, int) or child_count < 0:
            child_count = None
        return (
            BackendNode(
                native=ref,
                parent_index=parent_index,
                role=role,
                name=_safe_text(properties.get("Name")),
                description=_safe_text(properties.get("Description")),
                value=self._text_value(
                    ref,
                    interfaces,
                    protected=protected or role == "unknown",
                    deadline=deadline,
                ),
                attributes=attributes,
                states=states,
                bounds=self._bounds(ref, interfaces, deadline=deadline),
                # Gio fallback 刻意只读；不宣称尚未资格验证的写动作。
                actions=(),
                provenance={
                    "bus_name": ref.bus_name,
                    "object_path": ref.object_path,
                    "accessible_id": _safe_text(properties.get("AccessibleId")),
                    "application_name": application.get("name"),
                    "toolkit_name": application.get("toolkit_name"),
                    "process_id": application.get("process_id"),
                    "value_redacted": protected,
                    "coordinate_space": "screen",
                    "atspi_interfaces": sorted(interfaces),
                },
            ),
            child_count,
        )

    def capture(
        self,
        application: Mapping[str, Any],
        *,
        max_depth: int,
        max_nodes: int,
        deadline: float,
    ) -> BackendSnapshot:
        root, info = self._resolve_application(application, deadline=deadline)
        queue: deque[tuple[GioAccessibleRef, int | None, int]] = deque([(root, None, 0)])
        nodes: list[BackendNode] = []
        truncated = False
        while queue:
            _check_deadline(deadline)
            if len(nodes) >= max_nodes:
                truncated = True
                break
            ref, parent_index, depth = queue.popleft()
            try:
                node, child_count = self._read_node(
                    ref, parent_index, info, deadline=deadline
                )
            except DriverError as exc:
                if exc.code == "DRIVER.TIMEOUT":
                    raise
                # 节点消失后无法证明该子树完整。
                truncated = True
                continue
            current_index = len(nodes)
            nodes.append(node)
            if depth >= max_depth:
                if child_count is None or child_count > 0:
                    truncated = True
                continue
            try:
                children = self._children(ref, deadline=deadline)
            except DriverError as exc:
                if exc.code in {"DRIVER.TIMEOUT", "DRIVER.OUTPUT_TOO_LARGE"}:
                    raise
                truncated = True
                continue
            if child_count is None or child_count != len(children):
                truncated = True
            for child in children:
                if len(nodes) + len(queue) >= max_nodes:
                    truncated = True
                    break
                queue.append((child, current_index, depth + 1))
        return BackendSnapshot(application=info, nodes=nodes, truncated=truncated)

    @staticmethod
    def _unsupported(action: str) -> NoReturn:
        raise DriverError(
            "DRIVER.ACTION_UNSUPPORTED",
            f"Gio AT-SPI fallback 暂不支持 {action} 写动作",
            data={"backend": "gio_atspi", "action": action},
        )

    def focus(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        self._unsupported("focus")

    def invoke(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        self._unsupported("invoke")

    def set_text(self, native: Any, text: str, *, deadline: float) -> Any:
        _check_deadline(deadline)
        self._unsupported("set_text")

    def toggle(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        self._unsupported("toggle")

    def expand(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        self._unsupported("expand")

    def collapse(self, native: Any, *, deadline: float) -> Any:
        _check_deadline(deadline)
        self._unsupported("collapse")

    def same_element(self, previous: Any, current: Any, *, deadline: float) -> bool:
        _check_deadline(deadline)
        return (
            isinstance(previous, GioAccessibleRef)
            and isinstance(current, GioAccessibleRef)
            and bool(previous.bus_name)
            and bool(previous.object_path)
            and previous == current
        )

    def accessible_at_point(
        self, root: Any, x: int, y: int, *, deadline: float
    ) -> Any | None:
        _check_deadline(deadline)
        self._unsupported("pointer_click")


def create_default_backend() -> AtspiBackend:
    """选择唯一后端；缺少条件时仍保留清单协商能力。"""

    if not sys.platform.startswith("linux"):
        return UnavailableBackend(
            "platform", platform=sys.platform, required_platform="linux"
        )
    session = _environment_session_info()
    session_type = str(session.get("session_type") or "").strip().lower()
    desktop_entries = {
        item.upper()
        for item in re.split(r"[:;]", str(session.get("desktop") or ""))
        if item
    }
    if (
        session_type != "x11"
        or not session.get("display")
        or "KDE" not in desktop_entries
    ):
        return UnavailableBackend(
            "unsupported_session",
            required_session_type="x11",
            required_desktop="KDE",
            session=session,
        )
    failures: list[dict[str, Any]] = []
    for backend_type in (PyGObjectAtspiBackend, GioAtspiBackend):
        try:
            return backend_type()
        except DriverError as exc:
            failures.append(
                {
                    "backend": backend_type.__name__,
                    "code": exc.code,
                    "message": exc.message,
                    "data": _json_safe(exc.data),
                }
            )
        except Exception as exc:
            failures.append(
                {
                    "backend": backend_type.__name__,
                    "exception_type": type(exc).__name__,
                }
            )
    return UnavailableBackend("initialization", attempts=failures)


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
        json.dumps(message, ensure_ascii=True, allow_nan=False, separators=(",", ":")) + "\n"
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


def handle_request(request: Any, driver: LinuxAtspiDriver) -> None:
    request_id: Any = request.get("id") if isinstance(request, dict) else None
    invocation: WorkerArtifactInvocation | None = None
    try:
        if not isinstance(request, dict):
            raise DriverError("PROTOCOL.INVALID_REQUEST", "请求必须是 JSON 对象")
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
        action = ACTION_NAMES[action_id]
        if action == "capture_target":
            try:
                invocation = WorkerArtifactInvocation.from_request(
                    request,
                    input_slots=(),
                    output_slots=("frame",),
                    expected_action=ACTION_IDS["capture_target"],
                )
                invocation.finish_inputs()
                values = (
                    {}
                    if request.get("args") is None
                    else _object(request.get("args"), "args")
                )
                result = driver._capture_target(values, invocation, deadline)
            except ArtifactIPCError as exc:
                raise DriverError(
                    "DRIVER.ARTIFACT_IPC",
                    "截图 artifact 传输失败",
                    data={"stage": exc.code},
                ) from exc
        else:
            result = driver.execute(action, request.get("args"), deadline=deadline)
        emit({"id": request_id, "result": result})
    except DriverError as exc:
        if invocation is not None and not invocation.completed:
            try:
                invocation.complete_error(exc.code, exc.message)
            except ArtifactIPCError:
                pass
        debug(f"request failed code={exc.code}")
        emit_error(request_id, exc)
    except Exception as exc:
        if invocation is not None and not invocation.completed:
            try:
                invocation.complete_error(
                    "DRIVER.ACTION_FAILED", "Linux AT-SPI 驱动遇到内部错误"
                )
            except ArtifactIPCError:
                pass
        debug(f"internal error type={type(exc).__name__}")
        emit_error(
            request_id,
            DriverError(
                "DRIVER.ACTION_FAILED",
                "Linux AT-SPI 驱动遇到内部错误",
                data={"exception_type": type(exc).__name__},
            ),
        )
    finally:
        if invocation is not None:
            invocation.close()


def _discard_until_newline(stream: Any) -> None:
    while True:
        chunk = stream.readline(MAX_REQUEST_BYTES + 1)
        if not chunk or chunk.endswith(b"\n"):
            return


def serve(driver: LinuxAtspiDriver, stream: Any = None) -> None:
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
    driver = LinuxAtspiDriver()
    if "--manifest" in sys.argv[1:]:
        emit({"type": "manifest", "manifest": MANIFEST})
    debug(f"started pid={os.getpid()} platform={sys.platform} backend={_backend_name(driver.backend)}")
    serve(driver)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
