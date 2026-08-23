"""Subprocess tests for the JSON command-line contract."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]


def descriptor(*steps: dict[str, object]) -> dict[str, object]:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "cli-contract"},
        "budgets": {"max_duration": "5s", "max_executed_steps": 20},
        "steps": list(steps),
    }


class CliTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)

    def write_descriptor(self, value: object) -> Path:
        path = self.directory / "workflow.json"
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def invoke(self, *arguments: str) -> tuple[subprocess.CompletedProcess[str], object]:
        environment = os.environ.copy()
        source_path = str(PROJECT_ROOT / "src")
        existing = environment.get("PYTHONPATH")
        environment["PYTHONPATH"] = (
            source_path if not existing else os.pathsep.join((source_path, existing))
        )
        completed = subprocess.run(
            [sys.executable, "-m", "ai_auto_desktop", *arguments],
            cwd=PROJECT_ROOT,
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
            check=False,
        )
        lines = completed.stdout.splitlines()
        self.assertEqual(len(lines), 1, completed.stdout)
        return completed, json.loads(lines[0])

    def test_validate_prints_one_structured_success_document(self) -> None:
        path = self.write_descriptor(
            descriptor({"id": "finish", "type": "return", "value": None})
        )

        completed, payload = self.invoke("validate", str(path))

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            payload,
            {
                "status": "valid",
                "workflow": "cli-contract",
                "apiVersion": "ai-auto-desktop.dev/v1alpha1",
                "steps": 1,
            },
        )

    def test_run_prints_structured_result_and_parses_json_input(self) -> None:
        value = descriptor(
            {
                "id": "finish",
                "type": "return",
                "value": "${{ inputs.payload }}",
            }
        )
        value["inputs"] = {
            "payload": {"schema": {"type": "object"}, "required": True}
        }
        path = self.write_descriptor(value)

        completed, payload = self.invoke(
            "run", str(path), "--input", 'payload={"answer":42}'
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(payload["status"], "succeeded")
        self.assertEqual(payload["output"], {"answer": 42})
        self.assertIsNone(payload["error"])

    def test_invalid_descriptor_is_a_structured_nonzero_error(self) -> None:
        invalid = descriptor({"id": "finish", "type": "return"})
        invalid["unexpected"] = True
        path = self.write_descriptor(invalid)

        completed, payload = self.invoke("validate", str(path))

        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(payload["status"], "error")
        self.assertEqual(payload["error"]["code"], "DESCRIPTOR.INVALID")
        self.assertEqual(payload["error"]["schema_version"], "1")
        self.assertTrue(payload["error"]["details"]["issues"])

    def test_cli_argument_error_is_structured_and_nonzero(self) -> None:
        path = self.write_descriptor(
            descriptor({"id": "finish", "type": "return"})
        )

        completed, payload = self.invoke(
            "run", str(path), "--input", "missing_equals"
        )

        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(payload["status"], "error")
        self.assertEqual(payload["error"]["code"], "CLI.INVALID_ARGUMENT")
        self.assertEqual(payload["error"]["category"], "cli")

    def test_invalid_plugin_command_quoting_is_a_cli_error(self) -> None:
        path = self.write_descriptor(
            descriptor({"id": "finish", "type": "return"})
        )

        completed, payload = self.invoke(
            "run", str(path), "--plugin", "fixture='unterminated"
        )

        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(payload["error"]["code"], "CLI.INVALID_ARGUMENT")


if __name__ == "__main__":
    unittest.main()
