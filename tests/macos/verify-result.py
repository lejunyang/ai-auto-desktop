#!/usr/bin/env python3
"""Safely verify a returned macOS AX fixture result archive.

The verifier deliberately does not extract the archive.  Both the compressed
input and the decompressed ustar payload are read with hard size limits before
the four expected regular files are validated in memory.
"""

from __future__ import annotations

import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import stat
import struct
import sys
from typing import Any, NoReturn
import zlib


VERIFIER_SCHEMA = "ai-auto-desktop.macos-result-verifier/v1"
REPORT_SCHEMA_VERSION = "1.0"
REPORT_KIND = "macos_ax_fixture_test"
RUNNER_BUNDLE_ID = "dev.ai-auto-desktop.testkit.ax-runner"
FIXTURE_BUNDLE_ID = "dev.ai-auto-desktop.testkit.fixture"
SUPPORTED_ARCHITECTURES = frozenset({"arm64", "x86_64"})
IDENTITY_STABILITIES = frozenset({"ephemeral", "stable_identity_requested"})
EXPECTED_MEMBERS = (
    "report.json",
    "README.txt",
    "identity.txt",
    "SHA256SUMS",
)
HASHED_MEMBERS = EXPECTED_MEMBERS[:3]
MEMBER_LIMITS = {
    "report.json": 512 * 1024,
    "README.txt": 64 * 1024,
    "identity.txt": 256 * 1024,
    "SHA256SUMS": 4 * 1024,
}
MAX_COMPRESSED_BYTES = 4 * 1024 * 1024
MAX_TAR_BYTES = 4 * 1024 * 1024
MAX_TOTAL_MEMBER_BYTES = sum(MEMBER_LIMITS.values())
TAR_BLOCK_SIZE = 512
NORMALIZED_MTIME = 946_684_800
REQUIRED_PASSED_CHECKS = frozenset({
    "screen_capture_preflight",
    "accessibility_trust",
    "bounded_discovery",
    "roles_and_ambiguity",
    "type_text_secure_rejected",
    "focus_and_reread",
    "set_value_and_reread",
    "type_text_unicode_and_reread",
    "press_and_reread",
})
HEX_64 = re.compile(r"^[0-9a-fA-F]{64}$")
LOWER_HEX_64 = re.compile(r"^[0-9a-f]{64}$")
SOURCE_REVISION = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
CDHASH = re.compile(r"^[0-9a-fA-F]+$")
SAFE_MEMBER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
TIMESTAMP_UTC = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$"
)


