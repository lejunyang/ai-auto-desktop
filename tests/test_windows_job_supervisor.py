"""Portable contracts and native checks for the Windows Job Object supervisor.

The portable tests run everywhere and only cover validation and fail-closed
behaviour.  The native tests require a real Windows kernel because a Job Object
process-tree kill cannot be simulated: the point of the feature is that the OS
reclaims descendants the host never saw.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import unittest

from ai_auto_desktop import _win_job as job
from ai_auto_desktop.plugin import ProcessPlugin


WINDOWS = sys.platform == "win32"

# A worker that spawns a long-lived grandchild the host never learns about, then
# reports both PIDs.  Nothing here writes to stdout except protocol frames.
TREE_WORKER = """
import json, os, subprocess, sys, time

grandchild = subprocess.Popen(
    [sys.executable, "-c", "import time; time.sleep(600)"]
)
sys.stdout.write(json.dumps({
    "type": "manifest",
    "manifest": {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": "jobtree", "version": "0.0.1"},
        "actions": {
            "identify": {
                "contract_major": 1,
                "effect": {"default_class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
                "input_schema": {"type": "object"},
                "output_schema": {"type": "object"},
            }
        },
    },
}) + "\\n")
sys.stdout.flush()
for line in sys.stdin:
    try:
        message = json.loads(line)
    except Exception:
        continue
    if message.get("type") == "invoke":
        sys.stdout.write(json.dumps({
            "id": message["id"],
            "result": {
                "worker_pid": os.getpid(),
                "grandchild_pid": grandchild.pid,
            },
        }) + "\\n")
        sys.stdout.flush()
time.sleep(600)
"""


class WindowsJobPortableContractTests(unittest.TestCase):
    """Validation that must hold on every platform."""

    def test_pid_validation_rejects_out_of_range_and_non_integers(self) -> None:
        for invalid in (0, -1, True, False, 1 << 32, "1", 1.0, None):
            with self.subTest(invalid=invalid):
                with self.assertRaises(job.WindowsJobError):
                    job._validate_pid(invalid)
        self.assertEqual(job._validate_pid(1), 1)
        self.assertEqual(job._validate_pid(0xFFFFFFFF), 0xFFFFFFFF)

    def test_error_exposes_a_stable_kind_without_paths(self) -> None:
        for kind in ("unavailable", "unsupported", "resume"):
            error = job.WindowsJobError(kind)
            self.assertEqual(error.kind, kind)
            self.assertEqual(str(error), kind)

    @unittest.skipIf(WINDOWS, "covers the non-Windows guard")
    def test_native_entry_points_fail_closed_off_windows(self) -> None:
        with self.assertRaises(job.WindowsJobError) as raised:
            job.WindowsJob()
        self.assertEqual(raised.exception.kind, "unavailable")
        for call in (
            lambda: job.resume_process(os.getpid()),
            lambda: job.process_is_running(os.getpid()),
        ):
            with self.assertRaises(job.WindowsJobError) as raised:
                call()
            self.assertEqual(raised.exception.kind, "unavailable")

    def test_job_limit_flags_kill_on_close_without_breakaway(self) -> None:
        # Breakaway would let a worker place a descendant outside the job, which
        # would silently defeat the process-tree guarantee.  Assert on the flag
        # value actually written into LimitFlags rather than on source text.
        self.assertEqual(job._JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, 0x00002000)
        self.assertFalse(
            hasattr(job, "_JOB_OBJECT_LIMIT_BREAKAWAY_OK"),
            "breakaway must not be part of the supervisor's vocabulary",
        )
        self.assertFalse(
            hasattr(job, "_JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK"),
            "silent breakaway must not be part of the supervisor's vocabulary",
        )


@unittest.skipUnless(WINDOWS, "requires a real Windows kernel")
class NativeWindowsJobTests(unittest.TestCase):
    """Real Job Object behaviour; no part of this can be mocked usefully."""

    def _spawn_suspended(self, code: str) -> subprocess.Popen[bytes]:
        process = subprocess.Popen(
            [sys.executable, "-c", code],
            stdout=subprocess.PIPE,
            creationflags=0x00000004,  # CREATE_SUSPENDED
        )
        self.addCleanup(self._reap, process)
        return process

    @staticmethod
    def _reap(process: subprocess.Popen[bytes]) -> None:
        try:
            process.kill()
        except OSError:
            pass
        try:
            process.wait(timeout=5)
        except (subprocess.TimeoutExpired, OSError):
            pass
        if process.stdout is not None:
            process.stdout.close()

    @staticmethod
    def _wait_until_gone(pids: tuple[int, ...], timeout: float = 5.0) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not any(job.process_is_running(pid) for pid in pids):
                return True
            time.sleep(0.02)
        return False

    def test_process_is_running_tracks_real_lifetime(self) -> None:
        self.assertTrue(job.process_is_running(os.getpid()))
        finished = subprocess.Popen([sys.executable, "-c", "pass"])
        finished.wait(timeout=30)
        self.assertFalse(job.process_is_running(finished.pid))

    def test_assignment_happens_before_the_worker_can_run(self) -> None:
        process = self._spawn_suspended("import time; time.sleep(600)")
        with job.WindowsJob() as supervisor:
            self.assertIsNone(supervisor.assigned_pid)
            supervisor.assign(process.pid)
            self.assertEqual(supervisor.assigned_pid, process.pid)
            # Still suspended: it has had no chance to spawn a descendant.
            supervisor.resume(process.pid)
            self.assertTrue(job.process_is_running(process.pid))
            self.assertTrue(supervisor.terminate())
            self.assertTrue(self._wait_until_gone((process.pid,)))

    def test_terminate_reclaims_a_grandchild_the_host_never_saw(self) -> None:
        code = (
            "import subprocess, sys, time;"
            "p = subprocess.Popen([sys.executable, '-c',"
            " 'import time; time.sleep(600)']);"
            "print(p.pid, flush=True);"
            "time.sleep(600)"
        )
        process = self._spawn_suspended(code)
        with job.WindowsJob() as supervisor:
            supervisor.assign(process.pid)
            supervisor.resume(process.pid)
            assert process.stdout is not None
            grandchild_pid = int(process.stdout.readline().strip())
            self.addCleanup(
                subprocess.run,
                ["taskkill", "/F", "/PID", str(grandchild_pid)],
                capture_output=True,
            )
            self.assertTrue(job.process_is_running(grandchild_pid))
            supervisor.terminate()
            self.assertTrue(
                self._wait_until_gone((process.pid, grandchild_pid)),
                "TerminateJobObject must reclaim the whole tree",
            )

    def test_closing_the_job_handle_reclaims_survivors(self) -> None:
        # KILL_ON_JOB_CLOSE is what protects against a host that dies without
        # unwinding, so closing the handle alone must reclaim the tree.
        process = self._spawn_suspended("import time; time.sleep(600)")
        supervisor = job.WindowsJob()
        supervisor.assign(process.pid)
        supervisor.resume(process.pid)
        self.assertTrue(job.process_is_running(process.pid))
        supervisor.close()
        self.assertTrue(self._wait_until_gone((process.pid,)))

    def test_terminate_and_close_are_idempotent(self) -> None:
        process = self._spawn_suspended("import time; time.sleep(600)")
        supervisor = job.WindowsJob()
        supervisor.assign(process.pid)
        supervisor.resume(process.pid)
        self.assertTrue(supervisor.terminate())
        supervisor.close()
        # After close the handle is gone, so further calls must report that
        # they did nothing rather than raising or reusing a stale handle.
        self.assertFalse(supervisor.terminate())
        supervisor.close()

    def test_resume_rejects_a_process_that_does_not_exist(self) -> None:
        finished = subprocess.Popen([sys.executable, "-c", "pass"])
        finished.wait(timeout=30)
        with self.assertRaises(job.WindowsJobError) as raised:
            job.resume_process(finished.pid)
        self.assertEqual(raised.exception.kind, "resume")


@unittest.skipUnless(WINDOWS, "requires a real Windows kernel")
class NativeProcessPluginTreeTests(unittest.TestCase):
    """The host must reclaim a worker's descendants when it closes it."""

    def test_close_reclaims_the_worker_and_its_grandchild(self) -> None:
        plugin = ProcessPlugin(
            [sys.executable, "-c", TREE_WORKER], timeout=30.0, name="jobtree"
        )
        self.addCleanup(plugin.close)
        manifest = plugin.start()
        self.assertEqual(manifest["metadata"]["name"], "jobtree")
        self.assertTrue(
            plugin.process_tree_bounded,
            "a supported Windows kernel must bound the worker tree",
        )

        identity = plugin.invoke("identify", {})
        worker_pid = identity["worker_pid"]
        grandchild_pid = identity["grandchild_pid"]
        self.addCleanup(
            subprocess.run,
            ["taskkill", "/F", "/PID", str(grandchild_pid)],
            capture_output=True,
        )
        self.assertEqual(worker_pid, plugin.pid)
        self.assertTrue(job.process_is_running(worker_pid))
        self.assertTrue(job.process_is_running(grandchild_pid))

        plugin.close()

        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            if not (
                job.process_is_running(worker_pid)
                or job.process_is_running(grandchild_pid)
            ):
                break
            time.sleep(0.02)
        self.assertFalse(
            job.process_is_running(worker_pid), "worker survived host close"
        )
        self.assertFalse(
            job.process_is_running(grandchild_pid),
            "grandchild survived host close; the tree was not bounded",
        )

    def test_process_tree_bounded_is_false_before_start(self) -> None:
        plugin = ProcessPlugin(
            [sys.executable, "-c", "pass"], timeout=10.0, name="unstarted"
        )
        self.addCleanup(plugin.close)
        self.assertFalse(plugin.process_tree_bounded)


if __name__ == "__main__":
    unittest.main()
