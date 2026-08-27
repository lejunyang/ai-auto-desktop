"""Worker-side contracts for the framed artifact IPC v1 transport."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import time
import unittest

from ai_auto_desktop.artifact_ipc import (
    ArtifactIPCError,
    CHANNEL_FD_ENV,
    MAGIC,
    MAX_FRAME_PAYLOAD_BYTES,
    MAX_HEADER_BYTES,
    PROTOCOL,
    VERSION,
    WorkerArtifactInvocation,
    receive_frame,
    send_frame,
)


PROJECT_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_PLUGIN = PROJECT_ROOT / "plugins/fixture/fixture_plugin.py"
REQUEST_ID = "1" * 32
INPUT_TOKEN = "2" * 32
OUTPUT_TOKEN = "3" * 32
PNG = (
    b"\x89PNG\r\n\x1a\n"
    b"fixture bytes are sufficient for IPC; Host performs image validation"
)


def sha256(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def future_deadline(seconds: float = 3.0) -> int:
    return int((time.time() + seconds) * 1000)


def request(
    *,
    data: bytes = PNG,
    input_token: str = INPUT_TOKEN,
    output_token: str = OUTPUT_TOKEN,
    media_type: str = "image/png",
    deadline_ms: int | None = None,
) -> dict[str, object]:
    artifact_ref = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "ArtifactRef",
        "artifactId": "art_" + "A" * 32,
        "digest": sha256(data),
        "mediaType": media_type,
        "sizeBytes": len(data),
    }
    return {
        "type": "invoke",
        "id": REQUEST_ID,
        "action": "fixture.artifact_copy@1",
        "args": {"source": artifact_ref},
        "deadline_ms": future_deadline() if deadline_ms is None else deadline_ms,
        "host_artifacts": {
            "protocol": PROTOCOL,
            "request_id": REQUEST_ID,
            "inputs": {
                "source": {
                    "token": input_token,
                    "media_type": media_type,
                    "size_bytes": len(data),
                    "digest": sha256(data),
                }
            },
            "outputs": {
                "result": {
                    "token": output_token,
                    "media_types": [media_type],
                    "max_size_bytes": 1024 * 1024,
                }
            },
        },
    }


def input_frames(
    channel: socket.socket,
    *,
    data: bytes = PNG,
    token: str = INPUT_TOKEN,
    media_type: str = "image/png",
    digest: str | None = None,
    declared_size: int | None = None,
    deadline_ms: int | None = None,
) -> None:
    deadline = future_deadline() if deadline_ms is None else deadline_ms
    expected_digest = sha256(data) if digest is None else digest
    size = len(data) if declared_size is None else declared_size
    identity = {
        "request_id": REQUEST_ID,
        "slot": "source",
        "token": token,
    }
    send_frame(
        channel,
        {
            "type": "input_open",
            **identity,
            "media_type": media_type,
            "size_bytes": size,
            "digest": expected_digest,
        },
        deadline_ms=deadline,
    )
    send_frame(
        channel,
        {"type": "input_chunk", **identity},
        data,
        deadline_ms=deadline,
    )
    send_frame(
        channel,
        {
            "type": "input_end",
            **identity,
            "size_bytes": size,
            "digest": expected_digest,
        },
        deadline_ms=deadline,
    )
    send_frame(
        channel,
        {"type": "inputs_complete", "request_id": REQUEST_ID},
        deadline_ms=deadline,
    )


@unittest.skipUnless(hasattr(socket, "socketpair"), "socketpair is required")
class FrameCodecTests(unittest.TestCase):
    def setUp(self) -> None:
        self.left, self.right = socket.socketpair()
        self.addCleanup(self.left.close)
        self.addCleanup(self.right.close)

    def test_roundtrip_uses_bounded_json_header_and_raw_bytes(self) -> None:
        header = {
            "type": "input_chunk",
            "request_id": REQUEST_ID,
            "slot": "source",
            "token": INPUT_TOKEN,
        }
        payload = b"\x00raw-not-base64\xff"

        send_frame(self.left, header, payload, deadline_ms=future_deadline())
        received_header, received_payload = receive_frame(
            self.right, deadline_ms=future_deadline()
        )

        self.assertEqual(received_header, header)
        self.assertEqual(received_payload, payload)

    def test_rejects_bad_magic_and_oversized_lengths_before_allocation(self) -> None:
        for prefix, code in (
            (struct.pack("!4sBII", b"NOPE", VERSION, 2, 0), "ARTIFACT_IPC.BAD_PREAMBLE"),
            (struct.pack("!4sBII", MAGIC, VERSION, MAX_HEADER_BYTES + 1, 0), "ARTIFACT_IPC.HEADER_LIMIT_EXCEEDED"),
            (struct.pack("!4sBII", MAGIC, VERSION, 2, MAX_FRAME_PAYLOAD_BYTES + 1), "ARTIFACT_IPC.PAYLOAD_LIMIT_EXCEEDED"),
        ):
            with self.subTest(code=code):
                sender, receiver = socket.socketpair()
                self.addCleanup(sender.close)
                self.addCleanup(receiver.close)
                sender.sendall(prefix)
                with self.assertRaises(ArtifactIPCError) as raised:
                    receive_frame(receiver, deadline_ms=future_deadline())
                self.assertEqual(raised.exception.code, code)

    def test_rejects_truncated_header_and_payload(self) -> None:
        header = json.dumps(
            {
                "type": "input_chunk",
                "request_id": REQUEST_ID,
                "slot": "source",
                "token": INPUT_TOKEN,
            },
            separators=(",", ":"),
        ).encode()
        for wire in (
            struct.pack("!4sBII", MAGIC, VERSION, len(header), 0)[:-1],
            struct.pack("!4sBII", MAGIC, VERSION, len(header), 0) + header[:-1],
            struct.pack("!4sBII", MAGIC, VERSION, len(header), 4) + header + b"xx",
        ):
            with self.subTest(length=len(wire)):
                sender, receiver = socket.socketpair()
                sender.sendall(wire)
                sender.shutdown(socket.SHUT_WR)
                try:
                    with self.assertRaises(ArtifactIPCError) as raised:
                        receive_frame(receiver, deadline_ms=future_deadline())
                    self.assertEqual(
                        raised.exception.code, "ARTIFACT_IPC.TRUNCATED_FRAME"
                    )
                finally:
                    sender.close()
                    receiver.close()

    def test_rejects_duplicate_json_header_fields(self) -> None:
        raw_header = (
            b'{"type":"inputs_complete","type":"inputs_complete",'
            b'"request_id":"' + REQUEST_ID.encode() + b'"}'
        )
        self.left.sendall(
            struct.pack("!4sBII", MAGIC, VERSION, len(raw_header), 0) + raw_header
        )

        with self.assertRaises(ArtifactIPCError) as raised:
            receive_frame(self.right, deadline_ms=future_deadline())

        self.assertEqual(raised.exception.code, "ARTIFACT_IPC.INVALID_FRAME")

    def test_rejects_unknown_fields_and_non_chunk_payload(self) -> None:
        with self.assertRaises(ArtifactIPCError) as raised:
            send_frame(
                self.left,
                {
                    "type": "inputs_complete",
                    "request_id": REQUEST_ID,
                    "path": "/tmp/forbidden",
                },
            )
        self.assertEqual(raised.exception.code, "ARTIFACT_IPC.INVALID_FRAME")

    def test_error_completion_shape_is_closed(self) -> None:
        invalid = (
            {
                "type": "invocation_complete",
                "request_id": REQUEST_ID,
                "status": "error",
            },
            {
                "type": "invocation_complete",
                "request_id": REQUEST_ID,
                "status": "ok",
                "error": {"code": "FIXTURE.BAD", "message": "bad"},
            },
            {
                "type": "invocation_complete",
                "request_id": REQUEST_ID,
                "status": "error",
                "error": {"code": "bad", "message": "bad"},
            },
        )
        for header in invalid:
            with self.subTest(header=header):
                with self.assertRaises(ArtifactIPCError) as raised:
                    send_frame(self.left, header)
                self.assertEqual(raised.exception.code, "ARTIFACT_IPC.INVALID_FRAME")

        with self.assertRaises(ArtifactIPCError) as raised:
            send_frame(
                self.left,
                {"type": "inputs_complete", "request_id": REQUEST_ID},
                b"unexpected",
            )
        self.assertEqual(raised.exception.code, "ARTIFACT_IPC.INVALID_FRAME")

    def test_deadline_interrupts_a_waiting_read(self) -> None:
        started = time.monotonic()
        with self.assertRaises(ArtifactIPCError) as raised:
            receive_frame(self.right, deadline_ms=int((time.time() + 0.03) * 1000))
        self.assertEqual(raised.exception.code, "ARTIFACT_IPC.DEADLINE_EXCEEDED")
        self.assertLess(time.monotonic() - started, 0.5)


@unittest.skipUnless(os.name == "posix", "v1 inherited-FD transport is POSIX-only")
class WorkerInvocationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.host, self.worker = socket.socketpair()
        self.addCleanup(self.host.close)
        self.addCleanup(self.worker.close)

    def make_invocation(
        self, value: dict[str, object] | None = None
    ) -> WorkerArtifactInvocation:
        invocation = WorkerArtifactInvocation.from_request(
            request() if value is None else value,
            input_slots=("source",),
            output_slots=("result",),
            expected_action="fixture.artifact_copy@1",
            environ={CHANNEL_FD_ENV: str(self.worker.fileno())},
        )
        self.addCleanup(invocation.close)
        return invocation

    def test_reads_input_writes_output_and_completes_ok(self) -> None:
        invocation = self.make_invocation()
        ready, payload = receive_frame(self.host, deadline_ms=future_deadline())
        self.assertEqual(ready["type"], "invocation_ready")
        self.assertEqual(payload, b"")
        input_frames(self.host)

        source = invocation.read_input("source")
        placeholder = invocation.write_output(
            "result", source.data, media_type=source.media_type
        )
        invocation.complete_ok()

        self.assertEqual(source.data, PNG)
        self.assertEqual(source.digest, sha256(PNG))
        self.assertEqual(
            placeholder,
            {
                "$hostArtifact": {
                    "request_id": REQUEST_ID,
                    "slot": "result",
                    "token": OUTPUT_TOKEN,
                }
            },
        )
        headers: list[dict[str, object]] = []
        output = bytearray()
        for _ in range(5):
            header, payload = receive_frame(
                self.host, deadline_ms=future_deadline()
            )
            headers.append(header)
            output.extend(payload)
        self.assertEqual(
            [item["type"] for item in headers],
            [
                "inputs_accepted", "output_open", "output_chunk",
                "output_end", "invocation_complete",
            ],
        )
        self.assertEqual(bytes(output), PNG)
        self.assertEqual(headers[-1]["status"], "ok")
        self.assertTrue(invocation.completed)

    def test_rejects_missing_extra_duplicate_slots_and_expired_deadline(self) -> None:
        candidates: list[dict[str, object]] = []
        missing = request()
        missing["host_artifacts"]["inputs"] = {}
        candidates.append(missing)
        extra = request()
        extra["host_artifacts"]["outputs"]["extra"] = dict(
            extra["host_artifacts"]["outputs"]["result"]
        )
        candidates.append(extra)
        duplicate = request(output_token=INPUT_TOKEN)
        candidates.append(duplicate)
        expired = request(deadline_ms=int((time.time() - 1) * 1000))
        candidates.append(expired)
        open_request = request()
        open_request["path"] = "/tmp/forbidden"
        candidates.append(open_request)

        expected = (
            "ARTIFACT_IPC.INVALID_REQUEST",
            "ARTIFACT_IPC.INVALID_REQUEST",
            "ARTIFACT_IPC.INVALID_REQUEST",
            "ARTIFACT_IPC.DEADLINE_EXCEEDED",
            "ARTIFACT_IPC.INVALID_REQUEST",
        )
        for value, code in zip(candidates, expected, strict=True):
            with self.subTest(code=code):
                with self.assertRaises(ArtifactIPCError) as raised:
                    self.make_invocation(value)
                self.assertEqual(raised.exception.code, code)

    def test_malformed_media_types_and_fd_values_fail_with_stable_errors(self) -> None:
        malformed_media = request()
        malformed_media["host_artifacts"]["outputs"]["result"][
            "media_types"
        ] = [{}]
        with self.assertRaises(ArtifactIPCError) as raised:
            self.make_invocation(malformed_media)
        self.assertEqual(raised.exception.code, "ARTIFACT_IPC.INVALID_REQUEST")

        for descriptor in ("9" * 5000, "9999999999"):
            with self.subTest(descriptor=descriptor[:16]):
                with self.assertRaises(ArtifactIPCError) as raised:
                    WorkerArtifactInvocation.from_request(
                        request(),
                        input_slots=("source",),
                        output_slots=("result",),
                        expected_action="fixture.artifact_copy@1",
                        environ={CHANNEL_FD_ENV: descriptor},
                    )
                self.assertEqual(
                    raised.exception.code, "ARTIFACT_IPC.CHANNEL_UNAVAILABLE"
                )

    def test_rejects_wrong_token_media_size_and_digest(self) -> None:
        cases = []

        wrong_token = self.make_invocation()
        receive_frame(self.host, deadline_ms=future_deadline())
        cases.append((wrong_token, {"token": "9" * 32}, "ARTIFACT_IPC.TOKEN_MISMATCH"))

        for invocation, overrides, code in cases:
            identity = {
                "request_id": REQUEST_ID,
                "slot": "source",
                "token": INPUT_TOKEN,
            }
            identity.update(overrides)
            send_frame(
                self.host,
                {
                    "type": "input_open",
                    **identity,
                    "media_type": "image/png",
                    "size_bytes": len(PNG),
                    "digest": sha256(PNG),
                },
            )
            with self.assertRaises(ArtifactIPCError) as raised:
                invocation.read_input("source")
            self.assertEqual(raised.exception.code, code)

        for mode, code in (
            ("media", "ARTIFACT_IPC.METADATA_MISMATCH"),
            ("size", "ARTIFACT_IPC.METADATA_MISMATCH"),
            ("digest", "ARTIFACT_IPC.DIGEST_MISMATCH"),
        ):
            host, worker = socket.socketpair()
            self.addCleanup(host.close)
            self.addCleanup(worker.close)
            invocation = WorkerArtifactInvocation.from_request(
                request(),
                input_slots=("source",),
                output_slots=("result",),
                expected_action="fixture.artifact_copy@1",
                environ={CHANNEL_FD_ENV: str(worker.fileno())},
            )
            self.addCleanup(invocation.close)
            receive_frame(host, deadline_ms=future_deadline())
            if mode == "media":
                input_frames(host, media_type="image/jpeg")
            elif mode == "size":
                input_frames(host, declared_size=len(PNG) - 1)
            else:
                corrupted = PNG[:-1] + b"!"
                input_frames(host, data=corrupted, digest=sha256(PNG))
            with self.subTest(mode=mode):
                with self.assertRaises(ArtifactIPCError) as raised:
                    invocation.read_input("source")
                self.assertEqual(raised.exception.code, code)

    def test_rejects_output_media_and_size_before_sending(self) -> None:
        invocation = self.make_invocation()
        receive_frame(self.host, deadline_ms=future_deadline())
        input_frames(self.host)
        invocation.read_input("source")
        receive_frame(self.host, deadline_ms=future_deadline())

        for media_type, data, code in (
            ("image/jpeg", PNG, "ARTIFACT_IPC.METADATA_MISMATCH"),
            ("image/png", b"x" * (1024 * 1024 + 1), "ARTIFACT_IPC.SIZE_MISMATCH"),
        ):
            with self.subTest(code=code):
                with self.assertRaises(ArtifactIPCError) as raised:
                    invocation.write_output("result", data, media_type=media_type)
                self.assertEqual(raised.exception.code, code)

    def test_error_completion_is_closed_and_contains_no_path(self) -> None:
        value = request()
        value["host_artifacts"]["inputs"] = {}
        invocation = WorkerArtifactInvocation.from_request(
            value,
            input_slots=(),
            output_slots=("result",),
            expected_action="fixture.artifact_copy@1",
            environ={CHANNEL_FD_ENV: str(self.worker.fileno())},
        )
        self.addCleanup(invocation.close)
        receive_frame(self.host, deadline_ms=future_deadline())
        send_frame(
            self.host,
            {"type": "inputs_complete", "request_id": REQUEST_ID},
        )
        invocation.finish_inputs()
        accepted, accepted_payload = receive_frame(
            self.host, deadline_ms=future_deadline()
        )
        self.assertEqual(accepted["type"], "inputs_accepted")
        self.assertEqual(accepted_payload, b"")
        invocation.complete_error("FIXTURE.ARTIFACT_IPC", "copy failed")

        header, payload = receive_frame(self.host, deadline_ms=future_deadline())
        self.assertEqual(header["status"], "error")
        self.assertEqual(header["error"]["code"], "FIXTURE.ARTIFACT_IPC")
        self.assertEqual(payload, b"")
        self.assertNotIn("path", json.dumps(header).lower())
        with self.assertRaises(ArtifactIPCError) as raised:
            invocation.complete_ok()
        self.assertEqual(raised.exception.code, "ARTIFACT_IPC.INVALID_STATE")

    def test_error_completion_can_abort_before_inputs_are_consumed(self) -> None:
        invocation = self.make_invocation()
        receive_frame(self.host, deadline_ms=future_deadline())

        invocation.complete_error("FIXTURE.ARTIFACT_IPC", "copy failed")

        header, payload = receive_frame(self.host, deadline_ms=future_deadline())
        self.assertEqual(header["status"], "error")
        self.assertEqual(payload, b"")

    def test_host_envelope_rejects_path_fields_without_echoing_them(self) -> None:
        secret_path = "/tmp/host-secret/image.png"
        value = request()
        value["host_artifacts"]["inputs"]["source"]["path"] = secret_path

        with self.assertRaises(ArtifactIPCError) as raised:
            self.make_invocation(value)

        self.assertEqual(raised.exception.code, "ARTIFACT_IPC.INVALID_REQUEST")
        self.assertNotIn(secret_path, str(raised.exception))
        self.assertNotIn("/tmp", str(raised.exception))


@unittest.skipUnless(os.name == "posix", "fixture FD inheritance is POSIX-only")
class FixtureArtifactCopyTests(unittest.TestCase):
    @staticmethod
    def _cleanup_process(process: subprocess.Popen[bytes]) -> None:
        if process.stdin is not None and not process.stdin.closed:
            process.stdin.close()
        if process.poll() is None:
            process.kill()
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=2)
        for stream in (process.stdout, process.stderr):
            if stream is not None and not stream.closed:
                stream.close()

    def test_fixture_copies_bytes_and_returns_only_a_placeholder(self) -> None:
        host, child = socket.socketpair()
        self.addCleanup(host.close)
        self.addCleanup(child.close)
        value = request()
        environment = dict(os.environ)
        source_path = str(PROJECT_ROOT / "src")
        environment["PYTHONPATH"] = os.pathsep.join(
            filter(None, (source_path, environment.get("PYTHONPATH")))
        )
        environment[CHANNEL_FD_ENV] = str(child.fileno())
        process = subprocess.Popen(
            [sys.executable, str(FIXTURE_PLUGIN)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            pass_fds=(child.fileno(),),
        )
        self.addCleanup(self._cleanup_process, process)
        child.close()
        assert process.stdin is not None
        process.stdin.write((json.dumps(value, separators=(",", ":")) + "\n").encode())
        process.stdin.flush()
        ready, ready_payload = receive_frame(host, deadline_ms=future_deadline())
        self.assertEqual(ready["type"], "invocation_ready")
        self.assertEqual(ready_payload, b"")
        input_frames(host)

        frames = [receive_frame(host, deadline_ms=future_deadline()) for _ in range(5)]
        assert process.stdout is not None
        response_line = process.stdout.readline()
        response = json.loads(response_line)
        process.stdin.close()
        process.wait(timeout=2)
        assert process.stderr is not None
        process.stderr.read()
        process.stdout.close()
        process.stderr.close()

        output = b"".join(payload for _, payload in frames)
        self.assertEqual(output, PNG)
        self.assertEqual(
            [header["type"] for header, _ in frames],
            [
                "inputs_accepted", "output_open", "output_chunk",
                "output_end", "invocation_complete",
            ],
        )
        self.assertEqual(frames[-1][0]["status"], "ok")
        self.assertEqual(
            response,
            {
                "id": REQUEST_ID,
                "result": {
                    "result": {
                        "$hostArtifact": {
                            "request_id": REQUEST_ID,
                            "slot": "result",
                            "token": OUTPUT_TOKEN,
                        }
                    }
                },
            },
        )
        public_wire = response_line.decode()
        self.assertNotIn("path", public_wire.lower())
        self.assertNotIn(CHANNEL_FD_ENV, public_wire)
        self.assertEqual(process.returncode, 0)

    def test_fixture_reports_artifact_error_completion_before_input_completion(self) -> None:
        host, child = socket.socketpair()
        self.addCleanup(host.close)
        self.addCleanup(child.close)
        value = request()
        environment = dict(os.environ)
        source_path = str(PROJECT_ROOT / "src")
        environment["PYTHONPATH"] = os.pathsep.join(
            filter(None, (source_path, environment.get("PYTHONPATH")))
        )
        environment[CHANNEL_FD_ENV] = str(child.fileno())
        process = subprocess.Popen(
            [sys.executable, str(FIXTURE_PLUGIN)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            pass_fds=(child.fileno(),),
        )
        self.addCleanup(self._cleanup_process, process)
        child.close()
        identity = {
            "request_id": REQUEST_ID,
            "slot": "source",
            "token": "9" * 32,
        }
        send_frame(
            host,
            {
                "type": "input_open",
                **identity,
                "media_type": "image/png",
                "size_bytes": len(PNG),
                "digest": sha256(PNG),
            },
            deadline_ms=future_deadline(),
        )
        assert process.stdin is not None
        process.stdin.write(
            (json.dumps(value, separators=(",", ":")) + "\n").encode()
        )
        process.stdin.flush()

        ready, ready_payload = receive_frame(host, deadline_ms=future_deadline())
        self.assertEqual(ready["type"], "invocation_ready")
        self.assertEqual(ready_payload, b"")
        completion, payload = receive_frame(host, deadline_ms=future_deadline())
        assert process.stdout is not None
        response = json.loads(process.stdout.readline())
        process.stdin.close()
        process.wait(timeout=2)
        assert process.stderr is not None
        process.stderr.read()
        process.stdout.close()
        process.stderr.close()

        self.assertEqual(completion["type"], "invocation_complete")
        self.assertEqual(completion["status"], "error")
        self.assertEqual(completion["error"]["code"], "FIXTURE.ARTIFACT_IPC")
        self.assertEqual(payload, b"")
        self.assertEqual(response["id"], REQUEST_ID)
        self.assertEqual(response["error"]["code"], "FIXTURE.ARTIFACT_IPC")
        self.assertEqual(response["error"]["data"]["stage"], "ARTIFACT_IPC.TOKEN_MISMATCH")


if __name__ == "__main__":
    unittest.main()
