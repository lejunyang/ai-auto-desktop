"""Runtime lifecycle contracts for Host-managed artifact actions."""

from __future__ import annotations

import io
import os
from pathlib import Path
import sys
import tempfile
import unittest

from PIL import Image

from ai_auto_desktop.artifacts import ArtifactStore
from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.errors import AutomationError
from ai_auto_desktop.plugin import ProcessPlugin
from ai_auto_desktop.runtime import WorkflowRunner, run_descriptor


PROJECT_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_PLUGIN = PROJECT_ROOT / "plugins" / "fixture" / "fixture_plugin.py"


def png_bytes() -> bytes:
    output = io.BytesIO()
    Image.new("RGB", (3, 2), "#235789").save(output, format="PNG")
    return output.getvalue()


def artifact_workflow(reference: dict[str, object]) -> dict[str, object]:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "artifact-runtime"},
        "budgets": {
            "max_duration": "5s",
            "max_executed_steps": 10,
            "cleanup_timeout": "1s",
        },
        "requires": {
            "capabilities": [
                {"name": "fixture", "version": "^1.0.0"}
            ]
        },
        "steps": [
            {
                "id": "copy",
                "type": "action",
                "uses": "fixture.artifact_copy@1",
                "with": {"source": reference},
            },
            {
                "id": "done",
                "type": "return",
                "value": "${{ steps.copy.output }}",
            },
        ],
    }


@unittest.skipUnless(
    os.name in {"posix", "nt"}, "artifact byte-stream transport is unavailable"
)
class RuntimeArtifactTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        options = (
            {"temporary_parent": self.temporary.name}
            if os.name == "posix"
            else {}
        )
        self.store = ArtifactStore(**options)
        self.addCleanup(self.store.cleanup)
        self.plugin = ProcessPlugin(
            [sys.executable, str(FIXTURE_PLUGIN)], timeout=2, name="fixture"
        )
        self.addCleanup(self.plugin.close)

    def test_runtime_materializes_output_into_the_supplied_run_store(self) -> None:
        payload = png_bytes()
        source = self.store.import_bytes(payload)
        descriptor = compile_descriptor(artifact_workflow(source.to_dict()))

        result = run_descriptor(
            descriptor, plugins={"fixture": self.plugin}, artifact_store=self.store
        )

        self.assertTrue(result.ok, result.to_dict())
        copied = result.output["result"]
        with result.resolve_artifact(copied) as handle:
            self.assertEqual(handle.read(), payload)
        result.close()
        self.assertFalse(self.store.closed)
        with self.store.resolve(copied) as handle:
            self.assertEqual(handle.read(), payload)

    def test_foreign_input_fails_without_publishing_an_output(self) -> None:
        payload = png_bytes()
        options = (
            {"temporary_parent": self.temporary.name}
            if os.name == "posix"
            else {}
        )
        other = ArtifactStore(**options)
        self.addCleanup(other.cleanup)
        source = other.import_bytes(payload)
        descriptor = compile_descriptor(artifact_workflow(source.to_dict()))
        before = set(self.store._records)

        result = run_descriptor(
            descriptor, plugins={"fixture": self.plugin}, artifact_store=self.store
        )

        self.assertFalse(result.ok)
        self.assertEqual(result.error.code, "ARTIFACT.SCOPE_MISMATCH")
        self.assertEqual(set(self.store._records), before)

    def test_runner_owned_scope_can_import_and_result_controls_lifetime(self) -> None:
        descriptor = compile_descriptor(artifact_workflow({"pending": True}))
        runner = WorkflowRunner(descriptor, plugins={"fixture": self.plugin})
        self.addCleanup(runner.close)
        reference = runner.import_artifact_bytes(png_bytes())
        descriptor = compile_descriptor(artifact_workflow(reference))
        runner.descriptor = descriptor

        result = runner.run()

        self.assertTrue(result.ok, result.to_dict())
        self.assertIsNone(runner.artifact_store)
        copied = result.output["result"]
        with result.resolve_artifact(copied) as handle:
            self.assertEqual(handle.read(), png_bytes())
        result.close()
        with self.assertRaises(AutomationError) as raised:
            result.resolve_artifact(copied)
        self.assertEqual(raised.exception.code, "ARTIFACT.STORE_UNAVAILABLE")

    def test_durable_preflight_rejects_ephemeral_artifact_contract(self) -> None:
        source = self.store.import_bytes(png_bytes())
        descriptor = compile_descriptor(artifact_workflow(source.to_dict()))
        runner = WorkflowRunner(
            descriptor, plugins={"fixture": self.plugin},
            durable_action_mode="read_only", artifact_store=self.store,
        )
        self.addCleanup(runner.close)

        with self.assertRaises(AutomationError) as raised:
            runner.preflight_durable_action(descriptor.steps[0])
        self.assertEqual(raised.exception.code, "DURABLE.UNSUPPORTED_PLAN")
        self.assertEqual(
            raised.exception.details["reason"], "ephemeral_artifact_transport"
        )

    def test_artifact_protocol_failure_is_unknown_for_non_idempotent_action(self) -> None:
        source = self.store.import_bytes(png_bytes())
        raw = artifact_workflow(source.to_dict())
        raw["steps"][0]["effect"] = {"class": "non_idempotent"}
        descriptor = compile_descriptor(raw)
        original = self.plugin.invoke_with_artifacts

        def fail_after_dispatch(*args, **kwargs):
            from ai_auto_desktop.plugin import PluginError

            raise PluginError(
                "PLUGIN.HOST_ARTIFACT_PROTOCOL_ERROR",
                "bad output frame", details={"dispatched": True},
            )

        self.plugin.invoke_with_artifacts = fail_after_dispatch
        try:
            result = run_descriptor(
                descriptor, plugins={"fixture": self.plugin},
                artifact_store=self.store,
            )
        finally:
            self.plugin.invoke_with_artifacts = original

        self.assertEqual(result.status, "unknown_effect")
        self.assertEqual(result.error.code, "ACTION.UNKNOWN_EFFECT")

    def test_artifact_action_is_not_scheduled_in_parallel_batch(self) -> None:
        source = self.store.import_bytes(png_bytes())
        raw = artifact_workflow(source.to_dict())
        raw["budgets"]["max_concurrency"] = 2
        descriptor = compile_descriptor(raw)
        runner = WorkflowRunner(
            descriptor, plugins={"fixture": self.plugin}, artifact_store=self.store
        )
        self.addCleanup(runner.close)

        self.assertIsNone(runner._parallel_action_contract(descriptor.steps[0]))


if __name__ == "__main__":
    unittest.main()
