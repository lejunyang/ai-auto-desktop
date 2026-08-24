"""Wheel contract tests for the canonical JSON Schema resources."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest import mock
import zipfile

from ai_auto_desktop import compiler
from ai_auto_desktop.errors import DescriptorError
from ai_auto_desktop.plugin import PluginError, ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
PACKAGE_PREFIX = "ai_auto_desktop/schemas"
SCHEMA_PATHS = (
    "workflow/v1alpha1/workflow.schema.json",
    "capabilities/v1alpha1/capability-manifest.schema.json",
    "runtime/v1alpha1/run.schema.json",
    "runtime/v1alpha1/event.schema.json",
)


class PackageResourceTests(unittest.TestCase):
    def test_public_lifecycle_api_imports(self) -> None:
        from ai_auto_desktop import (
            DesiredState,
            DispatchState,
            JournalStore,
            RunService,
            RunStatus,
        )

        self.assertEqual(DesiredState.PAUSE.value, "pause")
        self.assertEqual(DispatchState.EFFECT_UNKNOWN.value, "effect_unknown")
        self.assertEqual(RunStatus.RUNNING.value, "running")
        self.assertTrue(callable(JournalStore))
        self.assertTrue(callable(RunService))

    @classmethod
    def setUpClass(cls) -> None:
        cls._temporary_directory = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls._temporary_directory.cleanup)
        cls.temporary_path = Path(cls._temporary_directory.name)
        wheel_directory = cls.temporary_path / "wheel"
        wheel_directory.mkdir()
        completed = subprocess.run(
            [
                sys.executable,
                "-m",
                "pip",
                "wheel",
                "--no-deps",
                "--no-build-isolation",
                "--wheel-dir",
                os.fspath(wheel_directory),
                os.fspath(PROJECT_ROOT),
            ],
            cwd=cls.temporary_path,
            capture_output=True,
            text=True,
        )
        if completed.returncode != 0:
            raise AssertionError(
                "wheel build failed:\n" + completed.stdout + completed.stderr
            )
        wheels = list(wheel_directory.glob("*.whl"))
        if len(wheels) != 1:
            raise AssertionError(f"expected one wheel, found {wheels!r}")
        cls.wheel = wheels[0]

    def test_wheel_contains_canonical_schema_resources(self) -> None:
        with zipfile.ZipFile(self.wheel) as archive:
            names = set(archive.namelist())
            for relative_path in SCHEMA_PATHS:
                packaged_path = f"{PACKAGE_PREFIX}/{relative_path}"
                self.assertIn(packaged_path, names)
                canonical_path = PROJECT_ROOT / "schemas" / relative_path
                self.assertEqual(
                    archive.read(packaged_path),
                    canonical_path.read_bytes(),
                    f"packaged {relative_path} drifted from the canonical schema",
                )

    def test_missing_schema_resources_fail_closed(self) -> None:
        workflow = {
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "missing-resource-test"},
            "budgets": {
                "max_duration": "1s",
                "max_executed_steps": 1,
            },
            "steps": [{"id": "finish", "type": "return"}],
        }
        with mock.patch.object(
            compiler.resources, "files", side_effect=FileNotFoundError
        ), self.assertRaises(DescriptorError) as raised:
            compiler.compile_descriptor(workflow)
        self.assertEqual(raised.exception.code, "DESCRIPTOR.SCHEMA_UNAVAILABLE")

        manifest = {
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "CapabilityManifest",
            "metadata": {"name": "missing-resource-test"},
            "actions": {"noop": {}},
        }
        plugin = ProcessPlugin(["unused"])
        with mock.patch(
            "ai_auto_desktop.plugin.resources.files",
            side_effect=FileNotFoundError,
        ), self.assertRaises(PluginError) as raised:
            plugin._validate_manifest(manifest)
        self.assertEqual(raised.exception.code, "PLUGIN.HOST_PROTOCOL_ERROR")
        self.assertIn("unavailable or invalid", raised.exception.message)

    def test_missing_jsonschema_dependency_fails_closed(self) -> None:
        workflow = {
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "missing-dependency-test"},
            "budgets": {
                "max_duration": "1s",
                "max_executed_steps": 1,
            },
            "steps": [{"id": "finish", "type": "return"}],
        }
        with mock.patch.object(
            compiler, "jsonschema", None
        ), self.assertRaises(DescriptorError) as raised:
            compiler.compile_descriptor(workflow)
        self.assertEqual(raised.exception.code, "DESCRIPTOR.UNSUPPORTED_FEATURE")

        plugin = ProcessPlugin(["unused"])
        with mock.patch(
            "ai_auto_desktop.plugin.jsonschema", None
        ), self.assertRaises(PluginError) as raised:
            plugin._validate_manifest({})
        self.assertEqual(raised.exception.code, "PLUGIN.HOST_PROTOCOL_ERROR")
        self.assertIn("requires jsonschema", raised.exception.message)

    def test_installed_package_uses_schemas_for_negative_validation(self) -> None:
        target = self.temporary_path / "target"
        completed = subprocess.run(
            [
                sys.executable,
                "-m",
                "pip",
                "install",
                "--no-deps",
                "--target",
                os.fspath(target),
                os.fspath(self.wheel),
            ],
            cwd=self.temporary_path,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            completed.returncode,
            0,
            "wheel installation failed:\n" + completed.stdout + completed.stderr,
        )

        validation_script = textwrap.dedent(
            f"""
            import json
            from pathlib import Path

            import ai_auto_desktop
            from ai_auto_desktop.compiler import compile_descriptor
            from ai_auto_desktop.errors import DescriptorError
            from ai_auto_desktop.plugin import PluginError, ProcessPlugin

            target = Path({os.fspath(target)!r}).resolve()
            package_file = Path(ai_auto_desktop.__file__).resolve()
            if target not in package_file.parents:
                raise AssertionError(f"import escaped target install: {{package_file}}")

            workflow = {{
                "apiVersion": "ai-auto-desktop.dev/v1alpha1",
                "kind": "Workflow",
                "metadata": {{
                    "name": "installed-resource-test",
                    "version": "not-semver",
                }},
                "budgets": {{
                    "max_duration": "1s",
                    "max_executed_steps": 1,
                }},
                "steps": [{{"id": "finish", "type": "return"}}],
            }}
            try:
                compile_descriptor(workflow)
            except DescriptorError as exc:
                if not any(issue["code"] == "schema" for issue in exc.issues):
                    raise AssertionError(exc.to_dict())
            else:
                raise AssertionError("canonical workflow schema was not enforced")

            manifest = {{
                "apiVersion": "ai-auto-desktop.dev/v1alpha1",
                "kind": "CapabilityManifest",
                "metadata": {{"name": "installed-resource-test"}},
                "actions": {{"noop": {{}}}},
            }}
            plugin = ProcessPlugin(["unused"])
            try:
                plugin._validate_manifest(manifest)
            except PluginError as exc:
                if exc.code != "PLUGIN.HOST_PROTOCOL_ERROR":
                    raise AssertionError(exc.to_dict())
                if "does not satisfy its schema" not in exc.message:
                    raise AssertionError(exc.to_dict())
            else:
                raise AssertionError("canonical manifest schema was not enforced")

            print(json.dumps({{"package_file": str(package_file)}}))
            """
        )
        environment = os.environ.copy()
        environment["PYTHONPATH"] = os.fspath(target)
        validated = subprocess.run(
            [sys.executable, "-c", validation_script],
            cwd=self.temporary_path,
            env=environment,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            validated.returncode,
            0,
            "isolated installed-package validation failed:\n"
            + validated.stdout
            + validated.stderr,
        )
        imported = Path(json.loads(validated.stdout)["package_file"])
        self.assertTrue(imported.is_relative_to(target))


if __name__ == "__main__":
    unittest.main()
