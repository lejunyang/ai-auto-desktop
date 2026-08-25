"""Cross-platform contracts for the fail-closed script executor."""

from __future__ import annotations

from pathlib import Path
import json
import shutil
import sys
import unittest
from unittest import mock

from ai_auto_desktop import script
from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.errors import AutomationError


def script_plan(**overrides: object) -> object:
    step: dict[str, object] = {
        "id": "compute",
        "type": "script",
        "runtime": "python",
        "source": "print('{}')\n",
        "inputs": {"value": 21},
        "output_schema": {"type": "object"},
    }
    step.update(overrides)
    return compile_descriptor(
        {
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "script-contract"},
            "budgets": {
                "max_duration": "5s",
                "max_executed_steps": 10,
                "cleanup_timeout": "1s",
            },
            "steps": [step],
        }
    )


def execute(plan: object, *, timeout: float = 1.0) -> object:
    return script.execute_python_script(
        plan,
        plan.steps[0],
        {"value": 21},
        timeout,
    )


class _CompletedProcess:
    def __init__(
        self,
        stdout: object,
        stderr: object,
        *,
        output: bytes = b'{"answer": 42}',
        diagnostics: bytes = b"",
        returncode: int = 0,
    ) -> None:
        self.pid = 12345
        self.returncode = returncode
        self._stdout = stdout
        self._stderr = stderr
        self._output = output
        self._diagnostics = diagnostics
        self.input: str | None = None
        self.timeout: float | None = None

    def communicate(self, value: str, timeout: float | None = None) -> None:
        self.input = value
        self.timeout = timeout
        self._stdout.write(self._output)
        self._stderr.write(self._diagnostics)


class ScriptAvailabilityContracts(unittest.TestCase):
    def assert_unavailable(self, plan: object) -> None:
        with self.assertRaises(AutomationError) as raised:
            execute(plan)
        self.assertEqual(raised.exception.code, "SCRIPT.SANDBOX_UNAVAILABLE")

    def test_windows_and_macos_fail_closed_before_process_start(self) -> None:
        plan = script_plan()

        for platform in ("win32", "darwin"):
            with self.subTest(platform=platform), mock.patch.object(
                script.sys, "platform", platform
            ), mock.patch.object(
                script.shutil, "which", return_value="/mock/tool"
            ), mock.patch.object(
                script.Path, "is_file", return_value=True
            ), mock.patch.object(script.subprocess, "Popen") as popen:
                self.assert_unavailable(plan)
                popen.assert_not_called()

    def test_linux_without_each_required_tool_fails_closed(self) -> None:
        plan = script_plan()

        for missing in ("bwrap", "prlimit"):
            with self.subTest(missing=missing), mock.patch.object(
                script.sys, "platform", "linux"
            ), mock.patch.object(
                script.shutil,
                "which",
                side_effect=lambda name, missing=missing: (
                    None if name == missing else f"/usr/bin/{name}"
                ),
            ), mock.patch.object(
                script.Path, "is_file", return_value=True
            ), mock.patch.object(script.subprocess, "Popen") as popen:
                self.assert_unavailable(plan)
                popen.assert_not_called()

    def test_current_host_reports_unavailable_without_prerequisites(self) -> None:
        prerequisites = (
            sys.platform.startswith("linux")
            and shutil.which("bwrap") is not None
            and shutil.which("prlimit") is not None
            and Path("/usr/bin/python3").is_file()
        )
        if prerequisites:
            self.skipTest("host has the Linux sandbox prerequisites")

        with mock.patch.object(script.subprocess, "Popen") as popen:
            self.assert_unavailable(script_plan())
            popen.assert_not_called()


class ScriptPolicyContracts(unittest.TestCase):
    def test_unsupported_policy_requests_are_rejected_before_process_start(self) -> None:
        requests = {
            "capability": {"capabilities": ["desktop.observe"]},
            "network": {
                "sandbox": {
                    "network": {
                        "mode": "allowlist",
                        "hosts": ["example.com"],
                    }
                }
            },
            "filesystem": {
                "sandbox": {"filesystem": {"mode": "read_only"}}
            },
            "environment": {
                "sandbox": {
                    "environment": {
                        "mode": "allowlist",
                        "names": ["LANG"],
                    }
                }
            },
        }

        for name, request in requests.items():
            with self.subTest(request=name), mock.patch.object(
                script.sys, "platform", "win32"
            ), mock.patch.object(
                script.shutil,
                "which",
                side_effect=AssertionError("policy must precede tool probing"),
            ), mock.patch.object(script.subprocess, "Popen") as popen:
                with self.assertRaises(AutomationError) as raised:
                    execute(script_plan(**request))
                self.assertEqual(raised.exception.code, "DESCRIPTOR.INVALID")
                popen.assert_not_called()

    def test_unsupported_runtime_is_rejected_before_process_start(self) -> None:
        for runtime in ("javascript", "shell"):
            with self.subTest(runtime=runtime), mock.patch.object(
                script.sys, "platform", "win32"
            ), mock.patch.object(
                script.shutil,
                "which",
                side_effect=AssertionError("policy must precede tool probing"),
            ), mock.patch.object(script.subprocess, "Popen") as popen:
                with self.assertRaises(AutomationError) as raised:
                    execute(script_plan(runtime=runtime))
                self.assertEqual(
                    raised.exception.code, "DESCRIPTOR.INVALID"
                )
                popen.assert_not_called()