class VerificationError(Exception):
    """A user-facing verification failure with a stable machine code."""

    def __init__(
        self, code: str, message: str, details: dict[str, Any] | None = None
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details or {}


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


def _read_archive(path: Path) -> tuple[bytes, os.stat_result]:
    try:
        path_metadata = path.lstat()
    except OSError as exc:
        _fail(
            "input_unreadable",
            "无法读取结果归档元数据。",
            {"reason": exc.__class__.__name__},
        )
    if stat.S_ISLNK(path_metadata.st_mode):
        _fail("input_symlink", "结果归档路径不能是符号链接。")
    if not stat.S_ISREG(path_metadata.st_mode):
        _fail("input_not_regular", "结果归档必须是普通文件。")
    if path_metadata.st_size <= 0:
        _fail("input_empty", "结果归档为空。")
    if path_metadata.st_size > MAX_COMPRESSED_BYTES:
        _fail(
            "archive_too_large",
            "压缩归档超过硬上限。",
            {"limit_bytes": MAX_COMPRESSED_BYTES},
        )
    flags = os.O_RDONLY
    flags |= getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    flags |= getattr(os, "O_NONBLOCK", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as exc:
        _fail(
            "input_unreadable",
            "无法安全打开结果归档。",
            {"reason": exc.__class__.__name__},
        )
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            _fail("input_not_regular", "结果归档必须是普通文件。")
        if (metadata.st_dev, metadata.st_ino) != (
            path_metadata.st_dev,
            path_metadata.st_ino,
        ):
            _fail("input_changed", "结果归档在打开期间发生替换。")
        chunks: list[bytes] = []
        remaining = MAX_COMPRESSED_BYTES + 1
        while remaining:
            chunk = os.read(descriptor, min(64 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        compressed = b"".join(chunks)
        if len(compressed) > MAX_COMPRESSED_BYTES or os.read(descriptor, 1):
            _fail(
                "archive_too_large",
                "压缩归档超过硬上限。",
                {"limit_bytes": MAX_COMPRESSED_BYTES},
            )
        final_metadata = os.fstat(descriptor)
        if (
            (final_metadata.st_dev, final_metadata.st_ino)
            != (metadata.st_dev, metadata.st_ino)
            or final_metadata.st_size != len(compressed)
        ):
            _fail("input_changed", "结果归档在读取期间发生变化。")
        return compressed, final_metadata
    finally:
        os.close(descriptor)


def _decompress(compressed: bytes) -> bytes:
    if len(compressed) < 18 or compressed[:3] != b"\x1f\x8b\x08":
        _fail("invalid_gzip", "归档不是 gzip deflate 数据。")
    flags = compressed[3]
    if flags != 0:
        _fail(
            "non_normalized_gzip",
            "gzip header 包含名称、时间或其他可变元数据。",
            {"flags": flags},
        )
    if compressed[4:8] != b"\0\0\0\0":
        _fail("non_normalized_gzip", "gzip mtime 必须为零。")
    if compressed[8] != 0 or compressed[9] != 3:
        _fail(
            "non_normalized_gzip",
            "gzip XFL/OS header 与受支持生成器不匹配。",
            {"xfl": compressed[8], "os": compressed[9]},
        )
    try:
        decompressor = zlib.decompressobj(wbits=-zlib.MAX_WBITS)
        payload = decompressor.decompress(
            compressed[10:-8], MAX_TAR_BYTES + 1
        )
        if decompressor.unconsumed_tail or len(payload) > MAX_TAR_BYTES:
            _fail(
                "tar_payload_too_large",
                "gzip 解压后的 tar 数据超过硬上限。",
                {"limit_bytes": MAX_TAR_BYTES},
            )
        payload += decompressor.flush(MAX_TAR_BYTES + 1 - len(payload))
    except zlib.error as exc:
        _fail(
            "invalid_gzip",
            "归档不是完整有效的 gzip 数据。",
            {"reason": exc.__class__.__name__},
        )
    if not decompressor.eof or decompressor.unused_data:
        _fail(
            "invalid_gzip",
            "归档必须恰好包含一个完整 gzip member。",
        )
    if len(payload) > MAX_TAR_BYTES:
        _fail(
            "tar_payload_too_large",
            "gzip 解压后的 tar 数据超过硬上限。",
            {"limit_bytes": MAX_TAR_BYTES},
        )
    expected_crc32, expected_size = struct.unpack("<II", compressed[-8:])
    if zlib.crc32(payload) & 0xFFFFFFFF != expected_crc32:
        _fail("invalid_gzip", "gzip CRC32 校验失败。")
    if len(payload) & 0xFFFFFFFF != expected_size:
        _fail("invalid_gzip", "gzip ISIZE 校验失败。")
    return payload


def _tar_text(field: bytes, label: str) -> str:
    raw = field.split(b"\0", 1)[0]
    try:
        return raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError:
        _fail("invalid_tar_header", f"tar {label} 不是 UTF-8。")


def _tar_octal(field: bytes, label: str) -> int:
    stripped = field.strip(b" \0")
    if not stripped:
        return 0
    if any(byte < ord("0") or byte > ord("7") for byte in stripped):
        _fail("invalid_tar_header", f"tar {label} 不是规范八进制数。")
    return int(stripped, 8)


def _unsafe_member_name(name: str) -> bool:
    if not name or name.startswith(("/", "\\")):
        return True
    normalized = name.replace("\\", "/")
    parts = normalized.split("/")
    return (
        any(part in ("", ".", "..") for part in parts)
        or bool(re.match(r"^[A-Za-z]:", normalized))
        or not SAFE_MEMBER.fullmatch(name)
    )


def _parse_ustar(payload: bytes) -> dict[str, bytes]:
    if len(payload) < TAR_BLOCK_SIZE * 3 or len(payload) % TAR_BLOCK_SIZE:
        _fail("invalid_tar", "tar 长度或结束块无效。")

    members: dict[str, bytes] = {}
    member_order: list[str] = []
    total_size = 0
    offset = 0
    saw_end = False
    while offset + TAR_BLOCK_SIZE <= len(payload):
        header = payload[offset : offset + TAR_BLOCK_SIZE]
        if header == bytes(TAR_BLOCK_SIZE):
            if payload[offset : offset + TAR_BLOCK_SIZE * 2] != bytes(
                TAR_BLOCK_SIZE * 2
            ):
                _fail("invalid_tar", "tar 缺少两个连续结束块。")
            if any(payload[offset + TAR_BLOCK_SIZE * 2 :]):
                _fail("invalid_tar", "tar 结束块后包含非零数据。")
            saw_end = True
            break

        stored_checksum = _tar_octal(header[148:156], "checksum")
        calculated_checksum = (
            sum(header[:148]) + (ord(" ") * 8) + sum(header[156:])
        )
        if stored_checksum != calculated_checksum:
            _fail("invalid_tar_header", "tar header checksum 不匹配。")
        if header[257:263] != b"ustar\0" or header[263:265] != b"00":
            _fail("invalid_tar_format", "结果归档必须使用 ustar 格式。")
        name = _tar_text(header[0:100], "member name")
        prefix = _tar_text(header[345:500], "member prefix")
        if prefix:
            name = f"{prefix}/{name}"
        if _unsafe_member_name(name):
            _fail(
                "unsafe_member_path",
                "归档成员路径不安全。",
                {"member": name},
            )
        if name in members:
            _fail(
                "duplicate_member",
                "归档包含重复成员。",
                {"member": name},
            )

        type_flag = header[156:157]
        if type_flag not in (b"0", b"\0"):
            kind = {
                b"1": "hardlink",
                b"2": "symlink",
                b"3": "character_device",
                b"4": "block_device",
                b"5": "directory",
                b"6": "fifo",
            }.get(type_flag, "special")
            _fail(
                "unsafe_member_type",
                "归档成员必须是普通文件。",
                {"member": name, "type": kind},
            )
        if name not in MEMBER_LIMITS:
            _fail(
                "extra_member",
                "归档包含白名单以外的成员。",
                {"member": name},
            )
        mode = _tar_octal(header[100:108], "member mode")
        uid = _tar_octal(header[108:116], "member uid")
        gid = _tar_octal(header[116:124], "member gid")
        mtime = _tar_octal(header[136:148], "member mtime")
        uname = _tar_text(header[265:297], "member uname")
        gname = _tar_text(header[297:329], "member gname")
        if mode != 0o644:
            _fail(
                "non_normalized_tar_metadata",
                "归档成员 mode 必须为 0644。",
                {"member": name, "mode": oct(mode)},
            )
        if uid != 0 or gid != 0:
            _fail(
                "non_normalized_tar_metadata",
                "归档成员 uid/gid 必须为 0。",
                {"member": name, "uid": uid, "gid": gid},
            )
        if (uname, gname) not in (("", ""), ("root", "root")):
            _fail(
                "non_normalized_tar_metadata",
                "归档成员 uname/gname 必须为空或 root。",
                {"member": name},
            )
        if mtime != NORMALIZED_MTIME:
            _fail(
                "non_normalized_tar_metadata",
                "归档成员 mtime 未归一化。",
                {"member": name, "mtime": mtime},
            )

        size = _tar_octal(header[124:136], "member size")
        limit = MEMBER_LIMITS[name]
        if size > limit:
            _fail(
                "member_too_large",
                "归档成员超过硬上限。",
                {"member": name, "limit_bytes": limit},
            )
        total_size += size
        if total_size > MAX_TOTAL_MEMBER_BYTES:
            _fail("members_too_large", "归档成员总大小超过硬上限。")

        content_start = offset + TAR_BLOCK_SIZE
        content_end = content_start + size
        next_offset = content_start + (
            (size + TAR_BLOCK_SIZE - 1) // TAR_BLOCK_SIZE * TAR_BLOCK_SIZE
        )
        if content_end > len(payload) or next_offset > len(payload):
            _fail("truncated_member", "归档成员数据被截断。", {"member": name})
        if any(payload[content_end:next_offset]):
            _fail(
                "invalid_tar_padding",
                "归档成员 padding 包含非零数据。",
                {"member": name},
            )
        members[name] = payload[content_start:content_end]
        member_order.append(name)
        offset = next_offset

    if not saw_end:
        _fail("invalid_tar", "tar 缺少结束块。")
    missing = sorted(set(EXPECTED_MEMBERS) - set(members))
    if missing:
        _fail("missing_member", "归档缺少必需成员。", {"members": missing})
    if len(members) != len(EXPECTED_MEMBERS):
        _fail("invalid_member_set", "归档成员集合无效。")
    if tuple(member_order) != EXPECTED_MEMBERS:
        _fail(
            "non_normalized_tar_order",
            "归档成员顺序未按生成器规范固定。",
            {"members": member_order},
        )
    return members


def _decode_utf8(data: bytes, member: str) -> str:
    try:
        text = data.decode("utf-8", errors="strict")
    except UnicodeDecodeError:
        _fail("invalid_utf8", "文本成员不是 UTF-8。", {"member": member})
    if "\x00" in text:
        _fail("invalid_text", "文本成员包含 NUL。", {"member": member})
    return text


def _verify_manifest(members: dict[str, bytes]) -> None:
    text = _decode_utf8(members["SHA256SUMS"], "SHA256SUMS")
    lines = text.splitlines()
    if len(lines) != len(HASHED_MEMBERS) or not text.endswith("\n"):
        _fail("invalid_manifest", "SHA256SUMS 必须恰好包含三行。")
    seen: set[str] = set()
    for line in lines:
        match = re.fullmatch(
            r"([0-9a-fA-F]{64})[ \t]+([A-Za-z0-9._-]+)", line
        )
        if match is None:
            _fail("invalid_manifest", "SHA256SUMS 行格式无效。")
        expected_hash, name = match.groups()
        if name in seen or name not in HASHED_MEMBERS:
            _fail("invalid_manifest", "SHA256SUMS 文件名无效或重复。")
        seen.add(name)
        actual_hash = hashlib.sha256(members[name]).hexdigest()
        if not hmac.compare_digest(expected_hash.lower(), actual_hash):
            _fail(
                "hash_mismatch",
                "归档成员 SHA-256 不匹配。",
                {"member": name},
            )


def _json_without_duplicates(text: str) -> Any:
    def object_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                _fail(
                    "duplicate_json_key",
                    "report.json 包含重复 JSON key。",
                    {"key": key},
                )
            result[key] = value
        return result

    def invalid_constant(value: str) -> NoReturn:
        _fail("invalid_report_json", f"report.json 包含无效常量 {value}。")

    try:
        return json.loads(
            text, object_pairs_hook=object_pairs, parse_constant=invalid_constant
        )
    except VerificationError:
        raise
    except (json.JSONDecodeError, RecursionError):
        _fail("invalid_report_json", "report.json 不是有效 JSON。")


def _dict(value: Any, field: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail("invalid_report_schema", f"report.json 的 {field} 必须是对象。")
    return value


def _nonempty_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value.strip() or "\n" in value:
        _fail("invalid_report_schema", f"report.json 的 {field} 必须是非空单行字符串。")
    return value


def _boolean(value: Any, field: str) -> bool:
    if type(value) is not bool:
        _fail("invalid_report_schema", f"report.json 的 {field} 必须是布尔值。")
    return value


def _integer(value: Any, field: str) -> int:
    if type(value) is not int or value < 0:
        _fail("invalid_report_schema", f"report.json 的 {field} 必须是非负整数。")
    return value


def _validate_launcher_diagnostics(report: dict[str, Any]) -> None:
    execution_value = report.get("execution")
    error_value = report.get("error")
    if execution_value is None and error_value is None:
        return
    execution = _dict(execution_value, "execution")
    phase = execution.get("phase")
    if phase not in ("build", "runner", "archive"):
        _fail("invalid_report_schema", "report execution.phase 无效。")
    command_status = _integer(
        execution.get("command_status"), "execution.command_status"
    )
    if command_status > 255:
        _fail("invalid_report_schema", "report command_status 超出范围。")
    timed_out = _boolean(
        execution.get("timed_out"), "execution.timed_out"
    )
    timeout_seconds = _integer(
        execution.get("timeout_seconds"), "execution.timeout_seconds"
    )
    if not 1 <= timeout_seconds <= 600:
        _fail("invalid_report_schema", "report timeout_seconds 超出范围。")
    _boolean(
        execution.get("runner_pid_observed"),
        "execution.runner_pid_observed",
    )
    error = _dict(error_value, "error")
    error_code = _nonempty_string(error.get("code"), "error.code")
    _nonempty_string(error.get("message"), "error.message")
    if report["status"] == "passed":
        _fail("invalid_report_schema", "passed 报告不能包含 launcher error。")
    if timed_out and (phase != "runner" or command_status != 124):
        _fail("invalid_report_schema", "timeout 诊断与 runner 状态不一致。")
    if error_code == "runner_timeout" and not timed_out:
        _fail("invalid_report_schema", "runner_timeout 必须标记 timed_out。")


def _validate_report(data: bytes) -> dict[str, Any]:
    report = _json_without_duplicates(_decode_utf8(data, "report.json"))
    if not isinstance(report, dict):
        _fail("invalid_report_schema", "report.json 顶层必须是对象。")
    if report.get("schema_version") != REPORT_SCHEMA_VERSION:
        _fail("invalid_report_schema", "report schema_version 不受支持。")
    if report.get("kind") != REPORT_KIND:
        _fail("invalid_report_schema", "report kind 不受支持。")
    status = report.get("status")
    if status not in ("passed", "failed", "unsupported"):
        _fail("invalid_report_status", "report status 无效。")
    _nonempty_string(report.get("message"), "message")
    _validate_launcher_diagnostics(report)
    source_value = report.get("source")
    if source_value is not None:
        source = _dict(source_value, "source")
        revision = _nonempty_string(source.get("revision"), "source.revision")
        worktree = _nonempty_string(source.get("worktree"), "source.worktree")
        package_digest = _nonempty_string(
            source.get("package_digest"), "source.package_digest"
        )
        if (
            SOURCE_REVISION.fullmatch(revision) is None
            or worktree not in ("clean", "dirty")
            or LOWER_HEX_64.fullmatch(package_digest) is None
        ):
            _fail("invalid_source_provenance", "report source provenance 无效。")

    checks = report.get("checks")
    if not isinstance(checks, list):
        _fail("invalid_report_checks", "report checks 必须是数组。")
    check_ids: set[str] = set()
    counts = {"pass": 0, "fail": 0}
    for index, check in enumerate(checks):
        if not isinstance(check, dict):
            _fail("invalid_report_checks", "每个 report check 必须是对象。")
        check_id = _nonempty_string(check.get("id"), f"checks[{index}].id")
        if check_id in check_ids:
            _fail("invalid_report_checks", "report check id 重复。", {"id": check_id})
        check_ids.add(check_id)
        check_status = check.get("status")
        if check_status not in ("pass", "fail", "unsupported"):
            _fail("invalid_report_checks", "report check status 无效。", {"id": check_id})
        _nonempty_string(check.get("message"), f"checks[{index}].message")
        if check_status in counts:
            counts[check_status] += 1

    summary = _dict(report.get("summary"), "summary")
    summary_passed = _integer(summary.get("passed"), "summary.passed")
    summary_failed = _integer(summary.get("failed"), "summary.failed")
    summary_total = _integer(summary.get("total"), "summary.total")
    if (
        summary_passed != counts["pass"]
        or summary_failed != counts["fail"]
        or summary_total != len(checks)
    ):
        _fail("invalid_report_summary", "report summary 与 checks 不一致。")

    if status == "passed":
        if not checks or any(
            check.get("status") != "pass" for check in checks
        ):
            _fail(
                "invalid_report_checks",
                "passed 报告的全部 checks 必须通过。",
            )
        missing_checks = sorted(REQUIRED_PASSED_CHECKS - check_ids)
        if missing_checks:
            _fail(
                "missing_required_checks",
                "passed 报告缺少必需 check。",
                {"checks": missing_checks},
            )

    timestamp = report.get("timestamp_utc")
    if status == "passed" or timestamp is not None:
        timestamp = _nonempty_string(timestamp, "timestamp_utc")
        if not TIMESTAMP_UTC.fullmatch(timestamp):
            _fail("invalid_report_schema", "report timestamp_utc 格式无效。")
    platform = _dict(report.get("platform"), "platform")
    platform_os = _nonempty_string(platform.get("os"), "platform.os")
    if status == "passed" and platform_os != "macos":
        _fail("invalid_report_platform", "passed report 必须来自 macOS。")
    architecture = platform.get("architecture")
    allowed_architectures = (
        SUPPORTED_ARCHITECTURES
        if status == "passed"
        else SUPPORTED_ARCHITECTURES | {"unknown"}
    )
    if architecture not in allowed_architectures:
        _fail("invalid_report_architecture", "report architecture 不受支持。")
    if status == "passed":
        _nonempty_string(platform.get("version"), "platform.version")
        _boolean(
            platform.get("rosetta_translated"),
            "platform.rosetta_translated",
        )

    if status == "passed":
        identity = _dict(report.get("identity"), "identity")
        if identity.get("runner_bundle_id") != RUNNER_BUNDLE_ID:
            _fail("invalid_bundle_id", "report runner bundle ID 不匹配。")
        if identity.get("fixture_bundle_id") != FIXTURE_BUNDLE_ID:
            _fail("invalid_bundle_id", "report fixture bundle ID 不匹配。")
        stability = identity.get("launcher_declared_identity_stability")
        if stability not in IDENTITY_STABILITIES:
            _fail(
                "invalid_identity_stability",
                "report identity stability 无效。",
            )

    permissions = _dict(report.get("permissions"), "permissions")
    accessibility = _dict(
        permissions.get("accessibility"), "permissions.accessibility"
    )
    if status == "passed":
        if _boolean(
            accessibility.get("trusted"),
            "permissions.accessibility.trusted",
        ) is not True:
            _fail(
                "invalid_report_permissions",
                "passed 报告必须已获得 Accessibility trust。",
            )
    elif "trusted" in accessibility:
        _boolean(
            accessibility.get("trusted"),
            "permissions.accessibility.trusted",
        )
    else:
        _boolean(
            accessibility.get("checked"),
            "permissions.accessibility.checked",
        )
    if "prompt_requested" in accessibility:
        _boolean(
            accessibility.get("prompt_requested"),
            "permissions.accessibility.prompt_requested",
        )

    capture = _dict(
        permissions.get("screen_capture"), "permissions.screen_capture"
    )
    if status == "passed":
        _boolean(
            capture.get("preflight_granted"),
            "permissions.screen_capture.preflight_granted",
        )
    elif "preflight_granted" in capture:
        _boolean(
            capture.get("preflight_granted"),
            "permissions.screen_capture.preflight_granted",
        )
    else:
        _boolean(
            capture.get("checked"),
            "permissions.screen_capture.checked",
        )
    for field in ("request_attempted", "capture_attempted"):
        if _boolean(capture.get(field), f"permissions.screen_capture.{field}"):
            _fail(
                "unsafe_report_claim",
                "报告显示曾请求授权或采集屏幕内容。",
            )

    limits_value = report.get("limits")
    if status == "passed" or limits_value is not None:
        limits = _dict(limits_value, "limits")
        if limits.get("target_scope") != "fixture_process_only":
            _fail("unsafe_report_claim", "report target scope 超出 fixture 进程。")
        if _boolean(
            limits.get("screen_content_collected"),
            "limits.screen_content_collected",
        ):
            _fail("unsafe_report_claim", "报告显示采集了屏幕内容。")
    return report


def _parse_identity(
    data: bytes, *, allow_unavailable: bool
) -> dict[str, Any]:
    text = _decode_utf8(data, "identity.txt")
    if text == "identity_attestation=unavailable\n":
        if not allow_unavailable:
            _fail("invalid_identity", "passed 报告缺少 identity attestation。")
        return {"available": False}
    if not text.endswith("\n"):
        _fail("invalid_identity", "identity.txt 必须以换行结束。")
    lines = text.splitlines()
    if len(lines) > 64:
        _fail("invalid_identity", "identity.txt 行数超过上限。")

    globals_: dict[str, str] = {}
    sections: dict[str, dict[str, str]] = {}
    current: dict[str, str] | None = None
    for line in lines:
        if not line or line != line.strip():
            _fail("invalid_identity", "identity.txt 包含空行或行首尾空白。")
        section_match = re.fullmatch(r"\[(runner|fixture)\]", line)
        if section_match:
            name = section_match.group(1)
            if name in sections:
                _fail("invalid_identity", "identity section 重复。", {"section": name})
            current = {}
            sections[name] = current
            continue
        if line.startswith("designated => "):
            key, value = "designated", line[len("designated => ") :]
        elif "=" in line:
            key, value = line.split("=", 1)
        else:
            _fail("invalid_identity", "identity.txt 行格式无效。")
        target = globals_ if current is None else current
        if not key or not value or key in target:
            _fail("invalid_identity", "identity 字段为空或重复。", {"field": key})
        target[key] = value

    required_globals = {"swift", "identity_stability"}
    provenance_globals = {
        "source_revision", "source_worktree", "source_package_digest"
    }
    if set(globals_) not in (required_globals, required_globals | provenance_globals):
        _fail("invalid_identity", "identity 顶层必填字段不完整。")
    if set(sections) != {"runner", "fixture"}:
        _fail("invalid_identity", "identity runner/fixture section 不完整。")
    stability = globals_["identity_stability"]
    if stability not in IDENTITY_STABILITIES:
        _fail("invalid_identity_stability", "identity stability 无效。")

    expected_ids = {"runner": RUNNER_BUNDLE_ID, "fixture": FIXTURE_BUNDLE_ID}
    allowed_fields = {
        "designated", "Identifier", "TeamIdentifier",
        "CDHash", "architectures", "sha256",
    }
    required_fields = allowed_fields - {"TeamIdentifier"}
    parsed_sections: dict[str, dict[str, Any]] = {}
    for name, fields in sections.items():
        if not required_fields.issubset(fields) or not set(fields).issubset(
            allowed_fields
        ):
            _fail(
                "invalid_identity",
                "identity section 字段不完整或未知。",
                {"section": name},
            )
        if fields["Identifier"] != expected_ids[name]:
            _fail("invalid_bundle_id", "identity bundle ID 不匹配。", {"section": name})
        if not CDHASH.fullmatch(fields["CDHash"]):
            _fail("invalid_cdhash", "identity CDHash 格式无效。", {"section": name})
        if not HEX_64.fullmatch(fields["sha256"]):
            _fail(
                "invalid_executable_hash",
                "identity executable SHA-256 格式无效。",
                {"section": name},
            )
        architectures = fields["architectures"].split(" ")
        if (
            not architectures
            or len(set(architectures)) != len(architectures)
            or not set(architectures).issubset(SUPPORTED_ARCHITECTURES)
        ):
            _fail(
                "invalid_identity_architecture",
                "identity architectures 无效。",
                {"section": name},
            )
        parsed_sections[name] = {
            "bundle_id": fields["Identifier"],
            "architectures": architectures,
            "sha256": fields["sha256"].lower(),
        }
    if set(parsed_sections["runner"]["architectures"]) != set(
        parsed_sections["fixture"]["architectures"]
    ):
        _fail("identity_architecture_mismatch", "runner 与 fixture 架构集合不一致。")
    result = {
        "available": True,
        "stability": stability,
        "runner": parsed_sections["runner"],
        "fixture": parsed_sections["fixture"],
    }
    if provenance_globals.issubset(globals_):
        if (
            SOURCE_REVISION.fullmatch(globals_["source_revision"]) is None
            or globals_["source_worktree"] not in ("clean", "dirty")
            or LOWER_HEX_64.fullmatch(
                globals_["source_package_digest"]
            ) is None
        ):
            _fail("invalid_source_provenance", "identity source provenance 无效。")
        result["source"] = {
            "revision": globals_["source_revision"],
            "worktree": globals_["source_worktree"],
            "package_digest": globals_["source_package_digest"],
        }
    return result


def _inspect_nonpassed_identity(data: bytes) -> dict[str, Any]:
    text = _decode_utf8(data, "identity.txt")
    if text.startswith("identity_attestation=unavailable\n"):
        lines = text.splitlines()
        if len(lines) == 1:
            return {"available": False, "validated": False}
        if len(lines) == 4:
            values: dict[str, str] = {}
            for line in lines[1:]:
                if "=" not in line:
                    _fail("invalid_source_provenance", "identity source provenance 无效。")
                key, value = line.split("=", 1)
                if key in values:
                    _fail("invalid_source_provenance", "identity source provenance 重复。")
                values[key] = value
            if (
                set(values)
                != {"source_revision", "source_worktree", "source_package_digest"}
                or SOURCE_REVISION.fullmatch(values["source_revision"]) is None
                or values["source_worktree"] not in ("clean", "dirty")
                or LOWER_HEX_64.fullmatch(values["source_package_digest"]) is None
            ):
                _fail("invalid_source_provenance", "identity source provenance 无效。")
            return {
                "available": False,
                "validated": False,
                "source": {
                    "revision": values["source_revision"],
                    "worktree": values["source_worktree"],
                    "package_digest": values["source_package_digest"],
                },
            }
        _fail("invalid_source_provenance", "identity unavailable 格式无效。")
    result: dict[str, Any] = {
        "available": True,
        "validated": False,
        "sha256": hashlib.sha256(data).hexdigest(),
    }
    provenance: dict[str, str] = {}
    for line in text.splitlines():
        if line.startswith("["):
            break
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        if key in (
            "source_revision", "source_worktree",
            "source_package_digest",
        ):
            if key in provenance:
                _fail("invalid_source_provenance", "identity source provenance 重复。")
            provenance[key] = value
    if provenance:
        if (
            set(provenance)
            != {"source_revision", "source_worktree", "source_package_digest"}
            or SOURCE_REVISION.fullmatch(provenance["source_revision"]) is None
            or provenance["source_worktree"] not in ("clean", "dirty")
            or LOWER_HEX_64.fullmatch(
                provenance["source_package_digest"]
            ) is None
        ):
            _fail("invalid_source_provenance", "identity source provenance 无效。")
        result["source"] = {
            "revision": provenance["source_revision"],
            "worktree": provenance["source_worktree"],
            "package_digest": provenance["source_package_digest"],
        }
    return result


def verify(
    path: Path, expected_archive_sha256: str | None = None,
    expected_source_revision: str | None = None,
    expected_source_package_digest: str | None = None,
) -> dict[str, Any]:
    compressed, metadata = _read_archive(path)
    archive_sha256 = hashlib.sha256(compressed).hexdigest()
    members = _parse_ustar(_decompress(compressed))
    _verify_manifest(members)
    _decode_utf8(members["README.txt"], "README.txt")
    report = _validate_report(members["report.json"])
    report_passed = report["status"] == "passed"
    if report_passed:
        identity = _parse_identity(
            members["identity.txt"], allow_unavailable=False
        )
        report_identity = report["identity"]
        reported_stability = report_identity.get(
            "launcher_declared_identity_stability"
        )
        if (
            reported_stability is not None
            and identity["stability"] != reported_stability
        ):
            _fail(
                "identity_stability_mismatch",
                "report 与 identity stability 不一致。",
            )
    else:
        identity = _inspect_nonpassed_identity(members["identity.txt"])
    source = report.get("source")
    identity_source = identity.get("source")
    if (source is None) != (identity_source is None):
        _fail(
            "source_provenance_mismatch",
            "report 与 identity 必须同时携带或同时缺少 source provenance。",
        )
    if source is not None and identity_source is not None and source != identity_source:
        _fail(
            "source_provenance_mismatch",
            "report 与 identity 的 source provenance 不一致。",
        )
    report_platform = report["platform"]
    architecture = report_platform["architecture"]
    if report_passed:
        for section in ("runner", "fixture"):
            if architecture not in identity[section]["architectures"]:
                _fail(
                    "identity_architecture_mismatch",
                    "report architecture 不在 identity 架构中。",
                    {"section": section},
                )

    trusted_archive = False
    source_trusted = False
    source_binding_matches = False
    trust_error: dict[str, Any] | None = None
    if expected_archive_sha256 is not None:
        if not HEX_64.fullmatch(expected_archive_sha256):
            trust_error = {
                "code": "invalid_expected_archive_sha256",
                "message": "受信任的预期归档 SHA-256 必须是 64 位十六进制。",
            }
        elif not hmac.compare_digest(
            expected_archive_sha256.lower(), archive_sha256
        ):
            trust_error = {
                "code": "archive_sha256_mismatch",
                "message": "归档 SHA-256 与受信任预期值不一致。",
            }
        else:
            trusted_archive = True
    if trust_error is None and (
        expected_source_revision is not None
        or expected_source_package_digest is not None
    ):
        if expected_source_revision is None or expected_source_package_digest is None:
            trust_error = {
                "code": "incomplete_expected_source",
                "message": "可信源码预期值必须同时提供 revision 和 package digest。",
            }
        elif SOURCE_REVISION.fullmatch(expected_source_revision) is None:
            trust_error = {
                "code": "invalid_expected_source_revision",
                "message": "受信任的预期源码 revision 必须是小写 Git commit SHA。",
            }
        elif LOWER_HEX_64.fullmatch(expected_source_package_digest) is None:
            trust_error = {
                "code": "invalid_expected_source_package_digest",
                "message": "受信任的预期源码 package digest 必须是 64 位小写十六进制。",
            }
        elif not trusted_archive:
            # Matching values inside an unauthenticated result archive cannot
            # prove which source produced it. Preserve the outer archive hash
            # as the root of trust before evaluating the source binding.
            pass
        elif source is None or identity_source is None:
            trust_error = {
                "code": "source_provenance_missing",
                "message": "旧报告未携带可校验的源码 provenance；拒绝源码信任。",
            }
        elif not hmac.compare_digest(
            expected_source_revision, source["revision"]
        ):
            trust_error = {
                "code": "source_revision_mismatch",
                "message": "报告源码 revision 与受信任预期值不一致。",
            }
        elif not hmac.compare_digest(
            expected_source_package_digest, source["package_digest"]
        ):
            trust_error = {
                "code": "source_package_digest_mismatch",
                "message": "报告源码 package digest 与受信任预期值不一致。",
            }
        elif source["worktree"] != "clean":
            trust_error = {
                "code": "source_worktree_dirty",
                "message": "报告来自显式允许的 dirty 开发源码包，不能用于资格认定。",
            }
        else:
            source_binding_matches = True
    source_trusted = trusted_archive and source_binding_matches
    qualified = report_passed and trusted_archive and source_trusted
    result: dict[str, Any] = {
        "schema_version": VERIFIER_SCHEMA,
        "status": "passed" if qualified else "failed",
        "archive_valid": True,
        "verified_archive": True,
        "report_passed": report_passed,
        "trusted_archive": trusted_archive,
        "source_trusted": source_trusted,
        "qualified": qualified,
        "archive": {
            "sha256": archive_sha256,
            "size_bytes": metadata.st_size,
            "members": list(EXPECTED_MEMBERS),
        },
        "report": {
            "status": report["status"],
            "architecture": architecture,
            "checks_passed": report["summary"]["passed"],
        },
        "identity": identity,
        "source": source,
    }
    if trust_error is not None:
        result["error"] = trust_error
    elif not report_passed:
        result["error"] = {
            "code": "report_not_passed",
            "message": "macOS 真机报告未通过。",
            "details": {"report_status": report["status"]},
        }
    elif not trusted_archive:
        result["error"] = {
            "code": "untrusted_archive",
            "message": (
                "报告内容通过，但未提供经独立可信渠道取得的归档 SHA-256；"
                "不能确认归档来源。"
            ),
        }
    elif not source_trusted:
        result["error"] = {
            "code": "untrusted_source",
            "message": (
                "归档来源已绑定，但未同时提供经独立可信渠道取得的源码 "
                "revision 与 package digest；不能确认测试源码。"
            ),
        }
    return result


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    expected_archive_sha256: str | None = None
    expected_source_revision: str | None = None
    expected_source_package_digest: str | None = None
    archive_argument: str | None = None
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument in (
            "--expected-archive-sha256",
            "--expected-source-revision",
            "--expected-source-package-digest",
        ):
            if index + 1 >= len(arguments):
                archive_argument = None
                break
            if argument == "--expected-archive-sha256":
                expected_archive_sha256 = arguments[index + 1]
            elif argument == "--expected-source-revision":
                expected_source_revision = arguments[index + 1]
            else:
                expected_source_package_digest = arguments[index + 1]
            index += 2
        elif argument not in ("-h", "--help") and archive_argument is None:
            archive_argument = argument
            index += 1
        else:
            archive_argument = None
            break
    if archive_argument is None:
        _emit({
            "schema_version": VERIFIER_SCHEMA,
            "status": "failed",
            "archive_valid": False,
            "verified_archive": False,
            "report_passed": False,
            "trusted_archive": False,
            "source_trusted": False,
            "qualified": False,
            "error": {
                "code": "usage",
                "message": (
                    "用法：verify-result.sh [--expected-archive-sha256 HEX] "
                    "[--expected-source-revision SHA] "
                    "[--expected-source-package-digest HEX] "
                    "/path/to/macos-ax-test-result.tar.gz"
                ),
            },
        })
        return 64
    try:
        result = verify(
            Path(archive_argument), expected_archive_sha256,
            expected_source_revision, expected_source_package_digest,
        )
    except VerificationError as exc:
        error: dict[str, Any] = {"code": exc.code, "message": exc.message}
        if exc.details:
            error["details"] = exc.details
        _emit({
            "schema_version": VERIFIER_SCHEMA,
            "status": "failed",
            "archive_valid": False,
            "verified_archive": False,
            "report_passed": False,
            "trusted_archive": False,
            "source_trusted": False,
            "qualified": False,
            "error": error,
        })
        return 1
    except Exception:
        _emit({
            "schema_version": VERIFIER_SCHEMA,
            "status": "failed",
            "archive_valid": False,
            "verified_archive": False,
            "report_passed": False,
            "trusted_archive": False,
            "source_trusted": False,
            "qualified": False,
            "error": {
                "code": "internal_error",
                "message": "验真器发生未预期错误。",
            },
        })
        return 1
    _emit(result)
    return 0 if result["qualified"] else 1



if __name__ == "__main__":
    raise SystemExit(main())
