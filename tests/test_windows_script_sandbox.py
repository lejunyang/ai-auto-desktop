"""Contracts for the Windows script sandbox.

The portable tests run everywhere and cover the platform-independent contract:
availability reporting, fail-closed behaviour off-Windows, and limit validation.
The native tests require a real Windows kernel because the whole point of the
feature is that the OS -- not Python -- enforces the caps: a memory ceiling or a
process-tree kill cannot be simulated.
"""

from __future__ import annotations

import json
import sys
import time
import unittest

from ai_auto_desktop import _win_job as job
from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.errors import AutomationError
from ai_auto_desktop.script import (
    execute_python_script,
    sandbox_availability,
    validate_script_policy,
)


WINDOWS = sys.platform == "win32"
LINUX = sys.platform.startswith("linux")


def _workflow(source: str, *, max_output_bytes: int | None = None) -> tuple:
    sandbox: dict = {
        "network": {"mode": "deny"},
        "filesystem": {"mode": "deny"},
        "environment": {"mode": "deny"},
    }
    if max_output_bytes is not None:
        sandbox["max_output_bytes"] = max_output_bytes
    descriptor = compile_descriptor(
        {
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "script-sandbox-test", "version": "0.1.0"},
            "budgets": {"max_duration": "5m", "max_executed_steps": 20},
            "steps": [
                {
                    "id": "compute",
                    "type": "script",
                    "runtime": "python",
                    "source": source,
                    "inputs": {},
                    "output_schema": {"type": "object"},
                    "sandbox": sandbox,
                }
            ],
        }
    )
    return descriptor, descriptor.steps[0]


class SandboxAvailabilityContractTests(unittest.TestCase):
    """The availability report is a contract the probe and docs both depend on."""

    def test_report_uses_the_probe_state_vocabulary(self) -> None:
        report = sandbox_availability()
        self.assertIn(report["state"], {"available", "degraded", "unavailable"})
        self.assertIsInstance(report["gaps"], list)
        self.assertIsInstance(report["missing"], list)

    def test_a_degraded_sandbox_must_name_its_unenforced_boundaries(self) -> None:
        report = sandbox_availability()
        if report["state"] != "degraded":
            self.skipTest("this platform's sandbox is not degraded")
        # A degraded sandbox that names no gap would be indistinguishable from
        # an available one, which is exactly the confusion this forbids.
        self.assertTrue(report["gaps"])

    def test_an_available_sandbox_must_not_claim_gaps(self) -> None:
        report = sandbox_availability()
        if report["state"] != "available":
            self.skipTest("this platform's sandbox is not fully available")
        self.assertEqual(report["gaps"], [])

    def test_the_probe_reports_the_same_state_as_the_sandbox(self) -> None:
        from ai_auto_desktop.probe import probe_capabilities

        checks = probe_capabilities().to_dict()["checks"]
        self.assertIn("script.sandbox", checks)
        self.assertEqual(
            checks["script.sandbox"]["state"], sandbox_availability()["state"]
        )

    def test_the_probe_lists_unenforced_boundaries_as_evidence(self) -> None:
        from ai_auto_desktop.probe import probe_capabilities

        checks = probe_capabilities().to_dict()["checks"]
        check = checks["script.sandbox"]
        self.assertEqual(
            list(check["evidence"]["not_enforced"]), sandbox_availability()["gaps"]
        )


class WindowsJobLimitValidationTests(unittest.TestCase):
    """Limit arguments are validated the same way on every platform."""

    def test_non_positive_and_non_integer_limits_are_rejected(self) -> None:
        for value in (0, -1, 1.5, "512", True):
            with self.subTest(value=value):
                with self.assertRaises(job.WindowsJobError):
                    job._validate_limit(value, "memory_bytes")

    def test_none_means_no_limit(self) -> None:
        self.assertIsNone(job._validate_limit(None, "memory_bytes"))

    def test_a_positive_integer_is_accepted(self) -> None:
        self.assertEqual(job._validate_limit(512, "memory_bytes"), 512)


@unittest.skipIf(WINDOWS or LINUX, "covers platforms with no sandbox at all")
class UnsupportedPlatformTests(unittest.TestCase):
    def test_script_execution_fails_closed(self) -> None:
        descriptor, step = _workflow("print('{}')\n")
        with self.assertRaises(AutomationError) as caught:
            execute_python_script(descriptor, step, {}, 10.0)
        self.assertEqual(caught.exception.code, "SCRIPT.SANDBOX_UNAVAILABLE")


