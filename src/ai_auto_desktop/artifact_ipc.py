"""Worker-side framed transport for Host-mediated artifact bytes.

The public workflow value remains a location-free ``ArtifactRef``.  A process
plugin receives the bytes for declared artifact slots over a dedicated socket
whose descriptor is inherited at process start.  This module never accepts or
returns a filesystem path.

Version 1 uses a fixed binary prefix, a small canonical JSON header, and an
optional raw payload::

    magic[4] | version:u8 | header_size:u32be | payload_size:u32be
    header[header_size] | payload[payload_size]

The helper deliberately implements a strict, single-invocation state machine.
Protocol errors leave the channel unsuitable for reuse; callers should report
an error completion when possible and then terminate the worker.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
import hashlib
import json
import os
import re
import select
import socket
import struct
import time
from types import TracebackType
from typing import Any


PROTOCOL = "aad-artifact-socket-v1"
CHANNEL_FD_ENV = "AAD_ARTIFACT_CHANNEL_FD"
MAGIC = b"AADF"
VERSION = 1
MAX_HEADER_BYTES = 4096
MAX_FRAME_PAYLOAD_BYTES = 256 * 1024
MAX_ARTIFACT_BYTES = 64 * 1024 * 1024

SUPPORTED_MEDIA_TYPES = frozenset(
    {
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/tiff",
        "image/bmp",
        "image/webp",
        "image/x-portable-anymap",
    }
)

_PREFIX = struct.Struct("!4sBII")
_REQUEST_ID = re.compile(r"[0-9a-f]{32}\Z")
_TOKEN = re.compile(r"[0-9a-f]{32}\Z")
_SLOT = re.compile(r"[A-Za-z_][A-Za-z0-9_]{0,127}\Z")
_DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
_ERROR_CODE = re.compile(r"[A-Z][A-Z0-9_]*(?:\.[A-Z][A-Z0-9_]*)+\Z")

_OPEN_TYPES = frozenset({"input_open", "output_open"})
_CHUNK_TYPES = frozenset({"input_chunk", "output_chunk"})
_END_TYPES = frozenset({"input_end", "output_end"})
_FRAME_TYPES = _OPEN_TYPES | _CHUNK_TYPES | _END_TYPES | frozenset(
    {
        "invocation_ready",
        "inputs_complete",
        "inputs_accepted",
        "invocation_complete",
    }
)


class ArtifactIPCError(RuntimeError):
    """Stable, path-redacted worker transport failure."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message

    def __str__(self) -> str:
        return f"{self.code}: {self.message}"


@dataclass(frozen=True, slots=True)
class ArtifactPayload:
    """Validated bytes received for one declared input slot."""

    data: bytes
    media_type: str
    digest: str
    size_bytes: int


@dataclass(frozen=True, slots=True)
class _InputBinding:
    token: str
    media_type: str
    size_bytes: int
    digest: str


@dataclass(frozen=True, slots=True)
class _OutputBinding:
    token: str
    media_types: tuple[str, ...]
    max_size_bytes: int


