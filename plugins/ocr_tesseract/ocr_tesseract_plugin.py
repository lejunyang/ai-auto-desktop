#!/usr/bin/env python3
"""Explicit-image Tesseract OCR provider over the NDJSON process protocol.

The provider never captures a screen.  Every recognition request names an
existing image artifact, and an optional pixel region is cropped before the
Tesseract process is started.  Protocol output is written only to stdout;
diagnostics are written to stderr.
"""

from __future__ import annotations

from collections import defaultdict
import bisect
import csv
import hashlib
import io
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, NoReturn


PLUGIN_NAME = "vision.ocr"
PLUGIN_VERSION = "0.1.0"
ACTION_ID = "vision.ocr.recognize@1"
MAX_IMAGE_BYTES = 64 * 1024 * 1024
MAX_ENGINE_STDOUT_BYTES = 4 * 1024 * 1024
MAX_ENGINE_STDERR_BYTES = 64 * 1024
MAX_NDJSON_BYTES = 7 * 1024 * 1024
MAX_TSV_ROWS = 20_000
MAX_WORDS = 20_000
MAX_LINES = 10_000
MAX_TEXT_CHARS = 1_000_000
MAX_WORD_TEXT_CHARS = 4_096
MAX_MATCHES = 10_000
MAX_COORDINATE = 1_000_000_000
DEFAULT_ENGINE_TIMEOUT_SECONDS = 30.0
PROCESS_POLL_SECONDS = 0.02
PROCESS_TERMINATE_GRACE_SECONDS = 0.20
RESPONSE_BUDGET_SECONDS = 0.10

BOUNDS_SCHEMA: dict[str, Any] = {
    "type": "object",
    "required": ["x", "y", "width", "height"],
    "properties": {
        "x": {"type": "integer", "minimum": 0},
        "y": {"type": "integer", "minimum": 0},
        "width": {"type": "integer", "minimum": 1},
        "height": {"type": "integer", "minimum": 1},
    },
    "additionalProperties": False,
}

ACTION_CONTRACT: dict[str, Any] = {
    "contract_major": 1,
    "description": (
        "Recognize text in an explicit image file with Tesseract. This action "
        "never captures the screen."
    ),
    "effect": {"default_class": "read_only"},
    "risk": {"category": "observe", "level": "low"},
    "input_schema": {
        "type": "object",
        "properties": {
            "image": {
                "type": "object",
                "required": ["path"],
                "properties": {"path": {"type": "string", "minLength": 1}},
                "additionalProperties": False,
            },
            "artifact": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "media_type": {
                        "type": "string",
                        "pattern": "^image/[A-Za-z0-9.+-]+$",
                    },
                },
                "additionalProperties": False,
            },
            "region": BOUNDS_SCHEMA,
            "languages": {
                "type": "array",
                "items": {
                    "type": "string",
                    "pattern": "^[A-Za-z0-9_]+$",
                    "maxLength": 64,
                },
                "minItems": 1,
                "maxItems": 32,
                "uniqueItems": True,
            },
            "minimum_confidence": {
                "type": "number",
                "minimum": 0,
                "maximum": 1,
            },
            "patterns": {
                "type": "array",
                "maxItems": 128,
                "items": {
                    "type": "object",
                    "required": ["id", "value"],
                    "properties": {
                        "id": {"type": "string", "minLength": 1, "maxLength": 128},
                        "value": {"type": "string", "minLength": 1, "maxLength": 1024},
                    },
                    "additionalProperties": False,
                },
            },
        },
        "oneOf": [
            {"required": ["image"], "not": {"required": ["artifact"]}},
            {"required": ["artifact"], "not": {"required": ["image"]}},
        ],
        "additionalProperties": False,
    },
    "output_schema": {
        "type": "object",
        "required": [
            "provider",
            "version",
            "source",
            "source_region",
            "text",
            "confidence",
            "lines",
            "matches",
        ],
        "properties": {
            "provider": {"const": "tesseract"},
            "version": {"type": "string"},
            "source": {
                "type": "object",
                "required": ["kind", "path", "digest", "media_type", "size_bytes"],
                "properties": {
                    "kind": {"enum": ["image", "artifact"]},
                    "path": {"type": "string"},
                    "digest": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$"},
                    "media_type": {"type": "string"},
                    "size_bytes": {"type": "integer", "minimum": 1},
                },
                "additionalProperties": False,
            },
            "source_region": {"oneOf": [BOUNDS_SCHEMA, {"type": "null"}]},
            "text": {"type": "string"},
            "confidence": {"type": "number", "minimum": 0, "maximum": 1},
            "lines": {"type": "array", "items": {"type": "object"}},
            "matches": {"type": "array", "items": {"type": "object"}},
        },
        "additionalProperties": False,
    },
    "errors": [
        {
            "code": code,
            "description": description,
            "retryable": retryable,
            "effect": "not_applied",
            "data_schema": {"type": "object"},
        }
        for code, description, retryable in (
            ("OCR.INVALID_REQUEST", "The image request or pattern is invalid.", False),
            ("OCR.IMAGE_UNAVAILABLE", "The explicit image cannot be read.", False),
            ("OCR.IMAGE_UNSUPPORTED", "The file is not a supported image.", False),
            ("OCR.ENGINE_UNAVAILABLE", "The Tesseract CLI is unavailable.", False),
            ("OCR.ENGINE_FAILED", "Tesseract exited unsuccessfully.", False),
            ("OCR.OUTPUT_INVALID", "Tesseract emitted invalid or excessive TSV.", False),
            ("OCR.NO_TEXT", "No text was recognized.", False),
            ("OCR.LOW_CONFIDENCE", "OCR confidence is below the requested minimum.", False),
            ("OCR.TIMEOUT", "The host deadline elapsed during OCR.", True),
        )
    ],
}

