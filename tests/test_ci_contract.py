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
        self.assertNotIn("apt-get", automatic_job)
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

    def test_windows_native_job_requires_explicit_manual_input(self) -> None:
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
                r"\s+if: github\.event_name == 'workflow_dispatch' "
                r"&& inputs\.run_windows_native == true"
            ),
        )

        automatic_matrix = source.split("  windows-native:", 1)[0]
        self.assertNotIn("windows-latest", automatic_matrix)
        self.assertIn("windows-latest", source.split("  windows-native:", 1)[1])

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
        automatic_job, windows_job = source.split("  windows-native:", 1)

        for name, job in (("automatic", automatic_job), ("windows", windows_job)):
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