def send_frame(
    channel: socket.socket,
    header: Mapping[str, Any],
    payload: bytes | bytearray | memoryview = b"",
    *,
    deadline_ms: int | None = None,
) -> None:
    """Write one validated v1 frame without resetting the deadline."""

    raw_payload = _coerce_payload(payload)
    normalized = _validate_frame_header(header, raw_payload)
    try:
        raw_header = json.dumps(
            normalized,
            ensure_ascii=False,
            allow_nan=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    except (TypeError, ValueError):
        raise ArtifactIPCError(
            "ARTIFACT_IPC.INVALID_FRAME",
            "Artifact frame header is not JSON serializable.",
        ) from None
    if not raw_header or len(raw_header) > MAX_HEADER_BYTES:
        raise ArtifactIPCError(
            "ARTIFACT_IPC.HEADER_LIMIT_EXCEEDED",
            "Artifact frame header exceeds the protocol limit.",
        )
    prefix = _PREFIX.pack(MAGIC, VERSION, len(raw_header), len(raw_payload))
    _send_all(channel, prefix + raw_header + raw_payload, deadline_ms)


def receive_frame(
    channel: socket.socket, *, deadline_ms: int | None = None
) -> tuple[dict[str, Any], bytes]:
    """Read and validate exactly one v1 frame."""

    prefix = _receive_exact(channel, _PREFIX.size, deadline_ms)
    magic, version, header_size, payload_size = _PREFIX.unpack(prefix)
    if magic != MAGIC or version != VERSION:
        raise ArtifactIPCError(
            "ARTIFACT_IPC.BAD_PREAMBLE",
            "Artifact frame magic or version is invalid.",
        )
    if header_size < 2 or header_size > MAX_HEADER_BYTES:
        raise ArtifactIPCError(
            "ARTIFACT_IPC.HEADER_LIMIT_EXCEEDED",
            "Artifact frame header size is invalid.",
        )
    if payload_size > MAX_FRAME_PAYLOAD_BYTES:
        raise ArtifactIPCError(
            "ARTIFACT_IPC.PAYLOAD_LIMIT_EXCEEDED",
            "Artifact frame payload exceeds the protocol limit.",
        )
    raw_header = _receive_exact(channel, header_size, deadline_ms)
    try:
        header = json.loads(
            raw_header.decode("utf-8", errors="strict"),
            object_pairs_hook=_closed_json_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise ArtifactIPCError(
            "ARTIFACT_IPC.INVALID_FRAME",
            "Artifact frame header is not valid UTF-8 JSON.",
        ) from None
    payload = _receive_exact(channel, payload_size, deadline_ms)
    return _validate_frame_header(header, payload), payload


class WorkerArtifactInvocation:
    """Consume and produce artifact slots for one NDJSON invocation."""

    def __init__(
        self,
        channel: socket.socket,
        *,
        request_id: str,
        deadline_ms: int,
        inputs: Mapping[str, _InputBinding],
        outputs: Mapping[str, _OutputBinding],
    ) -> None:
        self._channel = channel
        self.request_id = request_id
        self.deadline_ms = deadline_ms
        self._inputs = dict(inputs)
        self._outputs = dict(outputs)
        self._input_order = tuple(sorted(self._inputs))
        self._output_order = tuple(sorted(self._outputs))
        self._input_index = 0
        self._output_index = 0
        self._inputs_complete = False
        self._completed = False
        self._closed = False

    @classmethod
    def from_request(
        cls,
        request: Mapping[str, Any],
        *,
        input_slots: Sequence[str],
        output_slots: Sequence[str],
        expected_action: str | None = None,
        environ: Mapping[str, str] | None = None,
    ) -> "WorkerArtifactInvocation":
        """Validate the Host envelope and duplicate its inherited socket FD."""

        request_id, deadline_ms, inputs, outputs = _parse_request_envelope(
            request,
            input_slots=input_slots,
            output_slots=output_slots,
            expected_action=expected_action,
        )
        _require_before_deadline(deadline_ms)
        environment = os.environ if environ is None else environ
        descriptor_text = environment.get(CHANNEL_FD_ENV)
        if (
            not isinstance(descriptor_text, str)
            or len(descriptor_text) > 10
            or not descriptor_text.isascii()
            or not descriptor_text.isdecimal()
        ):
            raise ArtifactIPCError(
                "ARTIFACT_IPC.CHANNEL_UNAVAILABLE",
                "Artifact side channel is unavailable.",
            )
        descriptor = int(descriptor_text, 10)
        if descriptor < 3:
            raise ArtifactIPCError(
                "ARTIFACT_IPC.CHANNEL_UNAVAILABLE",
                "Artifact side channel is unavailable.",
            )
        duplicate = -1
        channel: socket.socket | None = None
        try:
            duplicate = os.dup(descriptor)
            channel = socket.socket(fileno=duplicate)
            duplicate = -1
            if (
                channel.family != socket.AF_UNIX
                or channel.getsockopt(socket.SOL_SOCKET, socket.SO_TYPE)
                != socket.SOCK_STREAM
            ):
                raise OSError
            channel.setblocking(False)
        except (OSError, OverflowError, ValueError):
            if channel is not None:
                channel.close()
            elif duplicate >= 0:
                os.close(duplicate)
            raise ArtifactIPCError(
                "ARTIFACT_IPC.CHANNEL_UNAVAILABLE",
                "Artifact side channel is unavailable.",
            ) from None
        invocation = cls(
            channel,
            request_id=request_id,
            deadline_ms=deadline_ms,
            inputs=inputs,
            outputs=outputs,
        )
        try:
            invocation._send(
                {"type": "invocation_ready", "request_id": request_id}
            )
        except BaseException:
            invocation.close()
            raise
        return invocation

    @property
    def completed(self) -> bool:
        return self._completed

    def read_input(self, slot: str) -> ArtifactPayload:
        self._ensure_active()
        if self._inputs_complete or self._input_index >= len(self._input_order):
            self._state_error("No further artifact input is expected.")
        expected_slot = self._input_order[self._input_index]
        if slot != expected_slot:
            self._slot_error("Artifact input slot is out of order.")
        binding = self._inputs[expected_slot]
        header, payload = self._receive()
        self._require_frame(
            header, payload, "input_open", expected_slot, binding.token
        )
        if (
            header["media_type"] != binding.media_type
            or header["size_bytes"] != binding.size_bytes
            or header["digest"] != binding.digest
        ):
            self._metadata_error("Artifact input metadata does not match its binding.")

        received = bytearray()
        digest = hashlib.sha256()
        while True:
            header, payload = self._receive()
            frame_type = header["type"]
            if frame_type == "input_chunk":
                self._require_frame(
                    header, payload, "input_chunk", expected_slot, binding.token
                )
                if len(received) + len(payload) > binding.size_bytes:
                    self._size_error("Artifact input exceeds its declared size.")
                received.extend(payload)
                digest.update(payload)
                continue
            self._require_frame(
                header, payload, "input_end", expected_slot, binding.token
            )
            actual_digest = "sha256:" + digest.hexdigest()
            if (
                len(received) != binding.size_bytes
                or header["size_bytes"] != binding.size_bytes
                or header["digest"] != binding.digest
                or actual_digest != binding.digest
            ):
                self._digest_error(
                    "Artifact input size or digest verification failed."
                )
            break

        self._input_index += 1
        if self._input_index == len(self._input_order):
            self.finish_inputs()
        return ArtifactPayload(
            bytes(received), binding.media_type, binding.digest, binding.size_bytes
        )

    def read_inputs(self) -> dict[str, ArtifactPayload]:
        values = {slot: self.read_input(slot) for slot in self._input_order}
        if not self._input_order:
            self.finish_inputs()
        return values

    def finish_inputs(self) -> None:
        self._ensure_active()
        if self._inputs_complete:
            self._state_error("Artifact inputs were already completed.")
        if self._input_index != len(self._input_order):
            self._state_error("Declared artifact inputs have not all been read.")
        header, payload = self._receive()
        self._require_frame(header, payload, "inputs_complete", None, None)
        self._inputs_complete = True
        self._send(
            {"type": "inputs_accepted", "request_id": self.request_id}
        )

    def write_output(
        self,
        slot: str,
        data: bytes | bytearray | memoryview,
        *,
        media_type: str,
    ) -> dict[str, Any]:
        self._ensure_active()
        if not self._inputs_complete:
            self._state_error("Artifact inputs must complete before outputs are sent.")
        if self._output_index >= len(self._output_order):
            self._state_error("No further artifact output is expected.")
        expected_slot = self._output_order[self._output_index]
        if slot != expected_slot:
            self._slot_error("Artifact output slot is out of order.")
        binding = self._outputs[expected_slot]
        payload = _coerce_artifact_bytes(data)
        if media_type not in binding.media_types:
            self._metadata_error("Artifact output media type is not allowed.")
        if len(payload) > binding.max_size_bytes:
            self._size_error("Artifact output exceeds its declared size limit.")
        digest = "sha256:" + hashlib.sha256(payload).hexdigest()
        identity = self._identity(expected_slot, binding.token)
        metadata = {
            **identity,
            "media_type": media_type,
            "size_bytes": len(payload),
            "digest": digest,
        }
        self._send({"type": "output_open", **metadata})
        for offset in range(0, len(payload), MAX_FRAME_PAYLOAD_BYTES):
            self._send(
                {"type": "output_chunk", **identity},
                payload[offset : offset + MAX_FRAME_PAYLOAD_BYTES],
            )
        self._send(
            {
                "type": "output_end",
                **identity,
                "size_bytes": len(payload),
                "digest": digest,
            }
        )
        self._output_index += 1
        return self.placeholder(expected_slot)

    def placeholder(self, slot: str) -> dict[str, Any]:
        binding = self._outputs.get(slot)
        if binding is None:
            self._slot_error("Artifact output slot is not declared.")
        return {
            "$hostArtifact": {
                "request_id": self.request_id,
                "slot": slot,
                "token": binding.token,
            }
        }

    def complete_ok(self) -> None:
        self._ensure_active()
        if not self._inputs_complete or self._output_index != len(self._output_order):
            self._state_error("Artifact invocation cannot complete with missing slots.")
        self._send(
            {
                "type": "invocation_complete",
                "request_id": self.request_id,
                "status": "ok",
            }
        )
        self._completed = True

    def complete_error(self, code: str, message: str) -> None:
        self._ensure_active()
        if not isinstance(code, str) or _ERROR_CODE.fullmatch(code) is None:
            raise ArtifactIPCError(
                "ARTIFACT_IPC.INVALID_COMPLETION",
                "Artifact completion error code is invalid.",
            )
        if not isinstance(message, str) or not message or len(message) > 1024:
            raise ArtifactIPCError(
                "ARTIFACT_IPC.INVALID_COMPLETION",
                "Artifact completion error message is invalid.",
            )
        self._send(
            {
                "type": "invocation_complete",
                "request_id": self.request_id,
                "status": "error",
                "error": {"code": code, "message": message},
            }
        )
        self._completed = True

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self._channel.close()
        except OSError:
            pass

    def __enter__(self) -> "WorkerArtifactInvocation":
        self._ensure_active()
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()

    def _identity(self, slot: str, token: str) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "slot": slot,
            "token": token,
        }

    def _receive(self) -> tuple[dict[str, Any], bytes]:
        self._ensure_active()
        return receive_frame(self._channel, deadline_ms=self.deadline_ms)

    def _send(
        self, header: Mapping[str, Any], payload: bytes | bytearray | memoryview = b""
    ) -> None:
        self._ensure_active()
        send_frame(
            self._channel, header, payload, deadline_ms=self.deadline_ms
        )

    def _require_frame(
        self,
        header: Mapping[str, Any],
        payload: bytes,
        frame_type: str,
        slot: str | None,
        token: str | None,
    ) -> None:
        if header["type"] != frame_type:
            self._protocol_error("Artifact frame arrived in an invalid state.")
        if header["request_id"] != self.request_id:
            self._protocol_error("Artifact frame request does not match.")
        if slot is None:
            if "slot" in header or "token" in header:
                self._protocol_error("Artifact completion frame has slot metadata.")
        elif header.get("slot") != slot:
            self._slot_error("Artifact frame slot does not match.")
        elif header.get("token") != token:
            raise ArtifactIPCError(
                "ARTIFACT_IPC.TOKEN_MISMATCH",
                "Artifact frame token does not match.",
            )
        if frame_type not in _CHUNK_TYPES and payload:
            self._protocol_error("Artifact control frame contains a payload.")

    def _ensure_active(self) -> None:
        if self._closed:
            self._state_error("Artifact invocation is closed.")
        if self._completed:
            self._state_error("Artifact invocation is already complete.")
        _require_before_deadline(self.deadline_ms)

    @staticmethod
    def _protocol_error(message: str) -> None:
        raise ArtifactIPCError("ARTIFACT_IPC.PROTOCOL_ERROR", message)

    @staticmethod
    def _state_error(message: str) -> None:
        raise ArtifactIPCError("ARTIFACT_IPC.INVALID_STATE", message)

    @staticmethod
    def _slot_error(message: str) -> None:
        raise ArtifactIPCError("ARTIFACT_IPC.SLOT_MISMATCH", message)

    @staticmethod
    def _metadata_error(message: str) -> None:
        raise ArtifactIPCError("ARTIFACT_IPC.METADATA_MISMATCH", message)

    @staticmethod
    def _size_error(message: str) -> None:
        raise ArtifactIPCError("ARTIFACT_IPC.SIZE_MISMATCH", message)

    @staticmethod
    def _digest_error(message: str) -> None:
        raise ArtifactIPCError("ARTIFACT_IPC.DIGEST_MISMATCH", message)


def _parse_request_envelope(
    request: Mapping[str, Any],
    *,
    input_slots: Sequence[str],
    output_slots: Sequence[str],
    expected_action: str | None,
) -> tuple[str, int, dict[str, _InputBinding], dict[str, _OutputBinding]]:
    if (
        not isinstance(request, Mapping)
        or set(request)
        != {"type", "id", "action", "args", "deadline_ms", "host_artifacts"}
        or request.get("type") != "invoke"
    ):
        _invalid_request("Artifact invocation request is invalid.")
    request_id = request.get("id")
    if not isinstance(request_id, str) or _REQUEST_ID.fullmatch(request_id) is None:
        _invalid_request("Artifact invocation request id is invalid.")
    action = request.get("action")
    if not isinstance(action, str) or not action:
        _invalid_request("Artifact invocation action is invalid.")
    if expected_action is not None and action != expected_action:
        _invalid_request("Artifact invocation action does not match.")
    if not isinstance(request.get("args"), Mapping):
        _invalid_request("Artifact invocation args must be an object.")
    deadline_ms = request.get("deadline_ms")
    if (
        isinstance(deadline_ms, bool)
        or not isinstance(deadline_ms, int)
        or deadline_ms <= 0
        or deadline_ms > 2**63 - 1
    ):
        _invalid_request("Artifact invocation deadline is invalid.")

    envelope = request.get("host_artifacts")
    if not isinstance(envelope, Mapping) or set(envelope) != {
        "protocol",
        "request_id",
        "inputs",
        "outputs",
    }:
        _invalid_request("Host artifact envelope is invalid.")
    if envelope.get("protocol") != PROTOCOL or envelope.get("request_id") != request_id:
        _invalid_request("Host artifact envelope does not match the request.")

    expected_inputs = _normalize_slot_names(input_slots)
    expected_outputs = _normalize_slot_names(output_slots)
    if set(expected_inputs).intersection(expected_outputs):
        _invalid_request("Artifact input and output slot names overlap.")
    raw_inputs = envelope.get("inputs")
    raw_outputs = envelope.get("outputs")
    if not isinstance(raw_inputs, Mapping) or set(raw_inputs) != set(expected_inputs):
        _invalid_request("Artifact input slots do not match the worker contract.")
    if not isinstance(raw_outputs, Mapping) or set(raw_outputs) != set(expected_outputs):
        _invalid_request("Artifact output slots do not match the worker contract.")

    inputs = {name: _parse_input_binding(raw_inputs[name]) for name in expected_inputs}
    outputs = {
        name: _parse_output_binding(raw_outputs[name]) for name in expected_outputs
    }
    tokens = [binding.token for binding in inputs.values()] + [
        binding.token for binding in outputs.values()
    ]
    if len(tokens) != len(set(tokens)):
        _invalid_request("Artifact slot tokens must be unique.")
    return request_id, deadline_ms, inputs, outputs


def _parse_input_binding(value: Any) -> _InputBinding:
    if not isinstance(value, Mapping) or set(value) != {
        "token",
        "media_type",
        "size_bytes",
        "digest",
    }:
        _invalid_request("Artifact input binding is invalid.")
    token = _validated_token(value.get("token"))
    media_type = _validated_media_type(value.get("media_type"))
    size_bytes = _validated_size(value.get("size_bytes"), MAX_ARTIFACT_BYTES)
    digest = _validated_digest(value.get("digest"))
    return _InputBinding(token, media_type, size_bytes, digest)


def _parse_output_binding(value: Any) -> _OutputBinding:
    if not isinstance(value, Mapping) or set(value) != {
        "token",
        "media_types",
        "max_size_bytes",
    }:
        _invalid_request("Artifact output binding is invalid.")
    token = _validated_token(value.get("token"))
    raw_media_types = value.get("media_types")
    if (
        not isinstance(raw_media_types, list)
        or not raw_media_types
        or any(not isinstance(item, str) for item in raw_media_types)
        or len(raw_media_types) != len(set(raw_media_types))
    ):
        _invalid_request("Artifact output media types are invalid.")
    media_types = tuple(_validated_media_type(item) for item in raw_media_types)
    max_size_bytes = _validated_size(
        value.get("max_size_bytes"), MAX_ARTIFACT_BYTES
    )
    return _OutputBinding(token, media_types, max_size_bytes)


def _normalize_slot_names(values: Sequence[str]) -> tuple[str, ...]:
    if isinstance(values, (str, bytes)) or not isinstance(values, Sequence):
        _invalid_request("Artifact slot declaration is invalid.")
    result = tuple(values)
    if (
        len(result) > 32
        or any(not isinstance(item, str) for item in result)
        or len(result) != len(set(result))
    ):
        _invalid_request("Artifact slot declaration is invalid.")
    if any(_SLOT.fullmatch(item) is None for item in result):
        _invalid_request("Artifact slot declaration is invalid.")
    return tuple(sorted(result))


def _validate_frame_header(
    header: Mapping[str, Any], payload: bytes
) -> dict[str, Any]:
    if not isinstance(header, Mapping):
        _invalid_frame("Artifact frame header must be an object.")
    value = dict(header)
    frame_type = value.get("type")
    if frame_type not in _FRAME_TYPES:
        _invalid_frame("Artifact frame type is invalid.")
    request_id = value.get("request_id")
    if not isinstance(request_id, str) or _REQUEST_ID.fullmatch(request_id) is None:
        _invalid_frame("Artifact frame request id is invalid.")

    if frame_type in _OPEN_TYPES:
        required = {
            "type", "request_id", "slot", "token", "media_type",
            "size_bytes", "digest",
        }
        _require_exact_fields(value, required)
        _validated_frame_slot(value["slot"]); _validated_frame_token(value["token"])
        _validated_frame_media_type(value["media_type"]); _validated_frame_digest(value["digest"])
        _validated_frame_size(value["size_bytes"], MAX_ARTIFACT_BYTES)
        if payload:
            _invalid_frame("Artifact open frame must not contain a payload.")
    elif frame_type in _CHUNK_TYPES:
        _require_exact_fields(value, {"type", "request_id", "slot", "token"})
        _validated_frame_slot(value["slot"]); _validated_frame_token(value["token"])
        if not payload or len(payload) > MAX_FRAME_PAYLOAD_BYTES:
            _invalid_frame("Artifact chunk payload size is invalid.")
    elif frame_type in _END_TYPES:
        _require_exact_fields(
            value,
            {"type", "request_id", "slot", "token", "size_bytes", "digest"},
        )
        _validated_frame_slot(value["slot"]); _validated_frame_token(value["token"])
        _validated_frame_size(value["size_bytes"], MAX_ARTIFACT_BYTES)
        _validated_frame_digest(value["digest"])
        if payload:
            _invalid_frame("Artifact end frame must not contain a payload.")
    elif frame_type in {
        "invocation_ready", "inputs_complete", "inputs_accepted"
    }:
        _require_exact_fields(value, {"type", "request_id"})
        if payload:
            _invalid_frame("Artifact input completion must not contain a payload.")
    else:
        status = value.get("status")
        required = {"type", "request_id", "status"}
        if status == "error":
            required.add("error")
        _require_exact_fields(value, required)
        if status not in {"ok", "error"}:
            _invalid_frame("Artifact invocation completion status is invalid.")
        if status == "error":
            error = value["error"]
            if not isinstance(error, Mapping) or set(error) != {"code", "message"}:
                _invalid_frame("Artifact error completion is invalid.")
            if not isinstance(error.get("code"), str) or _ERROR_CODE.fullmatch(error["code"]) is None:
                _invalid_frame("Artifact error completion code is invalid.")
            if not isinstance(error.get("message"), str) or not error["message"] or len(error["message"]) > 1024:
                _invalid_frame("Artifact error completion message is invalid.")
        if payload:
            _invalid_frame("Artifact invocation completion must not contain a payload.")
    return value


def _require_exact_fields(value: Mapping[str, Any], fields: set[str]) -> None:
    if set(value) != fields:
        _invalid_frame("Artifact frame fields are invalid.")


def _closed_json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            _invalid_frame("Artifact frame header contains duplicate fields.")
        value[key] = item
    return value


def _reject_json_constant(value: str) -> None:
    _invalid_frame("Artifact frame header contains a non-finite number.")


def _coerce_payload(value: bytes | bytearray | memoryview) -> bytes:
    if type(value) not in (bytes, bytearray, memoryview):
        _invalid_frame("Artifact frame payload must be bytes.")
    try:
        payload = bytes(value)
    except (TypeError, ValueError):
        _invalid_frame("Artifact frame payload must be contiguous bytes.")
    if len(payload) > MAX_FRAME_PAYLOAD_BYTES:
        raise ArtifactIPCError(
            "ARTIFACT_IPC.PAYLOAD_LIMIT_EXCEEDED",
            "Artifact frame payload exceeds the protocol limit.",
        )
    return payload


def _coerce_artifact_bytes(value: bytes | bytearray | memoryview) -> bytes:
    if type(value) not in (bytes, bytearray, memoryview):
        raise ArtifactIPCError(
            "ARTIFACT_IPC.INVALID_PAYLOAD",
            "Artifact output must be bytes.",
        )
    try:
        payload = bytes(value)
    except (TypeError, ValueError):
        raise ArtifactIPCError(
            "ARTIFACT_IPC.INVALID_PAYLOAD",
            "Artifact output must be contiguous bytes.",
        ) from None
    if not payload:
        raise ArtifactIPCError(
            "ARTIFACT_IPC.SIZE_MISMATCH",
            "Artifact output must not be empty.",
        )
    return payload


def _send_all(channel: socket.socket, data: bytes, deadline_ms: int | None) -> None:
    view = memoryview(data)
    while view:
        timeout = _deadline_timeout(deadline_ms)
        try:
            _, writable, _ = select.select([], [channel], [], timeout)
        except (OSError, ValueError):
            _channel_error()
        if not writable:
            _deadline_error()
        try:
            sent = channel.send(view, getattr(socket, "MSG_DONTWAIT", 0))
        except (BlockingIOError, InterruptedError):
            continue
        except OSError:
            _channel_error()
        if sent <= 0:
            _truncated_error()
        view = view[sent:]


def _receive_exact(
    channel: socket.socket, size: int, deadline_ms: int | None
) -> bytes:
    value = bytearray()
    while len(value) < size:
        timeout = _deadline_timeout(deadline_ms)
        try:
            readable, _, _ = select.select([channel], [], [], timeout)
        except (OSError, ValueError):
            _channel_error()
        if not readable:
            _deadline_error()
        try:
            chunk = channel.recv(
                size - len(value), getattr(socket, "MSG_DONTWAIT", 0)
            )
        except (BlockingIOError, InterruptedError):
            continue
        except OSError:
            _channel_error()
        if not chunk:
            _truncated_error()
        value.extend(chunk)
    return bytes(value)


def _deadline_timeout(deadline_ms: int | None) -> float | None:
    if deadline_ms is None:
        return None
    _validate_deadline(deadline_ms)
    remaining = deadline_ms / 1000.0 - time.time()
    if remaining <= 0:
        _deadline_error()
    # Some platforms reject very large timeouts even though the epoch value is
    # syntactically valid.  Periodic re-checking preserves the one absolute
    # deadline without overflowing the underlying select implementation.
    return min(remaining, 86_400.0)


def _require_before_deadline(deadline_ms: int) -> None:
    _deadline_timeout(deadline_ms)


def _validate_deadline(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0 or value > 2**63 - 1:
        raise ArtifactIPCError(
            "ARTIFACT_IPC.INVALID_DEADLINE",
            "Artifact invocation deadline is invalid.",
        )
    return value


def _validated_frame_slot(value: Any) -> str:
    if not isinstance(value, str) or _SLOT.fullmatch(value) is None:
        _invalid_frame("Artifact frame slot is invalid.")
    return value


def _validated_frame_token(value: Any) -> str:
    if not isinstance(value, str) or _TOKEN.fullmatch(value) is None:
        _invalid_frame("Artifact frame token is invalid.")
    return value


def _validated_frame_media_type(value: Any) -> str:
    if not isinstance(value, str) or value not in SUPPORTED_MEDIA_TYPES:
        _invalid_frame("Artifact frame media type is invalid.")
    return value


def _validated_frame_size(value: Any, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 1 or value > maximum:
        _invalid_frame("Artifact frame byte size is invalid.")
    return value


def _validated_frame_digest(value: Any) -> str:
    if not isinstance(value, str) or _DIGEST.fullmatch(value) is None:
        _invalid_frame("Artifact frame digest is invalid.")
    return value


def _validated_token(value: Any) -> str:
    if not isinstance(value, str) or _TOKEN.fullmatch(value) is None:
        _invalid_request("Artifact slot token is invalid.")
    return value


def _validated_media_type(value: Any) -> str:
    if not isinstance(value, str) or value not in SUPPORTED_MEDIA_TYPES:
        _invalid_request("Artifact media type is invalid.")
    return value


def _validated_size(value: Any, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 1 or value > maximum:
        _invalid_request("Artifact byte size is invalid.")
    return value


def _validated_digest(value: Any) -> str:
    if not isinstance(value, str) or _DIGEST.fullmatch(value) is None:
        _invalid_request("Artifact digest is invalid.")
    return value


def _invalid_request(message: str) -> None:
    raise ArtifactIPCError("ARTIFACT_IPC.INVALID_REQUEST", message)


def _invalid_frame(message: str) -> None:
    raise ArtifactIPCError("ARTIFACT_IPC.INVALID_FRAME", message)


def _deadline_error() -> None:
    raise ArtifactIPCError(
        "ARTIFACT_IPC.DEADLINE_EXCEEDED",
        "Artifact IPC deadline expired.",
    )


def _channel_error() -> None:
    raise ArtifactIPCError(
        "ARTIFACT_IPC.CHANNEL_ERROR",
        "Artifact side channel failed.",
    )


def _truncated_error() -> None:
    raise ArtifactIPCError(
        "ARTIFACT_IPC.TRUNCATED_FRAME",
        "Artifact frame was truncated.",
    )


__all__ = [
    "ArtifactIPCError",
    "ArtifactPayload",
    "CHANNEL_FD_ENV",
    "MAGIC",
    "MAX_ARTIFACT_BYTES",
    "MAX_FRAME_PAYLOAD_BYTES",
    "MAX_HEADER_BYTES",
    "PROTOCOL",
    "VERSION",
    "WorkerArtifactInvocation",
    "receive_frame",
    "send_frame",
]
