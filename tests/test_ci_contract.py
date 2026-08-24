"""Static contracts for platform CI trigger policy."""

from pathlib import Path
import re
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = PROJECT_ROOT / ".github" / "workflows" / "ci.yml"


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


if __name__ == "__main__":
    unittest.main()