MANIFEST: dict[str, Any] = {
    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
    "kind": "CapabilityManifest",
    "metadata": {
        "name": PLUGIN_NAME,
        "version": PLUGIN_VERSION,
        "description": "Optional process-isolated Tesseract OCR provider.",
    },
    "permissions": ["filesystem.read"],
    "actions": {"recognize": ACTION_CONTRACT},
    "runtime": {
        "kind": "process",
        "protocol": "ndjson-stdio-v1",
        "entrypoint": "./run.sh",
    },
}


class RequestError(Exception):
    def __init__(
        self, code: str, message: str, *, retryable: bool = False, data: Any = None
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.retryable = retryable
        self.data = data


def debug(message: str) -> None:
    print(f"[{PLUGIN_NAME}] {message}", file=sys.stderr, flush=True)


def emit(message: dict[str, Any]) -> None:
    encoded = (
        json.dumps(message, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
        + "\n"
    ).encode("utf-8")
    if len(encoded) > MAX_NDJSON_BYTES:
        raise RequestError(
            "OCR.OUTPUT_INVALID",
            "OCR response exceeded the provider NDJSON limit",
            data={"limit_bytes": MAX_NDJSON_BYTES},
        )
    sys.stdout.buffer.write(encoded)
    sys.stdout.buffer.flush()


def emit_result(request_id: Any, result: Any) -> None:
    emit({"id": request_id, "result": result})


def emit_error(request_id: Any, error: RequestError) -> None:
    payload: dict[str, Any] = {
        "code": error.code,
        "message": error.message,
        "retryable": error.retryable,
    }
    if error.data is not None:
        payload["data"] = error.data
    emit({"id": request_id, "error": payload})


def _invalid(message: str, **data: Any) -> NoReturn:
    raise RequestError("OCR.INVALID_REQUEST", message, data=data or None)


def _request_deadline(deadline_ms: Any) -> float:
    if deadline_ms is None:
        return time.monotonic() + DEFAULT_ENGINE_TIMEOUT_SECONDS
    if isinstance(deadline_ms, bool) or not isinstance(deadline_ms, (int, float)):
        _invalid("deadline_ms must be a Unix timestamp in milliseconds")
    remaining = float(deadline_ms) / 1000.0 - time.time()
    if not math.isfinite(remaining) or remaining <= 0:
        raise RequestError(
            "OCR.TIMEOUT",
            "host deadline elapsed before OCR could complete",
            retryable=True,
        )
    if remaining <= RESPONSE_BUDGET_SECONDS:
        raise RequestError(
            "OCR.TIMEOUT",
            "host deadline elapsed before OCR could complete",
            retryable=True,
        )
    # Finish slightly ahead of the host deadline so a structured timeout can
    # be serialized before the host tears down the provider process.
    return time.monotonic() + remaining - RESPONSE_BUDGET_SECONDS


def _remaining(deadline: float) -> float:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise RequestError(
            "OCR.TIMEOUT",
            "host deadline elapsed before OCR could complete",
            retryable=True,
        )
    return remaining


def _absolute_path(value: Any) -> Path:
    if not isinstance(value, str) or not value:
        _invalid("source path must be a non-empty string")
    if "\x00" in value:
        _invalid("source path must not contain NUL")
    path = Path(value)
    if not path.is_absolute():
        _invalid("source path must be absolute; implicit screenshots are forbidden", path=value)
    try:
        resolved = path.resolve(strict=True)
    except OSError as exc:
        raise RequestError(
            "OCR.IMAGE_UNAVAILABLE",
            "source image cannot be accessed",
            data={"path": value, "reason": str(exc)},
        ) from exc
    return resolved


def _media_type(header: bytes, tail: bytes, size: int) -> str | None:
    if header.startswith(b"\x89PNG\r\n\x1a\n"):
        if (
            size < 33
            or len(header) < 24
            or header[8:12] != b"\x00\x00\x00\r"
            or header[12:16] != b"IHDR"
            or int.from_bytes(header[16:20], "big") <= 0
            or int.from_bytes(header[20:24], "big") <= 0
            or not tail.endswith(b"IEND\xaeB`\x82")
        ):
            return None
        return "image/png"
    if header.startswith(b"\xff\xd8\xff"):
        if size < 4 or not tail.endswith(b"\xff\xd9"):
            return None
        return "image/jpeg"
    if header.startswith((b"GIF87a", b"GIF89a")):
        if size < 13 or len(header) < 10 or header[6:10] == b"\x00\x00\x00\x00":
            return None
        return "image/gif"
    if header.startswith((b"II*\x00", b"MM\x00*")):
        if size < 8:
            return None
        return "image/tiff"
    if header.startswith(b"BM"):
        if size < 26 or len(header) < 6 or int.from_bytes(header[2:6], "little") > size:
            return None
        return "image/bmp"
    if len(header) >= 12 and header[:4] == b"RIFF" and header[8:12] == b"WEBP":
        if size < 16 or int.from_bytes(header[4:8], "little") + 8 > size:
            return None
        return "image/webp"
    if len(header) >= 2 and header[:1] == b"P" and header[1:2] in b"1234567":
        if size < 8 or len(header) < 3 or header[2:3] not in b" \t\r\n":
            return None
        return "image/x-portable-anymap"
    return None


def _inspect_source(
    args: dict[str, Any], deadline: float, directory: Path
) -> tuple[Path, dict[str, Any]]:
    present = [name for name in ("image", "artifact") if name in args]
    if len(present) != 1:
        _invalid("exactly one of image or artifact is required")
    kind = present[0]
    source_arg = args[kind]
    if not isinstance(source_arg, dict):
        _invalid(f"{kind} must be an object")
    allowed = {"path"} if kind == "image" else {"path", "media_type"}
    unexpected = sorted(set(source_arg) - allowed)
    if unexpected:
        _invalid(f"{kind} contains unsupported fields", fields=unexpected)
    path = _absolute_path(source_arg.get("path"))
    digest = hashlib.sha256()
    snapshot = directory / "source.img"
    header = bytearray()
    tail = bytearray()
    copied = 0
    try:
        with path.open("rb") as stream, snapshot.open("xb") as target:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode):
                raise RequestError(
                    "OCR.IMAGE_UNAVAILABLE",
                    "source image must be a regular file",
                    data={"path": str(path)},
                )
            if info.st_size <= 0 or info.st_size > MAX_IMAGE_BYTES:
                raise RequestError(
                    "OCR.IMAGE_UNAVAILABLE",
                    f"source image must be between 1 and {MAX_IMAGE_BYTES} bytes",
                    data={"path": str(path), "size_bytes": info.st_size},
                )
            while True:
                _remaining(deadline)
                chunk = stream.read(1024 * 1024)
                if not chunk:
                    break
                copied += len(chunk)
                if copied > MAX_IMAGE_BYTES:
                    raise RequestError(
                        "OCR.IMAGE_UNAVAILABLE",
                        f"source image exceeds {MAX_IMAGE_BYTES} bytes",
                        data={"path": str(path), "limit_bytes": MAX_IMAGE_BYTES},
                    )
                if len(header) < 64:
                    header.extend(chunk[: 64 - len(header)])
                tail.extend(chunk)
                if len(tail) > 16:
                    del tail[:-16]
                digest.update(chunk)
                target.write(chunk)
            if copied <= 0:
                raise RequestError(
                    "OCR.IMAGE_UNAVAILABLE",
                    "source image is empty",
                    data={"path": str(path)},
                )
    except RequestError:
        raise
    except OSError as exc:
        raise RequestError(
            "OCR.IMAGE_UNAVAILABLE",
            "source image could not be read",
            data={"path": str(path), "reason": str(exc)},
        ) from exc
    detected = _media_type(bytes(header), bytes(tail), copied)
    if detected is None:
        raise RequestError(
            "OCR.IMAGE_UNSUPPORTED",
            "source file does not have a supported image signature",
            data={"path": str(path)},
        )
    declared = source_arg.get("media_type")
    if declared is not None:
        if not isinstance(declared, str) or not declared.startswith("image/"):
            _invalid("artifact.media_type must be an image media type")
        normalized = "image/jpeg" if declared == "image/jpg" else declared.lower()
        if normalized != detected:
            raise RequestError(
                "OCR.IMAGE_UNSUPPORTED",
                "artifact media_type does not match the image content",
                data={"declared": declared, "detected": detected},
            )
    return snapshot, {
        "kind": kind,
        "path": str(path),
        "digest": f"sha256:{digest.hexdigest()}",
        "media_type": detected,
        "size_bytes": copied,
    }


def _region(value: Any) -> dict[str, int] | None:
    if value is None:
        return None
    if not isinstance(value, dict) or set(value) != {"x", "y", "width", "height"}:
        _invalid("region must contain only x, y, width, and height")
    result: dict[str, int] = {}
    for name in ("x", "y", "width", "height"):
        item = value[name]
        minimum = 1 if name in {"width", "height"} else 0
        if isinstance(item, bool) or not isinstance(item, int) or item < minimum:
            _invalid(f"region.{name} must be an integer >= {minimum}")
        result[name] = item
    return result


def _languages(value: Any) -> list[str]:
    if value is None:
        return []
    if not isinstance(value, list) or not value or len(value) > 32:
        _invalid("languages must be a non-empty array with at most 32 entries")
    result: list[str] = []
    for language in value:
        if not isinstance(language, str) or not re.fullmatch(r"[A-Za-z0-9_]{1,64}", language):
            _invalid("each language must match [A-Za-z0-9_]{1,64}")
        if language in result:
            _invalid("languages must be unique", language=language)
        result.append(language)
    return result


def _minimum_confidence(value: Any) -> float:
    if value is None:
        return 0.0
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        _invalid("minimum_confidence must be a number between 0 and 1")
    result = float(value)
    if not math.isfinite(result) or result < 0 or result > 1:
        _invalid("minimum_confidence must be a number between 0 and 1")
    return result


def _patterns(value: Any) -> list[tuple[str, str]]:
    if value is None:
        return []
    if not isinstance(value, list) or len(value) > 128:
        _invalid("patterns must be an array with at most 128 entries")
    result: list[tuple[str, str]] = []
    ids: set[str] = set()
    for index, pattern in enumerate(value):
        if not isinstance(pattern, dict) or set(pattern) != {"id", "value"}:
            _invalid("each pattern must contain only id and value", index=index)
        pattern_id, literal = pattern["id"], pattern["value"]
        if not isinstance(pattern_id, str) or not pattern_id or len(pattern_id) > 128:
            _invalid("pattern id must be a non-empty string up to 128 characters", index=index)
        if pattern_id in ids:
            _invalid("pattern ids must be unique", id=pattern_id)
        if not isinstance(literal, str) or not literal or len(literal) > 1024:
            _invalid("pattern value must be a non-empty string up to 1024 characters", id=pattern_id)
        if "\x00" in literal:
            _invalid("pattern value must not contain NUL", id=pattern_id)
        ids.add(pattern_id)
        result.append((pattern_id, literal))
    return result


def _engine_command() -> list[str]:
    raw = os.environ.get("OCR_TESSERACT_COMMAND")
    if raw:
        try:
            command = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise RequestError(
                "OCR.ENGINE_UNAVAILABLE",
                "OCR_TESSERACT_COMMAND must be a JSON argv array",
                data={"reason": str(exc)},
            ) from exc
        if (
            not isinstance(command, list)
            or not command
            or not all(isinstance(item, str) and item for item in command)
        ):
            raise RequestError(
                "OCR.ENGINE_UNAVAILABLE",
                "OCR_TESSERACT_COMMAND must be a non-empty JSON array of strings",
            )
        prefix = list(command)
    else:
        prefix = [os.environ.get("TESSERACT_CMD", "tesseract")]
    executable = shutil.which(prefix[0])
    if executable is None:
        raise RequestError(
            "OCR.ENGINE_UNAVAILABLE",
            "Tesseract executable was not found",
            data={"executable": prefix[0]},
        )
    prefix[0] = executable
    return prefix


def _engine_environment(executable: str) -> dict[str, str]:
    search_paths = [os.path.dirname(executable)]
    if os.name == "nt":
        system_root = os.environ.get("SystemRoot")
        if system_root:
            search_paths.append(os.path.join(system_root, "System32"))
    else:
        search_paths.extend(("/usr/bin", "/bin"))
    environment: dict[str, str] = {
        "PATH": os.pathsep.join(dict.fromkeys(filter(None, search_paths))),
        "PYTHONIOENCODING": "utf-8",
        "PYTHONUTF8": "1",
    }
    for name in ("LANG", "LC_ALL", "TESSDATA_PREFIX", "TEMP", "TMP"):
        value = os.environ.get(name)
        if value:
            environment[name] = value
    # Trusted test/wrapper configuration may explicitly namespace variables
    # for the engine; arbitrary host secrets are never inherited.
    for name, value in os.environ.items():
        if name.startswith("FAKE_TESSERACT_"):
            environment[name] = value
    return environment


def _terminate_process_tree(process: subprocess.Popen[bytes]) -> None:
    """Best-effort bounded termination of the engine and its descendants."""

    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except (OSError, ProcessLookupError):
            process.terminate()
        try:
            process.wait(timeout=PROCESS_TERMINATE_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            pass
        # The group leader may exit while a wrapper's descendants remain.
        # Always follow with SIGKILL for the same process group after the grace.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (OSError, ProcessLookupError):
            process.kill()
        try:
            process.wait(timeout=PROCESS_TERMINATE_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            process.kill()
        return
    elif os.name == "nt":
        try:
            subprocess.run(
                ["taskkill", "/PID", str(process.pid), "/T"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=PROCESS_TERMINATE_GRACE_SECONDS,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            process.terminate()
    else:
        process.terminate()
    try:
        process.wait(timeout=PROCESS_TERMINATE_GRACE_SECONDS)
        return
    except subprocess.TimeoutExpired:
        pass
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (OSError, ProcessLookupError):
            process.kill()
    elif os.name == "nt":
        try:
            subprocess.run(
                ["taskkill", "/F", "/PID", str(process.pid), "/T"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=PROCESS_TERMINATE_GRACE_SECONDS,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            process.kill()
    else:
        process.kill()
    try:
        process.wait(timeout=PROCESS_TERMINATE_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()


def _run_process(command: list[str], deadline: float) -> tuple[bytes, bytes]:
    _remaining(deadline)
    popen_options: dict[str, Any] = {
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "shell": False,
        "env": _engine_environment(command[0]),
    }
    if os.name == "posix":
        popen_options["start_new_session"] = True
    elif os.name == "nt":
        popen_options["creationflags"] = getattr(
            subprocess, "CREATE_NEW_PROCESS_GROUP", 0
        )
    try:
        process = subprocess.Popen(command, **popen_options)
    except FileNotFoundError as exc:
        raise RequestError(
            "OCR.ENGINE_UNAVAILABLE", "Tesseract executable was not found"
        ) from exc
    except OSError as exc:
        raise RequestError(
            "OCR.ENGINE_UNAVAILABLE",
            "Tesseract could not be started",
            data={"reason": str(exc)},
        ) from exc

    assert process.stdout is not None and process.stderr is not None
    stdout_bytes = bytearray()
    stderr_bytes = bytearray()
    overflow = threading.Event()
    overflow_stream: list[str] = []
    lock = threading.Lock()

    def drain(stream: Any, target: bytearray, limit: int, name: str) -> None:
        try:
            while True:
                chunk = stream.read(64 * 1024)
                if not chunk:
                    return
                with lock:
                    remaining = limit - len(target)
                    if remaining > 0:
                        target.extend(chunk[:remaining])
                    if len(chunk) > remaining:
                        if not overflow_stream:
                            overflow_stream.append(name)
                        overflow.set()
                        return
        except (OSError, ValueError):
            return

    readers = [
        threading.Thread(
            target=drain,
            args=(process.stdout, stdout_bytes, MAX_ENGINE_STDOUT_BYTES, "stdout"),
            daemon=True,
        ),
        threading.Thread(
            target=drain,
            args=(process.stderr, stderr_bytes, MAX_ENGINE_STDERR_BYTES, "stderr"),
            daemon=True,
        ),
    ]
    for reader in readers:
        reader.start()
    timed_out = False
    try:
        while process.poll() is None or any(reader.is_alive() for reader in readers):
            if overflow.wait(timeout=min(PROCESS_POLL_SECONDS, _remaining(deadline))):
                break
    except RequestError:
        timed_out = True
    if timed_out or overflow.is_set():
        _terminate_process_tree(process)
    else:
        process.wait()
    for reader in readers:
        reader.join(timeout=PROCESS_TERMINATE_GRACE_SECONDS)
    if overflow.is_set() and process.poll() is not None and os.name == "posix":
        _terminate_process_tree(process)
    if timed_out:
        raise RequestError(
            "OCR.TIMEOUT",
            "host deadline elapsed while Tesseract was running",
            retryable=True,
        )
    if overflow.is_set():
        stream_name = overflow_stream[0] if overflow_stream else "output"
        limit = (
            MAX_ENGINE_STDOUT_BYTES
            if stream_name == "stdout"
            else MAX_ENGINE_STDERR_BYTES
        )
        raise RequestError(
            "OCR.OUTPUT_INVALID",
            f"Tesseract {stream_name} exceeded the provider limit",
            data={"stream": stream_name, "limit_bytes": limit},
        )
    returncode = process.returncode
    if returncode != 0:
        raise RequestError(
            "OCR.ENGINE_FAILED",
            "Tesseract exited unsuccessfully",
            data={
                "returncode": returncode,
                "stderr": bytes(stderr_bytes[-8192:]).decode("utf-8", errors="replace"),
            },
        )
    return bytes(stdout_bytes), bytes(stderr_bytes)


_VERSION_CACHE: dict[tuple[str, ...], str] = {}


def _engine_version(prefix: list[str], deadline: float) -> str:
    key = tuple(prefix)
    if key in _VERSION_CACHE:
        return _VERSION_CACHE[key]
    stdout, stderr = _run_process(prefix + ["--version"], deadline)
    first_line = (stdout or stderr).decode("utf-8", errors="replace").splitlines()
    if not first_line:
        version = "unknown"
    else:
        match = re.search(r"(?:tesseract\s+)?([^\s]+)", first_line[0], re.IGNORECASE)
        version = match.group(1) if match else "unknown"
    _VERSION_CACHE[key] = version
    return version


def _crop_image(source: Path, region: dict[str, int], directory: Path) -> Path:
    try:
        from PIL import Image, UnidentifiedImageError
    except ImportError as exc:
        raise RequestError(
            "OCR.ENGINE_UNAVAILABLE",
            "Pillow is required when region cropping is requested",
            data={"dependency": "Pillow"},
        ) from exc
    try:
        with Image.open(source) as image:
            image.load()
            right = region["x"] + region["width"]
            bottom = region["y"] + region["height"]
            if right > image.width or bottom > image.height:
                _invalid(
                    "region falls outside the source image",
                    image_width=image.width,
                    image_height=image.height,
                )
            cropped = image.crop((region["x"], region["y"], right, bottom))
            target = directory / "region.png"
            cropped.save(target, format="PNG")
            return target
    except RequestError:
        raise
    except (OSError, UnidentifiedImageError, ValueError) as exc:
        raise RequestError(
            "OCR.IMAGE_UNSUPPORTED",
            "source image could not be decoded for region cropping",
            data={"reason": str(exc)},
        ) from exc


def _integer_field(row: dict[str, str], name: str, row_number: int) -> int:
    try:
        value = int(row[name])
    except (KeyError, TypeError, ValueError) as exc:
        raise RequestError(
            "OCR.OUTPUT_INVALID",
            "Tesseract TSV contains an invalid integer",
            data={"row": row_number, "field": name},
        ) from exc
    return value


def _union_bounds(items: list[dict[str, int]]) -> dict[str, int]:
    left = min(item["x"] for item in items)
    top = min(item["y"] for item in items)
    right = max(item["x"] + item["width"] for item in items)
    bottom = max(item["y"] + item["height"] for item in items)
    return {"x": left, "y": top, "width": right - left, "height": bottom - top}


def _parse_tsv(
    payload: bytes, offset: tuple[int, int], deadline: float
) -> tuple[str, float, list[dict[str, Any]], list[list[dict[str, Any]]]]:
    try:
        decoded = payload.decode("utf-8-sig")
    except UnicodeDecodeError as exc:
        raise RequestError("OCR.OUTPUT_INVALID", "Tesseract TSV is not UTF-8") from exc
    if "\x00" in decoded:
        raise RequestError("OCR.OUTPUT_INVALID", "Tesseract TSV contains NUL")
    required = {
        "level", "page_num", "block_num", "par_num", "line_num",
        "left", "top", "width", "height", "conf", "text",
    }
    try:
        reader = csv.DictReader(
            io.StringIO(decoded, newline=""), delimiter="\t", strict=True
        )
        fieldnames = reader.fieldnames
    except csv.Error as exc:
        raise RequestError(
            "OCR.OUTPUT_INVALID",
            "Tesseract TSV is malformed",
            data={"reason": str(exc)},
        ) from exc
    if fieldnames is None or not required.issubset(fieldnames):
        raise RequestError(
            "OCR.OUTPUT_INVALID",
            "Tesseract TSV is missing required columns",
            data={"columns": fieldnames or []},
        )
    groups: dict[tuple[int, int, int, int], list[dict[str, Any]]] = defaultdict(list)
    word_count = 0
    text_chars = 0
    try:
        for row_number, row in enumerate(reader, 2):
            if row_number > MAX_TSV_ROWS + 1:
                raise RequestError(
                    "OCR.OUTPUT_INVALID",
                    "Tesseract TSV exceeded the row limit",
                    data={"limit": MAX_TSV_ROWS},
                )
            if row_number % 128 == 0:
                _remaining(deadline)
            if None in row or any(value is None for value in row.values()):
                raise RequestError(
                    "OCR.OUTPUT_INVALID",
                    "Tesseract TSV contains a malformed row",
                    data={"row": row_number},
                )
            if _integer_field(row, "level", row_number) != 5:
                continue
            text = (row.get("text") or "").strip()
            if not text:
                continue
            if len(text) > MAX_WORD_TEXT_CHARS:
                raise RequestError(
                    "OCR.OUTPUT_INVALID",
                    "Tesseract word text exceeded the provider limit",
                    data={"row": row_number, "limit_chars": MAX_WORD_TEXT_CHARS},
                )
            try:
                raw_confidence = float(row["conf"])
            except (KeyError, TypeError, ValueError) as exc:
                raise RequestError(
                    "OCR.OUTPUT_INVALID",
                    "Tesseract TSV contains an invalid confidence",
                    data={"row": row_number},
                ) from exc
            if not math.isfinite(raw_confidence) or raw_confidence < 0:
                continue
            left = _integer_field(row, "left", row_number)
            top = _integer_field(row, "top", row_number)
            width = _integer_field(row, "width", row_number)
            height = _integer_field(row, "height", row_number)
            if left < 0 or top < 0 or width < 0 or height < 0:
                raise RequestError(
                    "OCR.OUTPUT_INVALID",
                    "Tesseract TSV contains negative coordinates",
                    data={"row": row_number},
                )
            if width == 0 or height == 0:
                continue
            if max(left, top, width, height) > MAX_COORDINATE:
                raise RequestError(
                    "OCR.OUTPUT_INVALID",
                    "Tesseract TSV coordinate exceeded the provider limit",
                    data={"row": row_number, "limit": MAX_COORDINATE},
                )
            key = tuple(
                _integer_field(row, name, row_number)
                for name in ("page_num", "block_num", "par_num", "line_num")
            )
            word_count += 1
            text_chars += len(text)
            if word_count > MAX_WORDS or text_chars > MAX_TEXT_CHARS:
                raise RequestError(
                    "OCR.OUTPUT_INVALID",
                    "Tesseract text exceeded the provider limit",
                    data={"word_limit": MAX_WORDS, "text_limit_chars": MAX_TEXT_CHARS},
                )
            groups[key].append(
                {
                    "text": text,
                    "confidence": max(0.0, min(1.0, raw_confidence / 100.0)),
                    "bounds": {
                        "x": left + offset[0],
                        "y": top + offset[1],
                        "width": width,
                        "height": height,
                    },
                }
            )
    except csv.Error as exc:
        raise RequestError(
            "OCR.OUTPUT_INVALID",
            "Tesseract TSV is malformed",
            data={"reason": str(exc)},
        ) from exc
    lines: list[dict[str, Any]] = []
    line_words: list[list[dict[str, Any]]] = []
    for words in groups.values():
        _remaining(deadline)
        if len(lines) >= MAX_LINES:
            raise RequestError(
                "OCR.OUTPUT_INVALID",
                "Tesseract TSV exceeded the line limit",
                data={"limit": MAX_LINES},
            )
        line_text = " ".join(word["text"] for word in words)
        weight = sum(max(1, len(word["text"])) for word in words)
        confidence = sum(
            word["confidence"] * max(1, len(word["text"])) for word in words
        ) / weight
        lines.append(
            {
                "text": line_text,
                "confidence": confidence,
                "bounds": _union_bounds([word["bounds"] for word in words]),
            }
        )
        line_words.append(words)
    if not lines:
        raise RequestError("OCR.NO_TEXT", "Tesseract did not recognize any text")
    text = "\n".join(line["text"] for line in lines)
    total_weight = sum(max(1, len(word["text"])) for words in line_words for word in words)
    confidence = sum(
        word["confidence"] * max(1, len(word["text"]))
        for words in line_words
        for word in words
    ) / total_weight
    return text, confidence, lines, line_words


def _matches(
    text: str,
    patterns: list[tuple[str, str]],
    lines: list[dict[str, Any]],
    line_words: list[list[dict[str, Any]]],
    deadline: float,
) -> list[dict[str, Any]]:
    word_spans: list[tuple[int, int, dict[str, Any]]] = []
    cursor = 0
    for line_index, words in enumerate(line_words):
        line_cursor = cursor
        for word_index, word in enumerate(words):
            if word_index:
                line_cursor += 1
            start = line_cursor
            line_cursor += len(word["text"])
            word_spans.append((start, line_cursor, word))
        cursor += len(lines[line_index]["text"]) + 1
    result: list[dict[str, Any]] = []
    span_ends = [end for _start, end, _word in word_spans]
    for pattern_id, literal in patterns:
        _remaining(deadline)
        search_from = 0
        while True:
            start = text.find(literal, search_from)
            if start < 0:
                break
            end = start + len(literal)
            first = bisect.bisect_right(span_ends, start)
            selected: list[dict[str, Any]] = []
            for span_start, _span_end, word in word_spans[first:]:
                if span_start >= end:
                    break
                selected.append(word)
            match_result: dict[str, Any] = {
                "pattern_id": pattern_id,
                "text": literal,
                "span": {"start": start, "end": end},
            }
            if selected:
                match_result["bounds"] = _union_bounds([word["bounds"] for word in selected])
                match_result["confidence"] = min(word["confidence"] for word in selected)
            else:
                match_result["bounds"] = None
                match_result["confidence"] = 0.0
            result.append(match_result)
            if len(result) > MAX_MATCHES:
                raise RequestError(
                    "OCR.OUTPUT_INVALID",
                    "pattern matches exceeded the provider limit",
                    data={"limit": MAX_MATCHES},
                )
            search_from = start + 1
            if len(result) % 128 == 0:
                _remaining(deadline)
    return result


def recognize(args: Any, deadline_ms: Any) -> dict[str, Any]:
    deadline = _request_deadline(deadline_ms)
    if not isinstance(args, dict):
        _invalid("args must be an object")
    allowed = {
        "image", "artifact", "region", "languages",
        "minimum_confidence", "patterns",
    }
    unexpected = sorted(set(args) - allowed)
    if unexpected:
        _invalid("request contains unsupported fields", fields=unexpected)
    region = _region(args.get("region"))
    languages = _languages(args.get("languages"))
    threshold = _minimum_confidence(args.get("minimum_confidence"))
    patterns = _patterns(args.get("patterns"))
    prefix = _engine_command()

    with tempfile.TemporaryDirectory(prefix="aad-ocr-") as directory_name:
        private_directory = Path(directory_name)
        source_path, source = _inspect_source(args, deadline, private_directory)
        version = _engine_version(prefix, deadline)
        input_path = source_path
        if region is not None:
            _remaining(deadline)
            input_path = _crop_image(source_path, region, private_directory)
            _remaining(deadline)
        command = prefix + [str(input_path), "stdout"]
        if languages:
            command += ["-l", "+".join(languages)]
        command.append("tsv")
        stdout, _stderr = _run_process(command, deadline)

    offset = (region["x"], region["y"]) if region else (0, 0)
    text, confidence, lines, line_words = _parse_tsv(stdout, offset, deadline)
    if confidence < threshold:
        raise RequestError(
            "OCR.LOW_CONFIDENCE",
            "recognized text is below minimum_confidence",
            data={"confidence": confidence, "minimum_confidence": threshold},
        )
    matches = _matches(text, patterns, lines, line_words, deadline)
    result = {
        "provider": "tesseract",
        "version": version,
        "source": source,
        "source_region": region,
        "text": text,
        "confidence": confidence,
        "lines": lines,
        "matches": matches,
    }
    _remaining(deadline)
    if len(
        json.dumps(result, ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode("utf-8")
    ) > MAX_NDJSON_BYTES - 256:
        raise RequestError(
            "OCR.OUTPUT_INVALID",
            "OCR result exceeded the provider NDJSON limit",
            data={"limit_bytes": MAX_NDJSON_BYTES},
        )
    return result


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
        if not isinstance(request, dict):
            raise RequestError("PROTOCOL.INVALID_REQUEST", "request must be an object")
        request_id = request.get("id")
        if request.get("type") == "manifest":
            emit_result(request_id, MANIFEST)
            return
        action = request.get("action")
        if request.get("type") != "invoke" or action != ACTION_ID:
            raise RequestError(
                "PROTOCOL.ACTION_NOT_FOUND",
                f"unknown action: {action}",
                data={"action": action, "availableActions": [ACTION_ID]},
            )
        emit_result(request_id, recognize(request.get("args"), request.get("deadline_ms")))
    except RequestError as exc:
        debug(f"request id={request_id!r} failed code={exc.code}")
        emit_error(request_id, exc)
    except Exception as exc:
        debug(f"internal error id={request_id!r}: {type(exc).__name__}: {exc}")
        emit_error(
            request_id,
            RequestError("PLUGIN.INTERNAL", "OCR provider encountered an internal error"),
        )


def main() -> int:
    for stream in (sys.stdin, sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is not None:
            reconfigure(encoding="utf-8", errors="strict")
    if "--manifest" in sys.argv[1:]:
        emit({"type": "manifest", "manifest": MANIFEST})
    debug(f"started pid={os.getpid()} action={ACTION_ID}")
    for raw_line in sys.stdin:
        line = raw_line.strip()
        if line:
            handle_line(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
