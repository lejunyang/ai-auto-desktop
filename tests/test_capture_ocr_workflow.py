"""End-to-end runtime contract for explicit capture-to-OCR decisions."""

from __future__ import annotations

import base64
import json
import os
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest

from ai_auto_desktop.artifacts import ArtifactStore
from ai_auto_desktop.compiler import load_descriptor
from ai_auto_desktop.plugin import ProcessPlugin
from ai_auto_desktop.runtime import WorkflowRunner


PROJECT_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = PROJECT_ROOT / "examples/workflows/linux-capture-ocr-decision.yaml"
OCR_PLUGIN = PROJECT_ROOT / "plugins/ocr_tesseract/ocr_tesseract_plugin.py"
PNG_1X1 = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
)
TSV = (
    "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n"
    "5\t1\t1\t1\t1\t1\t0\t0\t40\t12\t96.0\tREADY\n"
)


class CaptureFixturePlugin(ProcessPlugin):
    """Trusted in-process capture stub; native pixels are tested separately."""

    def __init__(self, payload: bytes) -> None:
        super().__init__(["unused"], name="desktop.linux_atspi")
        self.payload = payload
        self.calls = 0
        self.manifest = {
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "CapabilityManifest",
            "metadata": {"name": "desktop.linux_atspi", "version": "0.1.0"},
            "runtime": {"kind": "process", "protocol": "ndjson-stdio-v1", "entrypoint": "fixture", "platforms": ["linux"]},
            "actions": {
                "capture_target": {
                    "contract_major": 1,
                    "effect": {"default_class": "read_only"},
                    "risk": {"category": "observe", "level": "medium"},
                    "permissions": ["desktop.observe", "desktop.capture"],
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"},
                    "artifacts": {
                        "outputs": {
                            "frame": {
                                "pointer": "/frame",
                                "media_types": ["image/png"],
                                "max_size_bytes": 64 * 1024 * 1024,
                            }
                        }
                    },
                }
            },
        }

    def start(self, timeout: float | None = None) -> dict[str, object]:
        return self.manifest

    def invoke_with_artifacts(
        self, action, args, artifact_store, *, contract=None, timeout=None,
        result_validator=None,
    ):
        self.calls += 1
        reference = artifact_store.import_bytes(self.payload, media_type="image/png")
        result = {
            "frame": reference.to_dict(),
            "provenance": {
                "capture_method": "test_fixture",
                "target": args["target"],
                "ocr_invoked": False,
            },
        }
        if result_validator is not None:
            result_validator(result)
        return result


@unittest.skipUnless(
    sys.platform.startswith("linux"),
    "the explicit capture-to-OCR example is Linux-only",
)
class CaptureOcrWorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        directory = Path(self.temporary.name)
        engine = directory / "fake_tesseract.py"
        engine.write_text(
            textwrap.dedent(
                """
                import os, sys
                if "--version" in sys.argv:
                    print("tesseract 5.4.1")
                else:
                    sys.stdout.write(os.environ["FAKE_TESSERACT_TSV"])
                """
            ),
            encoding="utf-8",
        )
        environment = dict(os.environ)
        environment["PYTHONPATH"] = os.fspath(PROJECT_ROOT / "src")
        environment["OCR_TESSERACT_COMMAND"] = json.dumps(
            [sys.executable, str(engine)]
        )
        environment["OCR_ALLOW_UNSANDBOXED_ENGINE"] = "1"
        environment["FAKE_TESSERACT_TSV"] = TSV
        self.ocr = ProcessPlugin(
            [sys.executable, str(OCR_PLUGIN)],
            env=environment, timeout=5, name="vision.ocr",
        )
        self.addCleanup(self.ocr.close)
        self.capture = CaptureFixturePlugin(PNG_1X1)
        self.addCleanup(self.capture.close)
        self.store = ArtifactStore(temporary_parent=directory)
        self.addCleanup(self.store.cleanup)
        self.descriptor = load_descriptor(WORKFLOW)

    def run_decision(self, target_text: str):
        return WorkflowRunner(
            self.descriptor,
            plugins={
                "desktop.linux_atspi": self.capture,
                "vision.ocr": self.ocr,
            },
            granted_permissions=["desktop.observe", "desktop.capture"],
            artifact_store=self.store,
        ).run(
            {
                "target": {"snapshot_id": "fixture:1", "revision": 1, "node_id": "n1"},
                "locator": {"name": "Fixture"},
                "target_text": target_text,
                "languages": ["eng"],
                "minimum_confidence": 0.8,
            }
        )

    def test_explicit_artifact_ocr_match_drives_decision_without_action(self) -> None:
        result = self.run_decision("READY")

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.output["decision"], "matched")
        self.assertEqual(result.output["text"], "READY")
        self.assertEqual(self.capture.calls, 1)
        serialized = json.dumps(result.to_dict())
        self.assertNotIn(os.fspath(self.store._root), serialized)
        self.assertNotIn("pointer_click", serialized)

    def test_no_match_returns_without_synthesizing_a_pointer_action(self) -> None:
        result = self.run_decision("MISSING")

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.output["decision"], "not_matched")
        self.assertNotIn("bounds", result.output)
        self.assertNotIn("pointer_click", json.dumps(result.to_dict()))


if __name__ == "__main__":
    unittest.main()
