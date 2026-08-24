"""Static contracts for platform CI trigger policy."""

from pathlib import Path
import re
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = PROJECT_ROOT / ".github" / "workflows" / "ci.yml"
WINDOWS_RESULT_RUNNER = PROJECT_ROOT / "tests" / "windows" / "run-native-fixture.ps1"
WINDOWS_RESULT_PATH = "artifacts/windows-native-fixture-result.json"


class CiTriggerContractTests(unittest.TestCase):
    def test_windows_native_job_requires_explicit_manual_input(self) -> None:
        source = CI_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("workflow_dispatch:", source)
        self.assertIn("run_windows_native:", source)
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
        self.assertIn("id: windows-native-fixture", windows_job)
        self.assertGreaterEqual(
            windows_job.count(
                "if: steps.windows-native-fixture.outcome == 'success'"
            ),
            2,
        )

    def test_windows_result_runner_has_stable_machine_readable_fields(self) -> None:
        source = WINDOWS_RESULT_RUNNER.read_text(encoding="utf-8")

        self.assertIn(
            "python -m unittest tests.test_windows_uia_native -v", source
        )
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
            self.assertRegex(source, rf"(?m)^\s+{field} = " )
        for test_field in ("command", "result", "exit_code"):
            self.assertRegex(source, rf"(?m)^\s+{test_field} = " )

        self.assertNotIn("GITHUB_TOKEN", source)
        self.assertNotRegex(source, r"(?i)Get-ChildItem\s+Env:")


if __name__ == "__main__":
    unittest.main()