class LinuxScriptLaunchContracts(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux sandbox smoke")
    def test_real_sandbox_hides_host_files_and_environment(self) -> None:
        if shutil.which("bwrap") is None or shutil.which("prlimit") is None:
            self.skipTest("bubblewrap and prlimit are required")
        source = (
            "import json, os, sys\n"
            "payload = json.load(sys.stdin)\n"
            "def readable(path):\n"
            "    try:\n"
            "        open(path, encoding='utf-8').read(1)\n"
            "        return True\n"
            "    except OSError:\n"
            "        return False\n"
            "json.dump({'answer': payload['value'] * 2, "
            "'passwd_visible': readable('/etc/passwd'), "
            "'home_visible': readable('/data00/home/lejunyang/.ssh/id_rsa'), "
            "'environment_keys': sorted(os.environ)}, sys.stdout)\n"
        )
        plan = script_plan(source=source)

        with mock.patch.dict(
            script.os.environ, {"AAD_HOST_SECRET": "must-not-cross"}
        ):
            result = execute(plan, timeout=5.0)

        self.assertEqual(result["answer"], 42)
        self.assertFalse(result["passwd_visible"])
        self.assertFalse(result["home_visible"])
        self.assertNotIn("AAD_HOST_SECRET", result["environment_keys"])
        self.assertLessEqual(
            set(result["environment_keys"]),
            {"PATH", "PWD", "PYTHONIOENCODING", "LC_CTYPE"},
        )

    def run_with_fake_sandbox(
        self,
        *,
        output: bytes = b'{"answer": 42}',
        diagnostics: bytes = b"",
        returncode: int = 0,
    ) -> tuple[object, mock.Mock, _CompletedProcess]:
        launched: list[_CompletedProcess] = []

        def popen_factory(command: object, **options: object) -> _CompletedProcess:
            process = _CompletedProcess(
                options["stdout"],
                options["stderr"],
                output=output,
                diagnostics=diagnostics,
                returncode=returncode,
            )
            launched.append(process)
            return process

        popen = mock.Mock(side_effect=popen_factory)
        with mock.patch.object(
            script.sys, "platform", "linux"
        ), mock.patch.object(
            script.shutil,
            "which",
            side_effect=lambda name: f"/usr/bin/{name}",
        ), mock.patch.object(
            script.Path, "is_file", return_value=True
        ), mock.patch.object(
            script.subprocess, "Popen", popen
        ):
            result = execute(script_plan())

        self.assertEqual(len(launched), 1)
        return result, popen, launched[0]

    def test_linux_with_required_tools_launches_the_deny_by_default_sandbox(
        self,
    ) -> None:
        result, popen, process = self.run_with_fake_sandbox()

        self.assertEqual(result, {"answer": 42})
        self.assertEqual(process.input, '{"value": 21}')
        self.assertEqual(process.timeout, 1.0)
        popen.assert_called_once()
        command = popen.call_args.args[0]
        self.assertEqual(command[0], "/usr/bin/bwrap")
        for required in (
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "--clearenv",
            "--proc",
            "--tmpfs",
            "--ro-bind",
            "/usr/bin/prlimit",
            "-I",
        ):
            self.assertIn(required, command)
        self.assertIn("/usr/bin/prlimit", command)
        self.assertEqual(popen.call_args.kwargs["env"].keys(), {"PATH"})
        self.assertIs(popen.call_args.kwargs["start_new_session"], True)

    def test_linux_sandbox_rejects_multiple_json_values(self) -> None:
        with self.assertRaises(AutomationError) as raised:
            self.run_with_fake_sandbox(
                output=b'{"answer": 1}\n{"answer": 2}\n'
            )

        self.assertEqual(raised.exception.code, "SCRIPT.OUTPUT_INVALID")

    def test_linux_sandbox_nonzero_exit_is_structured(self) -> None:
        with self.assertRaises(AutomationError) as raised:
            self.run_with_fake_sandbox(
                diagnostics=b"sandbox setup denied", returncode=1
            )

        self.assertEqual(raised.exception.code, "SCRIPT.EXIT_NONZERO")
        self.assertEqual(raised.exception.details["returncode"], 1)
        self.assertEqual(raised.exception.details["stderr"], "sandbox setup denied")


if __name__ == "__main__":
    unittest.main()
