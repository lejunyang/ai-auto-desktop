"""Portable contracts and native checks for the Windows artifact pipe."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import re
import sys
import time
import unittest
from unittest import mock

from ai_auto_desktop import _win_named_pipe as pipe
from ai_auto_desktop.artifact_ipc import (
    ArtifactIPCError,
    CHANNEL_HOST_PID_ENV,
    CHANNEL_PIPE_ENV,
    _open_worker_channel,
)


class WindowsPipePortableContractTests(unittest.TestCase):
    def test_pipe_name_is_single_local_random_endpoint(self) -> None:
        name = rf"\\.\pipe\aad-artifact-{os.getpid()}-{'a' * 64}"
        self.assertEqual(pipe._validate_pipe_name(name), name)
        for invalid in (
            rf"\\server\pipe\aad-artifact-{os.getpid()}-{'a' * 64}",
            rf"\\.\pipe\aad-artifact-{os.getpid()}-{'a' * 63}",
            rf"\\.\pipe\aad-artifact-{os.getpid()}-{'a' * 64}\extra",
            rf"\\.\pipe\aad-artifact-0-{'a' * 64}",
        ):
            with self.subTest(invalid=invalid), self.assertRaises(pipe.WindowsPipeError):
                pipe._validate_pipe_name(invalid)

    def test_worker_environment_is_closed_and_path_redacted(self) -> None:
        future = int((time.time() + 1) * 1000)
        with mock.patch(
            "ai_auto_desktop.artifact_ipc.os.name", "nt"
        ), self.assertRaises(ArtifactIPCError) as raised:
            _open_worker_channel(
                {
                    CHANNEL_PIPE_ENV: r"C:\private\secret",
                    CHANNEL_HOST_PID_ENV: "1",
                },
                future,
            )
        self.assertEqual(raised.exception.code, "ARTIFACT_IPC.CHANNEL_UNAVAILABLE")
        self.assertNotIn("secret", str(raised.exception))

    def test_security_descriptor_is_protected_current_user_and_system(self) -> None:
        source = Path(pipe.__file__).read_text(encoding="utf-8")
        self.assertIn('D:P(A;;GA;;;SY)(A;;GA;;;{sid})', source)
        self.assertIn("_PIPE_REJECT_REMOTE_CLIENTS", source)
        self.assertIn("_FILE_FLAG_FIRST_PIPE_INSTANCE", source)
        self.assertRegex(
            source,
            re.compile(r"GetNamedPipeClientProcessId[\s\S]+actual_pid\.value != expected_pid"),
        )
        self.assertRegex(
            source,
            re.compile(r"GetNamedPipeServerProcessId[\s\S]+actual_pid\.value != expected_pid"),
        )


@unittest.skipUnless(sys.platform == "win32", "requires native Windows named pipes")
class WindowsPipeNativeTests(unittest.TestCase):
    def test_server_and_child_worker_exchange_bytes_and_verify_pids(self) -> None:
        server = pipe.WindowsPipeServer()
        self.addCleanup(server.close)
        deadline = int((time.time() + 5) * 1000)
        script = (
            "import os,sys,time; "
            "from ai_auto_desktop._win_named_pipe import connect_worker; "
            "c=connect_worker(sys.argv[1],int(sys.argv[2]),int(sys.argv[3])); "
            "c.send_all(b'worker',int(sys.argv[3])); "
            "assert c.recv_exact(4,int(sys.argv[3]))==b'host'; c.close()"
        )
        process = subprocess.Popen(
            [sys.executable, "-c", script, server.name, str(os.getpid()), str(deadline)],
            env=dict(os.environ),
        )
        self.addCleanup(
            lambda: process.kill() if process.poll() is None else None
        )
        host = server.accept(process.pid, deadline)
        self.addCleanup(host.close)
        self.assertEqual(host.recv_exact(6, deadline), b"worker")
        host.send_all(b"host", deadline)
        self.assertEqual(process.wait(timeout=3), 0)

    def test_same_pipe_name_accepts_two_sequential_child_connections(self) -> None:
        first = pipe.WindowsPipeServer()
        self.addCleanup(first.close)
        name = first.name
        server = first
        for iteration in range(2):
            if iteration:
                server = pipe.WindowsPipeServer(name)
                self.addCleanup(server.close)
            deadline = int((time.time() + 5) * 1000)
            script = (
                "import os,sys; "
                "from ai_auto_desktop._win_named_pipe import connect_worker; "
                "c=connect_worker(sys.argv[1],int(sys.argv[2]),int(sys.argv[3])); "
                "c.send_all(b'ok',int(sys.argv[3])); c.close()"
            )
            process = subprocess.Popen(
                [
                    sys.executable, "-c", script, name, str(os.getpid()),
                    str(deadline),
                ],
                env=dict(os.environ),
            )
            channel = server.accept(process.pid, deadline)
            self.assertEqual(channel.recv_exact(2, deadline), b"ok")
            channel.close()
            self.assertEqual(process.wait(timeout=3), 0)

    def test_wrong_host_pid_is_rejected(self) -> None:
        server = pipe.WindowsPipeServer()
        self.addCleanup(server.close)
        with self.assertRaises(pipe.WindowsPipeError) as raised:
            pipe.connect_worker(
                server.name, os.getpid() + 1, int((time.time() + 1) * 1000)
            )
        self.assertEqual(raised.exception.kind, "identity")


if __name__ == "__main__":
    unittest.main()
