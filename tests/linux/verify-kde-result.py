#!/usr/bin/env python3
"""Safely verify a KDE/X11 qualification JSON result.

The report is untrusted input.  It is read as one bounded regular file without
following symbolic links, parsed with duplicate-key and non-finite-number
rejection, and then checked against the qualifier's fail-closed safety
contract.  The verifier never launches or attaches to a desktop application.
"""

from __future__ import annotations

import ast
from collections import Counter
from datetime import datetime
import json
import math
import os
from pathlib import Path
import stat
import sys
from typing import Any, NoReturn


VERIFIER_SCHEMA_VERSION = "ai-auto-desktop.kde-x11-result-verifier/v1"
REPORT_SCHEMA_VERSION = "ai-auto-desktop.kde-x11-qualification/v1"
MAX_REPORT_BYTES = 1024 * 1024
MAX_SNAPSHOT_DEPTH = 128
MAX_SNAPSHOT_NODES = 5000
MAX_ENCODED_SNAPSHOT_BYTES = 64 * 1024 * 1024
STATE_NAMES = (
    "enabled", "visible", "showing", "focusable", "focused",
    "editable", "sensitive", "protected", "checked",
    "expandable", "expanded", "selectable", "selected",
)
QUALIFIER_PATH = Path(__file__).with_name("kde_app_qualifier.py")
SNAPSHOT_KEYS = frozenset({
    "selector", "max_depth", "max_nodes", "truncated",
    "encoded_bytes", "completeness", "content_retention",
})
COMPLETENESS_KEYS = frozenset({
    "element_count", "role", "name", "value", "description",
    "state", "semantic_actions",
})
COMPLETED_REPORT_KEYS = frozenset({
    "schema_version", "generated_at", "status", "environment", "host",
    "safety", "limits", "summary", "applications",
})
TERMINAL_REPORT_KEYS = frozenset({
    "schema_version", "generated_at", "status", "errors", "applications",
})
ENVIRONMENT_KEYS = frozenset({
    "session_type", "desktop", "display", "private_session_bus",
    "private_home_and_xdg_dirs", "inherited_at_spi_bus_address",
})
SAFETY_KEYS = frozenset({
    "existing_windows_selected", "application_selector",
    "write_actions_enabled", "write_actions_dispatched",
    "screenshots_or_ocr", "node_ui_text_retained",
})
LIMIT_KEYS = frozenset({
    "registration_timeout_seconds", "snapshot_timeout_seconds",
    "max_depth", "max_nodes",
})
SUMMARY_KEYS = frozenset({
    "total", "supported", "unsupported", "error", "duration_ms",
})
APPLICATION_KEYS = frozenset({
    "application", "status", "support_level", "executable", "version",
    "launch_pid", "pid_selection", "registration_latency_ms",
    "snapshot_latency_ms", "snapshot", "errors", "writes_dispatched",
    "driver_version", "backend", "private_registry_baseline_count",
    "launch_args", "atspi_application", "cleanup", "stderr_tail",
})
AT_SPI_APPLICATION_KEYS = frozenset({
    "bus_name", "object_path", "name", "process_id", "toolkit_name",
    "toolkit_version", "atspi_version", "locale",
})
EXACT_SELECTOR_DESCRIPTION = (
    "exact owned Popen PID plus available bus/toolkit identity"
)