@unittest.skipUnless(WINDOWS, "requires a real Windows kernel")
class WindowsScriptSandboxTests(unittest.TestCase):
    """The caps must be enforced by the OS, not by cooperation."""

    def test_a_script_receives_stdin_json_and_returns_json(self) -> None:
        descriptor, step = _workflow(
            "import json, sys\n"
            "data = json.load(sys.stdin)\n"
            "print(json.dumps({'doubled': data['n'] * 2}))\n"
        )
        self.assertEqual(
            execute_python_script(descriptor, step, {"n": 21}, 30.0), {"doubled": 42}
        )

    def test_the_environment_is_empty(self) -> None:
        descriptor, step = _workflow(
            "import json, os\nprint(json.dumps({'count': len(os.environ)}))\n"
        )
        self.assertEqual(
            execute_python_script(descriptor, step, {}, 30.0), {"count": 0}
        )

    def test_the_working_directory_is_isolated_and_empty(self) -> None:
        descriptor, step = _workflow(
            "import json, os\nprint(json.dumps({'entries': os.listdir('.')}))\n"
        )
        self.assertEqual(
            execute_python_script(descriptor, step, {}, 30.0), {"entries": []}
        )

    def test_the_interpreter_runs_isolated(self) -> None:
        descriptor, step = _workflow(
            "import json, sys\n"
            "print(json.dumps({\n"
            "  'isolated': bool(sys.flags.isolated),\n"
            "  'no_user_site': bool(sys.flags.no_user_site),\n"
            "}))\n"
        )
        result = execute_python_script(descriptor, step, {}, 30.0)
        self.assertTrue(result["isolated"])
        self.assertTrue(result["no_user_site"])

    def test_the_memory_ceiling_is_enforced_by_the_kernel(self) -> None:
        # Allocating far beyond the cap must not succeed.  The kernel refuses
        # the commit, so the interpreter dies rather than printing.
        descriptor, step = _workflow(
            "blocks = []\n"
            "for _ in range(64):\n"
            "    blocks.append(bytearray(32 * 1024 * 1024))\n"
            "print('{}')\n"
        )
        with self.assertRaises(AutomationError) as caught:
            execute_python_script(descriptor, step, {}, 60.0)
        self.assertEqual(caught.exception.code, "SCRIPT.EXIT_NONZERO")

    def test_a_wall_clock_timeout_terminates_the_script(self) -> None:
        descriptor, step = _workflow("import time\ntime.sleep(60)\nprint('{}')\n")
        with self.assertRaises(AutomationError) as caught:
            execute_python_script(descriptor, step, {}, 3.0)
        self.assertEqual(caught.exception.code, "SCRIPT.TIMEOUT")

    def test_descendants_are_reclaimed_with_the_script(self) -> None:
        # The script spawns a grandchild the host never sees.  When the step
        # returns, that grandchild must be gone; otherwise the tree escaped.
        descriptor, step = _workflow(
            "import json, subprocess, sys\n"
            "child = subprocess.Popen(\n"
            "    [sys.executable, '-I', '-c', 'import time; time.sleep(300)']\n"
            ")\n"
            "print(json.dumps({'child_pid': child.pid}))\n"
        )
        result = execute_python_script(descriptor, step, {}, 30.0)
        child_pid = result["child_pid"]
        for _ in range(40):
            if not job.process_is_running(child_pid):
                break
            time.sleep(0.25)
        self.assertFalse(
            job.process_is_running(child_pid),
            "a descendant survived the script step",
        )

    def test_stdout_that_is_not_json_is_rejected(self) -> None:
        descriptor, step = _workflow("print('definitely not json')\n")
        with self.assertRaises(AutomationError) as caught:
            execute_python_script(descriptor, step, {}, 30.0)
        self.assertEqual(caught.exception.code, "SCRIPT.OUTPUT_INVALID")

    def test_output_beyond_the_configured_limit_is_rejected(self) -> None:
        descriptor, step = _workflow(
            "import json\nprint(json.dumps({'x': 'y' * 5000}))\n",
            max_output_bytes=256,
        )
        with self.assertRaises(AutomationError) as caught:
            execute_python_script(descriptor, step, {}, 30.0)
        self.assertEqual(caught.exception.code, "SCRIPT.OUTPUT_INVALID")

    def test_a_non_zero_exit_is_surfaced_with_stderr(self) -> None:
        descriptor, step = _workflow(
            "import sys\nsys.stderr.write('boom')\nsys.exit(3)\n"
        )
        with self.assertRaises(AutomationError) as caught:
            execute_python_script(descriptor, step, {}, 30.0)
        self.assertEqual(caught.exception.code, "SCRIPT.EXIT_NONZERO")
        self.assertEqual(caught.exception.details["returncode"], 3)
        self.assertIn("boom", caught.exception.details["stderr"])

    def test_policy_is_validated_before_anything_is_executed(self) -> None:
        descriptor, step = _workflow("print('{}')\n")
        validate_script_policy(step)  # must not raise for a deny-only sandbox

    def test_the_sandbox_reports_the_interpreter_it_will_run(self) -> None:
        report = sandbox_availability()
        self.assertEqual(report["mechanism"], "windows.job_object")
        self.assertTrue(report["interpreter"])

    def test_network_and_filesystem_are_declared_unenforced(self) -> None:
        # This is a documentation-honesty test: the sandbox must not claim
        # boundaries it does not enforce.  Windows has no per-process network or
        # mount namespace, so both must appear in the gaps list.
        report = sandbox_availability()
        self.assertEqual(report["state"], "degraded")
        self.assertIn("network", report["gaps"])
        self.assertIn("filesystem", report["gaps"])


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
