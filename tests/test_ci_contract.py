"""Static contracts for platform CI trigger policy."""

from pathlib import Path
import re
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = PROJECT_ROOT / ".github" / "workflows" / "ci.yml"
WINDOWS_RESULT_RUNNER = PROJECT_ROOT / "tests" / "windows" / "run-native-fixture.ps1"
WINDOWS_RESULT_PATH = "artifacts/windows-native-fixture-result.json"


class CiTriggerContractTests(unittest.TestCase):
    def test_automatic_contract_jobs_install_declared_optional_dependencies(
        self,
    ) -> None:
        source = CI_WORKFLOW.read_text(encoding="utf-8")
        automatic_job = source.split("  windows-native:", 1)[0]

        self.assertIn(
            'python -m pip install wheel "${{ matrix.install-target }}[ocr]"',
            automatic_job,
        )
        self.assertRegex(
            automatic_job,
            re.compile(
                r"- name: Install Linux X11 capture test dependencies\n"
                r"\s+if: runner\.os == 'Linux'\n"
                r"\s+run: sudo apt-get update && sudo apt-get install -y "
                r"--no-install-recommends xvfb libx11-dev pkg-config g\+\+"
            ),
        )
        self.assertNotIn("brew install", automatic_job)

    def test_macos_builds_production_helper_after_portable_contracts(self) -> None:
        source = CI_WORKFLOW.read_text(encoding="utf-8")
        automatic_job = source.split("  windows-native:", 1)[0]
        contract_command = "python -m unittest discover -s tests -v"
        production_build = "plugins/macos_ax/build.sh"
        fixture_build = "tests/macos/build.sh"

        self.assertIn(production_build, automatic_job)
        self.assertRegex(
            automatic_job,
            re.compile(
                r"- name: Build production macOS AX helper\n"
                r"\s+if: runner\.os == 'macOS'\n"
                r"\s+run: plugins/macos_ax/build\.sh"
            ),
        )
        self.assertLess(
            automatic_job.index(contract_command),
            automatic_job.index(production_build),
        )
        self.assertLess(
            automatic_job.index(production_build), automatic_job.index(fixture_build)
        )

    def test_windows_native_job_requires_explicit_opt_in(self) -> None:
        source = CI_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("workflow_dispatch:", source)
        self.assertIn("run_windows_native:", source)
        self.assertRegex(
            source,
            re.compile(
                r"run_windows_native:\n"
                r"(?:\s+[^\n]+\n)*?"
                r"\s+default: false\n"
                r"\s+type: boolean"
            ),
        )
        self.assertRegex(
            source,
            re.compile(
                r"windows-native:\n"
                r"\s+if: >-\n"
                r"\s+\(github\.event_name == 'workflow_dispatch' "
                r"&& inputs\.run_windows_native == true\) \|\|\n"
                r"\s+\(github\.event_name == 'push' "
                r"&& contains\(github\.event\.head_commit\.message, "
                r"'\[windows-native\]'\)\)"
            ),
        )
        self.assertNotIn(
            "github.event_name == 'pull_request'",
            source.split("  windows-native:", 1)[1],
        )

        automatic_matrix = source.split("  windows-contracts:", 1)[0]
        self.assertNotIn("windows-latest", automatic_matrix)
        self.assertIn("windows-latest", source.split("  windows-native:", 1)[1])

    def test_windows_contract_job_gates_native_artifact_transport_and_store(self) -> None:
        source = CI_WORKFLOW.read_text(encoding="utf-8")
        windows_contracts = source.split("  windows-contracts:", 1)[1].split(
            "  windows-native:", 1
        )[0]

        self.assertIn("runs-on: windows-latest", windows_contracts)
        self.assertIn('python -m pip install wheel ".[ocr,windows-uia]"', windows_contracts)
        for module in (
            "tests.test_artifacts",
            "tests.test_artifact_ipc",
            "tests.test_windows_artifact_pipe_contract",
            "tests.test_plugin_artifact_transport",
            "tests.test_runtime_artifacts",
            "tests.test_ocr_plugin",
            "tests.test_package_resources",
        ):
            self.assertIn(module, windows_contracts)
        self.assertNotIn("tests.test_windows_uia_native", windows_contracts)

    def test_windows_contract_job_gates_recorder_capture(self) -> None:
        # Capture runs against a fake backend and needs no UIA, so every push
        # must exercise it.  Otherwise the recorder's privacy and event-loss
        # guarantees would only be checked on the opt-in native job.
        source = CI_WORKFLOW.read_text(encoding="utf-8")
        windows_contracts = source.split("  windows-contracts:", 1)[1].split(
            "  windows-native:", 1
        )[0]

        self.assertIn("tests.test_windows_uia_capture", windows_contracts)

    def test_windows_contract_job_gates_kernel_confinement(self) -> None:
        # The Job Object supervisor, the script sandbox and the Windows probe
        # checks all assert real kernel behaviour, so a Linux run cannot cover
        # them.  They must run on the Windows job that executes on every push,
        # not only on the opt-in native fixture job.
        source = CI_WORKFLOW.read_text(encoding="utf-8")
        windows_contracts = source.split("  windows-contracts:", 1)[1].split(
            "  windows-native:", 1
        )[0]

        for module in (
            "tests.test_windows_job_supervisor",
            "tests.test_windows_script_sandbox",
            "tests.test_windows_probe",
            "tests.test_script_contracts",
            "tests.test_probe",
        ):
            self.assertIn(module, windows_contracts)

    def test_windows_result_is_uploaded_even_when_fixture_fails(self) -> None:
        source = CI_WORKFLOW.read_text(encoding="utf-8")
        windows_job = source.split("  windows-native:", 1)[1]

        runner_call = "./tests/windows/run-native-fixture.ps1"
        upload_action = "uses: actions/upload-artifact@v4"
        self.assertIn(runner_call, windows_job)
        self.assertIn(upload_action, windows_job)
        self.assertLess(windows_job.index(runner_call), windows_job.index(upload_action))
        self.assertRegex(
            windows_job,
            re.compile(
                r"- name: Upload Windows native fixture result\n"
                r"\s+if: always\(\)\n"
                r"\s+uses: actions/upload-artifact@v4"
            ),
        )
        self.assertIn(f"path: {WINDOWS_RESULT_PATH}", windows_job)
        self.assertIn("if-no-files-found: error", windows_job)
        upload_and_later_steps = windows_job.split(upload_action, 1)[1]
        for step_name in ("Validate canonical example", "Smoke capability probe"):
            self.assertRegex(
                upload_and_later_steps,
                re.compile(rf"- name: {step_name}\n\s+if: success\(\)"),
            )
        self.assertIn("id: windows-native-fixture", windows_job)

    def test_all_platform_jobs_gate_every_tracked_workflow_example(self) -> None:
        source = CI_WORKFLOW.read_text(encoding="utf-8")
        automatic_job, remainder = source.split("  windows-contracts:", 1)
        windows_contracts, windows_job = remainder.split("  windows-native:", 1)

        for name, job in (
            ("automatic", automatic_job),
            ("windows-contracts", windows_contracts),
            ("windows-native", windows_job),
        ):
            with self.subTest(job=name):
                self.assertIn("- name: Gate tracked workflow examples", job)
                self.assertIn("python -m unittest tests.test_examples -v", job)

    def test_windows_result_runner_has_stable_machine_readable_fields(self) -> None:
        source = WINDOWS_RESULT_RUNNER.read_text(encoding="utf-8")

        self.assertIn(
            "python -m unittest -v tests.test_windows_uia_native", source
        )
        self.assertNotIn("unittest discover -s tests", source)
        self.assertIn("finally {", source)
        self.assertIn("ConvertTo-Json", source)
        for field in (
            "schema_version",
            "commit_sha",
            "runner",
            "os",
            "python",
            "test",
            "timestamp",
            "status",
        ):
            self.assertRegex(source, rf"(?m)^\s+{field} = ")
        for test_field in ("command", "result", "exit_code"):
            self.assertRegex(source, rf"(?m)^\s+{test_field} = ")

        self.assertNotIn("GITHUB_TOKEN", source)
        self.assertNotRegex(source, r"(?i)Get-ChildItem\s+Env:")


if __name__ == "__main__":
    unittest.main()