class VerificationError(Exception):
    """A stable, user-facing rejection of the input report."""

    def __init__(
        self, code: str, message: str, details: dict[str, Any] | None = None
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details or {}


class _DuplicateKeyError(ValueError):
    pass


def _fail(
    code: str, message: str, details: dict[str, Any] | None = None
) -> NoReturn:
    raise VerificationError(code, message, details)


def _emit(payload: dict[str, Any]) -> None:
    print(
        json.dumps(
            payload, ensure_ascii=False, sort_keys=True, separators=(",", ":")
        ),
        flush=True,
    )


def _read_regular_file(path: Path) -> bytes:
    try:
        path_metadata = path.lstat()
    except OSError as exc:
        _fail(
            "input_unreadable", "无法读取 KDE 资格报告元数据。",
            {"reason": exc.__class__.__name__},
        )
    if stat.S_ISLNK(path_metadata.st_mode):
        _fail("input_symlink", "KDE 资格报告路径不能是符号链接。")
    if not stat.S_ISREG(path_metadata.st_mode):
        _fail("input_not_regular", "KDE 资格报告必须是普通文件。")
    if path_metadata.st_size == 0:
        _fail("input_empty", "KDE 资格报告为空。")
    if path_metadata.st_size > MAX_REPORT_BYTES:
        _fail(
            "input_too_large", "KDE 资格报告超过硬上限。",
            {"limit_bytes": MAX_REPORT_BYTES},
        )

    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    flags |= getattr(os, "O_NONBLOCK", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as exc:
        _fail(
            "input_unreadable", "无法安全打开 KDE 资格报告。",
            {"reason": exc.__class__.__name__},
        )
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            _fail("input_not_regular", "KDE 资格报告必须是普通文件。")
        if (metadata.st_dev, metadata.st_ino) != (
            path_metadata.st_dev, path_metadata.st_ino
        ):
            _fail("input_changed", "KDE 资格报告在打开期间发生替换。")
        chunks: list[bytes] = []
        remaining = MAX_REPORT_BYTES + 1
        while remaining:
            chunk = os.read(descriptor, min(64 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        payload = b"".join(chunks)
        if len(payload) > MAX_REPORT_BYTES or os.read(descriptor, 1):
            _fail(
                "input_too_large", "KDE 资格报告超过硬上限。",
                {"limit_bytes": MAX_REPORT_BYTES},
            )
        final_metadata = os.fstat(descriptor)
        if (
            (final_metadata.st_dev, final_metadata.st_ino)
            != (metadata.st_dev, metadata.st_ino)
            or final_metadata.st_size != len(payload)
            or final_metadata.st_mtime_ns != metadata.st_mtime_ns
        ):
            _fail("input_changed", "KDE 资格报告在读取期间发生变化。")
        return payload
    finally:
        os.close(descriptor)


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise _DuplicateKeyError(key)
        result[key] = value
    return result


def _reject_constant(value: str) -> NoReturn:
    raise ValueError(f"non-finite JSON number: {value}")


def _parse_report(payload: bytes) -> dict[str, Any]:
    try:
        text = payload.decode("utf-8", errors="strict")
    except UnicodeDecodeError:
        _fail("invalid_utf8", "KDE 资格报告必须是 UTF-8。")
    try:
        document = json.loads(
            text, object_pairs_hook=_unique_object, parse_constant=_reject_constant
        )
    except _DuplicateKeyError as exc:
        _fail(
            "duplicate_json_key", "KDE 资格报告包含重复 JSON key。",
            {"key": str(exc)},
        )
    except (json.JSONDecodeError, ValueError, RecursionError):
        _fail("invalid_json", "KDE 资格报告不是有效的有界 JSON。")
    if type(document) is not dict:
        _fail("invalid_report_schema", "KDE 资格报告顶层必须是对象。")
    return document


def _expected_applications() -> tuple[str, ...]:
    """Read only the literal APP_SPECS keys from the trusted qualifier."""

    try:
        source = QUALIFIER_PATH.read_text(encoding="utf-8")
        module = ast.parse(source, filename=str(QUALIFIER_PATH))
    except (OSError, UnicodeError, SyntaxError) as exc:
        _fail(
            "verifier_contract_unavailable",
            "无法读取本版本 qualifier 的必选应用集合。",
            {"reason": exc.__class__.__name__},
        )
    for statement in module.body:
        value: ast.expr | None = None
        if (
            isinstance(statement, ast.AnnAssign)
            and isinstance(statement.target, ast.Name)
            and statement.target.id == "APP_SPECS"
        ):
            value = statement.value
        elif isinstance(statement, ast.Assign) and any(
            isinstance(target, ast.Name) and target.id == "APP_SPECS"
            for target in statement.targets
        ):
            value = statement.value
        if not isinstance(value, ast.Dict):
            continue
        names: list[str] = []
        for key in value.keys:
            try:
                name = ast.literal_eval(key)
            except (ValueError, TypeError):
                _fail(
                    "verifier_contract_unavailable",
                    "APP_SPECS 包含非字面量应用名。",
                )
            if not isinstance(name, str) or not name or name in names:
                _fail(
                    "verifier_contract_unavailable",
                    "APP_SPECS 的应用名无效或重复。",
                )
            names.append(name)
        if names:
            return tuple(names)
    _fail(
        "verifier_contract_unavailable",
        "未找到本版本 qualifier 的 APP_SPECS。",
    )


def _schema_error(path: str, expected: str) -> NoReturn:
    _fail(
        "invalid_report_schema", f"字段 {path} 必须是 {expected}。",
        {"path": path, "expected": expected},
    )


def _exact_keys(value: dict[str, Any], expected: frozenset[str], path: str) -> None:
    if set(value) != expected:
        _fail(
            "invalid_report_schema", f"字段 {path} 的成员集合不正确。",
            {
                "path": path,
                "missing": sorted(expected - set(value)),
                "unexpected": sorted(set(value) - expected),
            },
        )


def _object(value: Any, path: str) -> dict[str, Any]:
    if type(value) is not dict:
        _schema_error(path, "object")
    return value


def _array(value: Any, path: str) -> list[Any]:
    if type(value) is not list:
        _schema_error(path, "array")
    return value


def _string(value: Any, path: str, *, nonempty: bool = True) -> str:
    if type(value) is not str or (nonempty and not value):
        _schema_error(path, "non-empty string" if nonempty else "string")
    return value


def _integer(value: Any, path: str, *, minimum: int = 0) -> int:
    if type(value) is not int or value < minimum:
        _schema_error(path, f"integer >= {minimum}")
    return value


def _number(value: Any, path: str, *, minimum: float = 0.0) -> float:
    if type(value) not in (int, float) or not math.isfinite(value) or value < minimum:
        _schema_error(path, f"finite number >= {minimum}")
    return float(value)


def _boolean(value: Any, path: str) -> bool:
    if type(value) is not bool:
        _schema_error(path, "boolean")
    return value


def _timestamp(value: Any) -> str:
    text = _string(value, "generated_at")
    try:
        parsed = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError:
        _schema_error("generated_at", "RFC 3339 timestamp")
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        _schema_error("generated_at", "timezone-aware RFC 3339 timestamp")
    return text


def _failure(
    failures: list[dict[str, Any]], code: str, message: str,
    *, application: str | None = None, details: dict[str, Any] | None = None,
) -> None:
    item: dict[str, Any] = {"code": code, "message": message}
    if application is not None:
        item["application"] = application
    if details:
        item["details"] = details
    failures.append(item)


def _require_exact_safety(
    owner: dict[str, Any], key: str, expected: Any, path: str,
    failures: list[dict[str, Any]], code: str,
) -> None:
    actual = owner.get(key)
    if type(actual) is not type(expected):
        _schema_error(f"{path}.{key}", type(expected).__name__)
    if actual != expected:
        _failure(
            failures, code, f"{path}.{key} 不满足只读安全约束。",
            details={"expected": expected, "actual": actual},
        )


def _validate_count_map(value: Any, path: str) -> dict[str, int]:
    mapping = _object(value, path)
    for key, count in mapping.items():
        _string(key, f"{path}.<key>")
        _integer(count, f"{path}.{key}")
    return mapping


def _expected_ratio(numerator: int, denominator: int) -> float | None:
    return None if denominator == 0 else round(numerator / denominator, 4)


def _validate_ratio(value: Any, expected: float | None, path: str) -> None:
    if expected is None:
        if value is not None:
            _schema_error(path, "null")
    elif type(value) not in (int, float) or not math.isfinite(value):
        _schema_error(path, "finite ratio")
    elif float(value) != expected:
        _fail(
            "summary_inconsistent", f"字段 {path} 与计数不一致。",
            {"path": path, "expected": expected, "actual": value},
        )


def _validate_field_completeness(
    value: Any, path: str, element_count: int
) -> None:
    field = _object(value, path)
    if set(field) != {
        "non_null", "non_empty", "non_null_ratio", "non_empty_ratio"
    }:
        _fail(
            "unsafe_content_retention",
            f"字段 {path} 不是仅含计数的聚合结构。",
            {"path": path},
        )
    non_null = _integer(field.get("non_null"), f"{path}.non_null")
    non_empty = _integer(field.get("non_empty"), f"{path}.non_empty")
    if non_empty > non_null or non_null > element_count:
        _fail("summary_inconsistent", f"字段 {path} 的聚合计数不一致。")
    _validate_ratio(
        field.get("non_null_ratio"),
        _expected_ratio(non_null, element_count),
        f"{path}.non_null_ratio",
    )
    _validate_ratio(
        field.get("non_empty_ratio"),
        _expected_ratio(non_empty, element_count),
        f"{path}.non_empty_ratio",
    )


def _validate_completeness(
    value: Any, path: str, max_nodes: int
) -> None:
    completeness = _object(value, path)
    if set(completeness) != COMPLETENESS_KEYS:
        _fail(
            "unsafe_content_retention",
            f"字段 {path} 必须只保留规定的聚合项。",
            {"unexpected_keys": sorted(set(completeness) - COMPLETENESS_KEYS)},
        )
    element_count = _integer(
        completeness.get("element_count"), f"{path}.element_count", minimum=1
    )
    if element_count > max_nodes:
        _fail("summary_inconsistent", f"字段 {path}.element_count 超过 max_nodes。")

    role = _object(completeness.get("role"), f"{path}.role")
    if set(role) != {"non_empty", "non_empty_ratio", "counts"}:
        _fail("unsafe_content_retention", f"字段 {path}.role 不是聚合结构。")
    role_count = _integer(role.get("non_empty"), f"{path}.role.non_empty")
    role_counts = _validate_count_map(role.get("counts"), f"{path}.role.counts")
    if sum(role_counts.values()) != role_count or role_count > element_count:
        _fail("summary_inconsistent", f"字段 {path}.role 的计数不一致。")
    _validate_ratio(
        role.get("non_empty_ratio"),
        _expected_ratio(role_count, element_count),
        f"{path}.role.non_empty_ratio",
    )

    for field_name in ("name", "value", "description"):
        _validate_field_completeness(
            completeness.get(field_name), f"{path}.{field_name}", element_count
        )

    state = _object(completeness.get("state"), f"{path}.state")
    if set(state) != {"known", "possible", "known_ratio", "known_by_state"}:
        _fail("unsafe_content_retention", f"字段 {path}.state 不是聚合结构。")
    known = _integer(state.get("known"), f"{path}.state.known")
    possible = _integer(state.get("possible"), f"{path}.state.possible")
    known_by_state = _validate_count_map(
        state.get("known_by_state"), f"{path}.state.known_by_state"
    )
    if set(known_by_state) != set(STATE_NAMES):
        _fail("summary_inconsistent", f"字段 {path}.state 缺少规定状态计数。")
    if (
        any(count > element_count for count in known_by_state.values())
        or sum(known_by_state.values()) != known
        or possible != element_count * len(STATE_NAMES)
        or known > possible
    ):
        _fail("summary_inconsistent", f"字段 {path}.state 的计数不一致。")
    _validate_ratio(
        state.get("known_ratio"), _expected_ratio(known, possible),
        f"{path}.state.known_ratio",
    )

    actions = _object(
        completeness.get("semantic_actions"), f"{path}.semantic_actions"
    )
    if set(actions) != {"node_action_counts", "native_action_name_counts"}:
        _fail(
            "unsafe_content_retention",
            f"字段 {path}.semantic_actions 不是聚合结构。",
        )
    _validate_count_map(
        actions.get("node_action_counts"), f"{path}.semantic_actions.node_action_counts"
    )
    _validate_count_map(
        actions.get("native_action_name_counts"),
        f"{path}.semantic_actions.native_action_name_counts",
    )


def _validate_snapshot(
    snapshot_value: Any, application: dict[str, Any], name: str,
    limits: dict[str, int], failures: list[dict[str, Any]],
) -> None:
    path = f"applications[{name}].snapshot"
    snapshot = _object(snapshot_value, path)
    if set(snapshot) != SNAPSHOT_KEYS:
        _fail(
            "unsafe_content_retention",
            "snapshot 的聚合字段集合不符合规定。",
            {
                "application": name,
                "missing_keys": sorted(SNAPSHOT_KEYS - set(snapshot)),
                "unexpected_keys": sorted(set(snapshot) - SNAPSHOT_KEYS),
            },
        )
    max_depth = _integer(snapshot.get("max_depth"), f"{path}.max_depth")
    max_nodes = _integer(snapshot.get("max_nodes"), f"{path}.max_nodes", minimum=1)
    if (
        max_depth > MAX_SNAPSHOT_DEPTH
        or max_nodes > MAX_SNAPSHOT_NODES
        or max_depth != limits["max_depth"]
        or max_nodes != limits["max_nodes"]
    ):
        _failure(
            failures, "snapshot_bounds_invalid",
            "snapshot bounds 超限或与顶层 limits 不一致。", application=name,
        )
    if _boolean(snapshot.get("truncated"), f"{path}.truncated"):
        _failure(
            failures, "snapshot_truncated", "snapshot 已截断。",
            application=name,
        )
    encoded_bytes = _integer(
        snapshot.get("encoded_bytes"), f"{path}.encoded_bytes", minimum=1
    )
    if encoded_bytes > MAX_ENCODED_SNAPSHOT_BYTES:
        _failure(
            failures, "snapshot_bounds_invalid",
            "编码后的 snapshot 超过 verifier 上限。", application=name,
        )
    if snapshot.get("content_retention") != "aggregate_only_no_ui_text":
        _failure(
            failures, "unsafe_content_retention",
            "snapshot 未声明仅保留无 UI 文本的聚合数据。", application=name,
        )
    _validate_completeness(snapshot.get("completeness"), f"{path}.completeness", max_nodes)

    launch_pid = _integer(
        application.get("launch_pid"), f"applications[{name}].launch_pid", minimum=1
    )
    if application.get("pid_selection") != "exact_popen_pid":
        _failure(
            failures, "inexact_pid_selector",
            "应用未声明 exact_popen_pid。", application=name,
        )
    selector = _object(snapshot.get("selector"), f"{path}.selector")
    if not set(selector).issubset({"process_id", "bus_name", "toolkit_name"}):
        _failure(
            failures, "inexact_pid_selector",
            "snapshot selector 包含非精确身份字段。", application=name,
        )
    selector_pid = _integer(
        selector.get("process_id"), f"{path}.selector.process_id", minimum=1
    )
    atspi = _object(
        application.get("atspi_application"), f"applications[{name}].atspi_application"
    )
    _exact_keys(atspi, AT_SPI_APPLICATION_KEYS, f"applications[{name}].atspi_application")
    atspi_pid = _integer(
        atspi.get("process_id"),
        f"applications[{name}].atspi_application.process_id", minimum=1,
    )
    if selector_pid != launch_pid or atspi_pid != launch_pid:
        _failure(
            failures, "inexact_pid_selector",
            "launch、AT-SPI 和 snapshot selector PID 不完全一致。",
            application=name,
        )
    for identity_key in ("bus_name", "toolkit_name"):
        observed = atspi.get(identity_key)
        if observed is not None and type(observed) is not str:
            _schema_error(f"applications[{name}].atspi_application.{identity_key}", "string or null")
        if isinstance(observed, str) and observed:
            if selector.get(identity_key) != observed:
                _failure(
                    failures, "inexact_pid_selector",
                    f"snapshot selector 未保留已观测 {identity_key}。",
                    application=name,
                )
        elif identity_key in selector:
            _failure(
                failures, "inexact_pid_selector",
                f"snapshot selector 的 {identity_key} 没有观测依据。",
                application=name,
            )


def _validate_error_list(value: Any, path: str) -> list[Any]:
    errors = _array(value, path)
    for index, item in enumerate(errors):
        error = _object(item, f"{path}[{index}]")
        _string(error.get("stage"), f"{path}[{index}].stage")
        _string(error.get("code"), f"{path}[{index}].code")
    return errors


def _validate_terminal_report(report: dict[str, Any]) -> dict[str, Any]:
    _exact_keys(report, TERMINAL_REPORT_KEYS, "$")
    status = report.get("status")
    if status not in ("unsupported", "error"):
        _schema_error("status", "completed, unsupported, or error")
    applications = _array(report.get("applications"), "applications")
    if applications:
        _fail("invalid_report_schema", "顶层终止报告不得声称应用结果。")
    _validate_error_list(report.get("errors"), "errors")
    failure = {
        "code": f"report_{status}",
        "message": f"qualifier 顶层状态为 {status}。",
    }
    return {
        "schema_version": VERIFIER_SCHEMA_VERSION,
        "status": "failed",
        "report_valid": True,
        "qualified": False,
        "expected_applications": list(_expected_applications()),
        "observed_applications": [],
        "qualification_failures": [failure],
        "error": failure,
    }


def _validate_completed_report(report: dict[str, Any]) -> dict[str, Any]:
    failures: list[dict[str, Any]] = []
    expected = _expected_applications()
    _exact_keys(report, COMPLETED_REPORT_KEYS, "$")
    _object(report.get("host"), "host")

    environment = _object(report.get("environment"), "environment")
    _exact_keys(environment, ENVIRONMENT_KEYS, "environment")
    if environment.get("session_type") != "x11":
        _failure(failures, "unsafe_environment", "session_type 必须为 x11。")
    desktop = _string(environment.get("desktop"), "environment.desktop")
    if "KDE" not in {part.upper() for part in desktop.replace(";", ":").split(":")}:
        _failure(failures, "unsafe_environment", "desktop 必须包含 KDE。")
    _string(environment.get("display"), "environment.display")
    _require_exact_safety(
        environment, "private_session_bus", True, "environment", failures,
        "unsafe_environment",
    )
    _require_exact_safety(
        environment, "private_home_and_xdg_dirs", True, "environment", failures,
        "unsafe_environment",
    )
    _require_exact_safety(
        environment, "inherited_at_spi_bus_address", False, "environment", failures,
        "unsafe_environment",
    )

    safety = _object(report.get("safety"), "safety")
    _exact_keys(safety, SAFETY_KEYS, "safety")
    for key, expected_value, code in (
        ("existing_windows_selected", False, "existing_windows_selected"),
        ("application_selector", EXACT_SELECTOR_DESCRIPTION, "inexact_pid_selector"),
        ("write_actions_enabled", False, "write_actions_enabled"),
        ("screenshots_or_ocr", False, "screen_content_collected"),
        ("node_ui_text_retained", False, "unsafe_content_retention"),
    ):
        _require_exact_safety(
            safety, key, expected_value, "safety", failures, code
        )
    top_write_count = _integer(
        safety.get("write_actions_dispatched"), "safety.write_actions_dispatched"
    )

    limits_object = _object(report.get("limits"), "limits")
    _exact_keys(limits_object, LIMIT_KEYS, "limits")
    registration_timeout = _number(
        limits_object.get("registration_timeout_seconds"),
        "limits.registration_timeout_seconds", minimum=0.000001,
    )
    snapshot_timeout = _number(
        limits_object.get("snapshot_timeout_seconds"),
        "limits.snapshot_timeout_seconds", minimum=0.000001,
    )
    max_depth = _integer(limits_object.get("max_depth"), "limits.max_depth")
    max_nodes = _integer(
        limits_object.get("max_nodes"), "limits.max_nodes", minimum=1
    )
    if (
        registration_timeout > 15
        or snapshot_timeout > 30
        or max_depth > MAX_SNAPSHOT_DEPTH
        or max_nodes > MAX_SNAPSHOT_NODES
    ):
        _failure(failures, "snapshot_bounds_invalid", "顶层执行或 snapshot bounds 超限。")
    limits = {"max_depth": max_depth, "max_nodes": max_nodes}

    applications = _array(report.get("applications"), "applications")
    names: list[str] = []
    statuses: Counter[str] = Counter()
    total_writes = 0
    for index, item in enumerate(applications):
        application = _object(item, f"applications[{index}]")
        unexpected_application_keys = set(application) - APPLICATION_KEYS
        if unexpected_application_keys:
            _fail(
                "unsafe_content_retention",
                "应用结果包含 qualifier 合约之外的字段。",
                {
                    "application_index": index,
                    "unexpected_keys": sorted(unexpected_application_keys),
                },
            )
        name = _string(application.get("application"), f"applications[{index}].application")
        if name in names:
            _fail(
                "duplicate_application", "资格报告包含重复应用。",
                {"application": name},
            )
        names.append(name)
        status = application.get("status")
        if status not in ("supported", "unsupported", "error"):
            _schema_error(f"applications[{index}].status", "supported, unsupported, or error")
        statuses[status] += 1
        if "version" not in application or (
            application["version"] is not None
            and type(application["version"]) is not dict
        ):
            _schema_error(f"applications[{name}].version", "object or null")
        launch_args = _array(
            application.get("launch_args"), f"applications[{name}].launch_args"
        ) if status == "supported" or "launch_args" in application else []
        for argument_index, argument in enumerate(launch_args):
            _string(
                argument,
                f"applications[{name}].launch_args[{argument_index}]",
                nonempty=False,
            )
        stderr_tail = application.get("stderr_tail")
        if stderr_tail is not None:
            _string(stderr_tail, f"applications[{name}].stderr_tail", nonempty=False)
        errors = _validate_error_list(
            application.get("errors"), f"applications[{name}].errors"
        )
        writes = _array(
            application.get("writes_dispatched"),
            f"applications[{name}].writes_dispatched",
        )
        total_writes += len(writes)
        if writes:
            _failure(
                failures, "write_actions_dispatched",
                "应用报告包含写动作派发。", application=name,
            )

        launch_pid = application.get("launch_pid")
        if launch_pid is not None:
            _integer(launch_pid, f"applications[{name}].launch_pid", minimum=1)
            cleanup = application.get("cleanup")
            if type(cleanup) is not dict:
                _failure(
                    failures, "cleanup_not_proven",
                    "已启动应用缺少 cleanup 证明。", application=name,
                )
            else:
                stopped = cleanup.get("owned_process_group_stopped")
                if type(stopped) is not bool:
                    _schema_error(
                        f"applications[{name}].cleanup.owned_process_group_stopped",
                        "boolean",
                    )
                if not stopped:
                    _failure(
                        failures, "cleanup_failed",
                        "自有应用进程组未确认停止。", application=name,
                    )
                returncode = cleanup.get("returncode")
                if type(returncode) is not int:
                    _failure(
                        failures, "cleanup_not_proven",
                        "cleanup 缺少最终 returncode。", application=name,
                    )

        support_level = application.get("support_level")
        if status == "supported":
            if support_level != "observed_read_only":
                _failure(
                    failures, "invalid_support_level",
                    "supported 应用必须是 observed_read_only。", application=name,
                )
            if errors:
                _failure(
                    failures, "supported_with_errors",
                    "supported 应用不能同时包含错误。", application=name,
                )
            _string(application.get("executable"), f"applications[{name}].executable")
            _string(application.get("driver_version"), f"applications[{name}].driver_version")
            _string(application.get("backend"), f"applications[{name}].backend")
            _integer(
                application.get("private_registry_baseline_count"),
                f"applications[{name}].private_registry_baseline_count",
            )
            _number(
                application.get("registration_latency_ms"),
                f"applications[{name}].registration_latency_ms",
            )
            _number(
                application.get("snapshot_latency_ms"),
                f"applications[{name}].snapshot_latency_ms",
            )
            _validate_snapshot(application.get("snapshot"), application, name, limits, failures)
        else:
            if support_level != "none":
                _failure(
                    failures, "invalid_support_level",
                    "非 supported 应用的 support_level 必须为 none。",
                    application=name,
                )
            if not errors:
                _failure(
                    failures, "missing_application_error",
                    "非 supported 应用必须记录错误。", application=name,
                )
            _failure(
                failures, f"application_{status}",
                f"必选应用状态为 {status}。", application=name,
            )

    if set(names) != set(expected) or len(names) != len(expected):
        _failure(
            failures, "application_set_mismatch",
            "报告应用集合与当前 APP_SPECS 不一致。",
            details={
                "expected": list(expected), "observed": names,
                "missing": sorted(set(expected) - set(names)),
                "unexpected": sorted(set(names) - set(expected)),
            },
        )

    summary = _object(report.get("summary"), "summary")
    _exact_keys(summary, SUMMARY_KEYS, "summary")
    expected_summary = {
        "total": len(applications),
        "supported": statuses["supported"],
        "unsupported": statuses["unsupported"],
        "error": statuses["error"],
    }
    for key, expected_value in expected_summary.items():
        actual = _integer(summary.get(key), f"summary.{key}")
        if actual != expected_value:
            _fail(
                "summary_inconsistent",
                f"summary.{key} 与 applications 不一致。",
                {"expected": expected_value, "actual": actual},
            )
    _number(summary.get("duration_ms"), "summary.duration_ms")
    if top_write_count != total_writes:
        _fail(
            "summary_inconsistent",
            "safety.write_actions_dispatched 与应用记录不一致。",
            {"expected": total_writes, "actual": top_write_count},
        )
    if top_write_count:
        _failure(failures, "write_actions_dispatched", "报告包含写动作派发。")

    qualified = not failures
    result: dict[str, Any] = {
        "schema_version": VERIFIER_SCHEMA_VERSION,
        "status": "passed" if qualified else "failed",
        "report_valid": True,
        "qualified": qualified,
        "expected_applications": list(expected),
        "observed_applications": names,
        "summary": expected_summary,
        "qualification_failures": failures,
    }
    if failures:
        result["error"] = failures[0]
    return result


def verify(path: Path) -> dict[str, Any]:
    report = _parse_report(_read_regular_file(path))
    if report.get("schema_version") != REPORT_SCHEMA_VERSION:
        _fail(
            "unsupported_report_schema",
            "KDE 资格报告 schema_version 不受支持。",
            {"expected": REPORT_SCHEMA_VERSION, "actual": report.get("schema_version")},
        )
    _timestamp(report.get("generated_at"))
    status = report.get("status")
    if status in ("unsupported", "error"):
        return _validate_terminal_report(report)
    if status != "completed":
        _schema_error("status", "completed, unsupported, or error")
    return _validate_completed_report(report)


def _failed_result(error: VerificationError) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "schema_version": VERIFIER_SCHEMA_VERSION,
        "status": "failed",
        "report_valid": False,
        "qualified": False,
        "error": {"code": error.code, "message": error.message},
    }
    if error.details:
        payload["error"]["details"] = error.details
    return payload


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    if len(arguments) != 1 or arguments[0] in ("-h", "--help"):
        _emit(_failed_result(VerificationError(
            "usage", "用法：verify-kde-result.sh /path/to/kde-x11-qualification.json"
        )))
        return 64
    try:
        result = verify(Path(arguments[0]))
    except VerificationError as exc:
        _emit(_failed_result(exc))
        return 1
    except Exception:
        _emit(_failed_result(VerificationError(
            "internal_error", "KDE 结果验真器发生未预期错误。"
        )))
        return 1
    _emit(result)
    return 0 if result["qualified"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
