"""Integration tests for the optional Tesseract OCR process plugin."""

from __future__ import annotations

import base64
from copy import deepcopy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from unittest import mock

import jsonschema
from PIL import Image
from PIL import ImageDraw
from PIL import ImageFont

from ai_auto_desktop.artifacts import ArtifactStore
from ai_auto_desktop.compiler import load_descriptor
from ai_auto_desktop.journal import (
    SensitiveDataError,
    assert_durable_descriptor_eligible,
)
from ai_auto_desktop.plugin import PluginError, ProcessPlugin
from ai_auto_desktop.runtime import WorkflowRunner


PROJECT_ROOT = Path(__file__).resolve().parents[1]
OCR_PLUGIN = PROJECT_ROOT / "plugins" / "ocr_tesseract" / "ocr_tesseract_plugin.py"
EXPLICIT_WORKFLOW = (
    PROJECT_ROOT / "examples" / "workflows" / "ocr-explicit-image-response.yaml"
)
EXPLICIT_WORKFLOW_JSON = EXPLICIT_WORKFLOW.with_suffix(".json")
PNG_1X1 = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
)
TSV = """level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
1\t1\t0\t0\t0\t0\t0\t0\t200\t80\t-1\t
5\t1\t1\t1\t1\t1\t10\t20\t40\t12\t96.0\tInvoice
5\t1\t1\t1\t1\t2\t55\t20\t50\t12\t84.0\tA-42
5\t1\t1\t1\t2\t1\t10\t42\t35\t11\t90.0\tTotal
5\t1\t1\t1\t2\t2\t50\t42\t45\t11\t80.0\t$12.50
"""


def load_ocr_module():
    spec = importlib.util.spec_from_file_location("aad_ocr_test_module", OCR_PLUGIN)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class TesseractPluginTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.image = self.directory / "input.png"
        self.image.write_bytes(PNG_1X1)
        self.fake = self.directory / "fake_tesseract.py"
        self.fake.write_text(
            textwrap.dedent(
                """
                import base64
                import hashlib
                import os
                from pathlib import Path
                import signal
                import sys
                import time

                started_log = os.environ.get("FAKE_TESSERACT_STARTED_LOG")
                if started_log:
                    with Path(started_log).open("a", encoding="utf-8") as stream:
                        stream.write(f"{os.getpid()} {' '.join(sys.argv[1:])}\\n")
                if (
                    os.name == "posix"
                    and os.environ.get("FAKE_TESSERACT_IGNORE_SIGTERM") == "1"
                ):
                    signal.signal(signal.SIGTERM, lambda _signum, _frame: None)
                if "--version" in sys.argv:
                    replacement = os.environ.get("FAKE_TESSERACT_REPLACEMENT")
                    source = os.environ.get("FAKE_TESSERACT_SOURCE")
                    if replacement and source:
                        os.replace(replacement, source)
                    delay = float(os.environ.get("FAKE_TESSERACT_VERSION_DELAY", "0"))
                    delay_once = os.environ.get("FAKE_TESSERACT_VERSION_DELAY_ONCE")
                    if delay_once:
                        marker = Path(delay_once)
                        if marker.exists():
                            delay = 0
                        else:
                            marker.write_text("started", encoding="ascii")
                    time.sleep(delay)
                    print("tesseract 5.4.1")
                    raise SystemExit(0)
                log = os.environ.get("FAKE_TESSERACT_LOG")
                if log:
                    Path(log).write_text("\\n".join(sys.argv[1:]), encoding="utf-8")
                digest_log = os.environ.get("FAKE_TESSERACT_DIGEST_LOG")
                if digest_log:
                    Path(digest_log).write_text(
                        hashlib.sha256(Path(sys.argv[1]).read_bytes()).hexdigest(),
                        encoding="ascii",
                    )
                rlimit_log = os.environ.get("FAKE_TESSERACT_RLIMIT_LOG")
                if rlimit_log:
                    import json
                    import resource
                    Path(rlimit_log).write_text(
                        json.dumps({
                            name: list(resource.getrlimit(getattr(resource, name)))
                            for name in (
                                "RLIMIT_AS", "RLIMIT_CPU", "RLIMIT_FSIZE",
                                "RLIMIT_NOFILE", "RLIMIT_NPROC",
                            )
                        }),
                        encoding="utf-8",
                    )
                code = int(os.environ.get("FAKE_TESSERACT_EXIT", "0"))
                if code:
                    print("synthetic engine failure", file=sys.stderr)
                    raise SystemExit(code)
                flood = int(os.environ.get("FAKE_TESSERACT_STDOUT_BYTES", "0"))
                if flood:
                    sys.stdout.buffer.write(b"x" * flood)
                    sys.stdout.buffer.flush()
                    time.sleep(30)
                stderr_flood = int(os.environ.get("FAKE_TESSERACT_STDERR_BYTES", "0"))
                if stderr_flood:
                    sys.stderr.buffer.write(b"x" * stderr_flood)
                    sys.stderr.buffer.flush()
                    time.sleep(30)
                raw = os.environ.get("FAKE_TESSERACT_TSV_B64")
                if raw is not None:
                    sys.stdout.buffer.write(base64.b64decode(raw))
                else:
                    sys.stdout.write(os.environ.get("FAKE_TESSERACT_TSV", ""))
                """
            ),
            encoding="utf-8",
        )

    def make_plugin(
        self,
        *,
        command: list[str] | None = None,
        env_updates: dict[str, str] | None = None,
        timeout: float = 3,
    ) -> ProcessPlugin:
        env = dict(os.environ)
        env["OCR_TESSERACT_COMMAND"] = json.dumps(
            command or [sys.executable, str(self.fake)]
        )
        env["FAKE_TESSERACT_TSV"] = TSV
        env["FAKE_TESSERACT_LOG"] = str(self.directory / "engine.log")
        env["FAKE_TESSERACT_DIGEST_LOG"] = str(self.directory / "digest.log")
        # The synthetic engine is the test-controlled process whose protocol
        # behavior is under test.  Non-Linux hosts have no prlimit equivalent,
        # so opt in explicitly instead of making the production plugin infer
        # that an unsandboxed engine is acceptable.
        env["OCR_ALLOW_UNSANDBOXED_ENGINE"] = "1"
        if env_updates:
            env.update(env_updates)
        plugin = ProcessPlugin(
            [sys.executable, str(OCR_PLUGIN)],
            env=env,
            timeout=timeout,
            name="vision.ocr",
        )
        self.addCleanup(plugin.close)
        return plugin

    def run_explicit_workflow(
        self,
        *,
        target_text: str,
        minimum_confidence: float,
        languages: list[str] | None = None,
        tsv: str = TSV,
    ):
        plugin = self.make_plugin(env_updates={"FAKE_TESSERACT_TSV": tsv})
        runner = WorkflowRunner(
            load_descriptor(EXPLICIT_WORKFLOW),
            plugins={"vision.ocr": plugin},
            granted_permissions=["filesystem.read"],
        )
        return runner.run(
            {
                "image_path": str(self.image.resolve()),
                "target_text": target_text,
                "languages": languages or ["eng"],
                "minimum_confidence": minimum_confidence,
            }
        )

    def test_manifest_declares_explicit_read_only_recognize_contract(self) -> None:
        manifest = self.make_plugin().start()

        self.assertEqual(manifest["metadata"]["name"], "vision.ocr")
        self.assertEqual(
            set(manifest["actions"]), {"recognize", "recognize_artifact"}
        )
        self.assertNotIn("permissions", manifest)
        contract = manifest["actions"]["recognize"]
        self.assertEqual(contract["contract_major"], 1)
        self.assertEqual(contract["effect"]["default_class"], "read_only")
        self.assertEqual(contract["risk"], {"category": "observe", "level": "low"})
        self.assertEqual(contract["permissions"], ["filesystem.read"])
        self.assertIn("OCR.ENGINE_UNAVAILABLE", {item["code"] for item in contract["errors"]})
        output_schema = contract["output_schema"]
        jsonschema.Draft202012Validator.check_schema(output_schema)
        source = output_schema["properties"]["source"]
        line = output_schema["properties"]["lines"]["items"]
        match = output_schema["properties"]["matches"]["items"]
        self.assertEqual(
            set(source["required"]),
            {"kind", "path", "digest", "media_type", "size_bytes"},
        )
        self.assertEqual(set(line["required"]), {"text", "confidence", "bounds"})
        self.assertEqual(
            set(match["required"]),
            {"pattern_id", "text", "span", "bounds", "confidence"},
        )
        self.assertFalse(source["additionalProperties"])
        self.assertFalse(line["additionalProperties"])
        self.assertFalse(match["additionalProperties"])

        artifact_contract = manifest["actions"]["recognize_artifact"]
        self.assertEqual(artifact_contract["contract_major"], 1)
        self.assertEqual(
            artifact_contract["effect"]["default_class"], "read_only"
        )
        self.assertNotIn("permissions", artifact_contract)
        self.assertEqual(
            artifact_contract["artifacts"],
            {
                "inputs": {
                    "source": {
                        "pointer": "/artifact",
                        "media_types": [
                            "image/png",
                            "image/jpeg",
                            "image/gif",
                            "image/tiff",
                            "image/bmp",
                            "image/webp",
                            "image/x-portable-anymap",
                        ],
                        "max_size_bytes": 64 * 1024 * 1024,
                    }
                }
            },
        )
        artifact_input = artifact_contract["input_schema"]
        self.assertEqual(artifact_input["required"], ["artifact"])
        self.assertNotIn("image", artifact_input["properties"])
        self.assertFalse(artifact_input["additionalProperties"])
        artifact_source = artifact_contract["output_schema"]["properties"][
            "source"
        ]
        self.assertEqual(
            set(artifact_source["required"]),
            {"kind", "digest", "media_type", "size_bytes"},
        )
        self.assertNotIn("path", artifact_source["properties"])
        self.assertFalse(artifact_source["additionalProperties"])
        jsonschema.Draft202012Validator.check_schema(
            artifact_contract["input_schema"]
        )
        jsonschema.Draft202012Validator.check_schema(
            artifact_contract["output_schema"]
        )

    def test_legacy_manifest_starts_without_package_import_path(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(OCR_PLUGIN), "--manifest"],
            input=b"",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={
                "PATH": os.environ.get("PATH", ""),
                "PYTHONIOENCODING": "utf-8",
            },
            timeout=2,
            check=False,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr.decode())
        message = json.loads(completed.stdout.splitlines()[0])
        self.assertEqual(message["manifest"]["metadata"]["version"], "0.1.1")

    def test_explicit_workflow_examples_are_equivalent_and_return_only(self) -> None:
        yaml_workflow = load_descriptor(EXPLICIT_WORKFLOW)
        json_workflow = load_descriptor(EXPLICIT_WORKFLOW_JSON)

        self.assertEqual(yaml_workflow.raw, json_workflow.raw)
        self.assertEqual(
            yaml_workflow.requires["permissions"], ("filesystem.read",)
        )
        actions = [
            step for step in yaml_workflow.all_steps() if step.type == "action"
        ]
        self.assertEqual(len(actions), 1)
        self.assertEqual(actions[0].params["uses"], "vision.ocr.recognize@1")
        self.assertEqual(
            actions[0].params["with"]["image"]["path"],
            "${{ inputs.image_path }}",
        )
        self.assertEqual(
            {"respond_to_match", "do_not_respond"},
            {
                step.id
                for step in yaml_workflow.all_steps()
                if step.type == "return"
            },
        )
        self.assertTrue(yaml_workflow.inputs["image_path"]["sensitive"])
        self.assertTrue(yaml_workflow.inputs["target_text"]["sensitive"])
        self.assertEqual(
            yaml_workflow.metadata["annotations"][
                "ai-auto-desktop.dev/durable-eligibility"
            ],
            "denied-sensitive-ocr",
        )
        with self.assertRaises(SensitiveDataError):
            assert_durable_descriptor_eligible(yaml_workflow)

    def test_recognizes_tsv_lines_matches_confidence_and_source(self) -> None:
        plugin = self.make_plugin()
        manifest = plugin.start()
        result = plugin.invoke(
            "vision.ocr.recognize@1",
            {
                "artifact": {"path": str(self.image), "media_type": "image/png"},
                "languages": ["eng", "deu"],
                "minimum_confidence": 0.80,
                "patterns": [{"id": "invoice_id", "value": "A-42"}],
            },
        )

        self.assertEqual(result["provider"], "tesseract")
        self.assertEqual(result["version"], "5.4.1")
        self.assertEqual(result["text"], "Invoice A-42\nTotal $12.50")
        self.assertIsNone(result["source_region"])
        self.assertEqual(result["source"]["kind"], "artifact")
        self.assertEqual(result["source"]["path"], str(self.image.resolve()))
        self.assertRegex(result["source"]["digest"], r"^sha256:[0-9a-f]{64}$")
        self.assertEqual(result["lines"][0]["bounds"], {"x": 10, "y": 20, "width": 95, "height": 12})
        self.assertAlmostEqual(result["confidence"], 0.88, places=2)
        self.assertEqual(result["matches"][0]["pattern_id"], "invoice_id")
        self.assertEqual(result["matches"][0]["text"], "A-42")
        self.assertEqual(result["matches"][0]["bounds"], {"x": 55, "y": 20, "width": 50, "height": 12})
        engine_args = (self.directory / "engine.log").read_text(
            encoding="utf-8"
        ).splitlines()
        self.assertNotEqual(engine_args[0], str(self.image.resolve()))
        self.assertIn("aad-ocr-", engine_args[0])
        self.assertEqual(engine_args[1:4], ["stdout", "-l", "eng+deu"])
        self.assertEqual(engine_args[4], "tsv")
        self.assertEqual(
            (self.directory / "digest.log").read_text(encoding="ascii"),
            result["source"]["digest"].removeprefix("sha256:"),
        )
        schema = manifest["actions"]["recognize"]["output_schema"]
        jsonschema.Draft202012Validator(schema).validate(result)
        for field in ("source", "lines", "matches"):
            malformed = deepcopy(result)
            nested = malformed[field] if field == "source" else malformed[field][0]
            nested["unexpected"] = True
            with self.subTest(field=field), self.assertRaises(
                jsonschema.ValidationError
            ):
                jsonschema.Draft202012Validator(schema).validate(malformed)

    @unittest.skipUnless(
        os.name in {"posix", "nt"}, "artifact byte-stream transport unavailable"
    )
    def test_artifact_action_consumes_bytes_without_exposing_a_host_path(self) -> None:
        store = ArtifactStore(
            **(
                {"temporary_parent": self.temporary.name}
                if os.name == "posix"
                else {}
            )
        )
        self.addCleanup(store.cleanup)
        reference = store.import_bytes(PNG_1X1, media_type="image/png")
        plugin = self.make_plugin()

        result = plugin.invoke_with_artifacts(
            "vision.ocr.recognize_artifact@1",
            {
                "artifact": reference.to_dict(),
                "languages": ["eng"],
                "patterns": [{"id": "invoice_id", "value": "A-42"}],
            },
            store,
        )

        self.assertEqual(result["text"], "Invoice A-42\nTotal $12.50")
        self.assertEqual(
            result["source"],
            {
                "kind": "artifact",
                "digest": reference.digest,
                "media_type": reference.media_type,
                "size_bytes": reference.size_bytes,
            },
        )
        self.assertNotIn("path", result["source"])
        if store._root is not None:
            self.assertNotIn(os.fspath(store._root), repr(result) + plugin.stderr)
        self.assertNotIn(str(self.image.resolve()), repr(result) + plugin.stderr)
        self.assertEqual(
            (self.directory / "digest.log").read_text(encoding="ascii"),
            reference.digest.removeprefix("sha256:"),
        )
        schema = plugin.manifest["actions"]["recognize_artifact"][
            "output_schema"
        ]
        jsonschema.Draft202012Validator(schema).validate(result)

    @unittest.skipUnless(
        os.name in {"posix", "nt"}, "artifact byte-stream transport unavailable"
    )
    def test_artifact_action_error_and_timeout_keep_connection_reusable(self) -> None:
        store = ArtifactStore(
            **(
                {"temporary_parent": self.temporary.name}
                if os.name == "posix"
                else {}
            )
        )
        self.addCleanup(store.cleanup)
        reference = store.import_bytes(PNG_1X1, media_type="image/png")
        plugin = self.make_plugin(timeout=3)
        plugin.start()
        provider_pid = plugin.pid
        arguments = {"artifact": reference.to_dict()}

        with self.assertRaises(PluginError) as low_confidence:
            plugin.invoke_with_artifacts(
                "vision.ocr.recognize_artifact@1",
                {**arguments, "minimum_confidence": 0.99},
                store,
                timeout=2,
            )
        self.assertEqual(low_confidence.exception.code, "OCR.LOW_CONFIDENCE")
        self.assertEqual(
            low_confidence.exception.message,
            "recognized text is below minimum_confidence",
        )
        self.assertFalse(plugin.closed)
        self.assertEqual(plugin.pid, provider_pid)

        with self.assertRaises(PluginError) as timed_out:
            plugin.invoke_with_artifacts(
                "vision.ocr.recognize_artifact@1",
                arguments,
                store,
                timeout=0.35,
            )
        self.assertEqual(timed_out.exception.code, "OCR.TIMEOUT")
        self.assertEqual(
            timed_out.exception.message,
            "host deadline elapsed before OCR could complete",
        )
        self.assertFalse(plugin.closed)
        self.assertEqual(plugin.pid, provider_pid)

        result = plugin.invoke_with_artifacts(
            "vision.ocr.recognize_artifact@1", arguments, store, timeout=2
        )
        self.assertEqual(result["text"], "Invoice A-42\nTotal $12.50")
        self.assertEqual(plugin.pid, provider_pid)

    @unittest.skipUnless(
        os.name in {"posix", "nt"}, "artifact byte-stream transport unavailable"
    )
    def test_artifact_action_rejects_path_object_before_dispatch(self) -> None:
        store = ArtifactStore(
            **(
                {"temporary_parent": self.temporary.name}
                if os.name == "posix"
                else {}
            )
        )
        self.addCleanup(store.cleanup)
        plugin = self.make_plugin()

        with self.assertRaises(PluginError) as raised:
            plugin.invoke_with_artifacts(
                "vision.ocr.recognize_artifact@1",
                {"artifact": {"path": str(self.image)}},
                store,
            )

        self.assertEqual(raised.exception.code, "PLUGIN.ARTIFACT_INPUT_INVALID")
        self.assertFalse(raised.exception.dispatched)
        self.assertFalse((self.directory / "engine.log").exists())

    def test_real_provider_workflow_match_drives_response_branch(self) -> None:
        matched = self.run_explicit_workflow(
            target_text="A-42",
            minimum_confidence=0.80,
            languages=["eng", "chi_sim"],
        )

        self.assertTrue(matched.ok, matched.to_dict())
        self.assertEqual(matched.output["decision"], "respond")
        self.assertEqual(matched.output["matched_text"], "A-42")
        self.assertGreaterEqual(matched.output["match_confidence"], 0.80)
        self.assertEqual(
            matched.steps["recognize_image"]["output"]["source"]["path"],
            str(self.image.resolve()),
        )
        self.assertEqual(matched.steps["respond_to_match"]["status"], "succeeded")
        self.assertNotIn("do_not_respond", matched.steps)
        engine_args = (self.directory / "engine.log").read_text(encoding="utf-8").splitlines()
        self.assertEqual(engine_args[1:4], ["stdout", "-l", "eng+chi_sim"])

    def test_real_provider_workflow_low_confidence_does_not_respond(self) -> None:
        low_confidence = self.run_explicit_workflow(
            target_text="A-42", minimum_confidence=0.86
        )

        self.assertTrue(low_confidence.ok, low_confidence.to_dict())
        self.assertEqual(low_confidence.output["decision"], "no_response")
        self.assertTrue(low_confidence.output["pattern_found"])
        recognition = low_confidence.steps["recognize_image"]["output"]
        self.assertGreaterEqual(recognition["confidence"], 0.86)
        self.assertLess(recognition["matches"][0]["confidence"], 0.86)
        self.assertNotIn("respond_to_match", low_confidence.steps)
        self.assertEqual(
            low_confidence.steps["do_not_respond"]["status"], "succeeded"
        )

    def test_real_provider_workflow_no_match_does_not_respond(self) -> None:
        no_match = self.run_explicit_workflow(
            target_text="Approve payment", minimum_confidence=0.80
        )

        self.assertTrue(no_match.ok, no_match.to_dict())
        self.assertEqual(no_match.output["decision"], "no_response")
        self.assertFalse(no_match.output["pattern_found"])
        self.assertNotIn("respond_to_match", no_match.steps)
        self.assertEqual(no_match.steps["do_not_respond"]["status"], "succeeded")

    def test_real_provider_workflow_no_text_explicitly_returns_no_response(self) -> None:
        no_text = self.run_explicit_workflow(
            target_text="A-42",
            minimum_confidence=0.80,
            tsv=TSV.splitlines()[0] + "\n",
        )

        self.assertTrue(no_text.ok, no_text.to_dict())
        self.assertEqual(
            no_text.output,
            {"decision": "no_response", "reason": "no_text_recognized"},
        )
        self.assertEqual(no_text.steps["record_no_text"]["status"], "succeeded")
        self.assertEqual(no_text.steps["recognize_image"]["status"], "failed")

    def test_missing_tesseract_is_a_structured_engine_unavailable_error(self) -> None:
        missing = str(self.directory / "missing-tesseract")
        plugin = self.make_plugin(command=[missing])

        with self.assertRaises(PluginError) as raised:
            plugin.invoke("vision.ocr.recognize@1", {"image": {"path": str(self.image)}})

        self.assertEqual(raised.exception.code, "OCR.ENGINE_UNAVAILABLE")
        self.assertFalse(raised.exception.retryable)
        self.assertTrue(raised.exception.dispatched)
        self.assertEqual(raised.exception.details["executable"], missing)

    def test_rejects_implicit_or_ambiguous_image_sources_before_engine(self) -> None:
        plugin = self.make_plugin()
        cases = (
            {},
            {"image": {"path": "relative.png"}},
            {"image": {"path": str(self.image)}, "artifact": {"path": str(self.image)}},
            {"screenshot": True},
        )
        for args in cases:
            with self.subTest(args=args), self.assertRaises(PluginError) as raised:
                plugin.invoke("vision.ocr.recognize@1", args)
            self.assertEqual(raised.exception.code, "OCR.INVALID_REQUEST")
        self.assertFalse((self.directory / "engine.log").exists())

    def test_low_confidence_and_regex_pattern_are_structured_errors(self) -> None:
        plugin = self.make_plugin()
        with self.assertRaises(PluginError) as raised:
            plugin.invoke(
                "vision.ocr.recognize@1",
                {"image": {"path": str(self.image)}, "minimum_confidence": 0.99},
            )
        self.assertEqual(raised.exception.code, "OCR.LOW_CONFIDENCE")
        self.assertLess(raised.exception.details["confidence"], 0.99)

        with self.assertRaises(PluginError) as raised:
            plugin.invoke(
                "vision.ocr.recognize@1",
                {
                    "image": {"path": str(self.image)},
                    "patterns": [{"id": "bad", "regex": "A-.*"}],
                },
            )
        self.assertEqual(raised.exception.code, "OCR.INVALID_REQUEST")
        self.assertEqual(raised.exception.details["index"], 0)

    def test_duplicate_pattern_ids_and_nul_are_rejected_before_engine(self) -> None:
        cases = {
            "duplicate": [
                {"id": "same", "value": "A"},
                {"id": "same", "value": "B"},
            ],
            "id_nul": [{"id": "bad\x00id", "value": "A"}],
            "value_nul": [{"id": "bad", "value": "A\x00B"}],
        }
        for name, patterns in cases.items():
            with self.subTest(name=name), self.assertRaises(PluginError) as raised:
                self.make_plugin().invoke(
                    "vision.ocr.recognize@1",
                    {"image": {"path": str(self.image)}, "patterns": patterns},
                )
            self.assertEqual(raised.exception.code, "OCR.INVALID_REQUEST")
        self.assertFalse((self.directory / "engine.log").exists())

    def test_out_of_bounds_region_is_rejected_before_engine(self) -> None:
        with self.assertRaises(PluginError) as raised:
            self.make_plugin().invoke(
                "vision.ocr.recognize@1",
                {
                    "image": {"path": str(self.image)},
                    "region": {"x": 1, "y": 0, "width": 1, "height": 1},
                },
            )

        self.assertEqual(raised.exception.code, "OCR.INVALID_REQUEST")
        self.assertEqual(raised.exception.details["image_width"], 1)
        self.assertFalse((self.directory / "engine.log").exists())

    def test_no_text_is_a_structured_provider_error(self) -> None:
        with self.assertRaises(PluginError) as raised:
            self.make_plugin(
                env_updates={"FAKE_TESSERACT_TSV": TSV.splitlines()[0] + "\n"}
            ).invoke(
                "vision.ocr.recognize@1", {"image": {"path": str(self.image)}}
            )

        self.assertEqual(raised.exception.code, "OCR.NO_TEXT")
        self.assertFalse(raised.exception.retryable)

    def test_pixel_limit_and_decode_bomb_are_structured_before_engine(self) -> None:
        oversized = self.directory / "oversized.png"
        raw = bytearray(PNG_1X1)
        raw[16:20] = (20_001).to_bytes(4, "big")
        raw[20:24] = (20_001).to_bytes(4, "big")
        oversized.write_bytes(raw)
        with self.assertRaises(PluginError) as raised:
            self.make_plugin().invoke(
                "vision.ocr.recognize@1", {"image": {"path": str(oversized)}}
            )
        self.assertEqual(raised.exception.code, "OCR.IMAGE_LIMIT_EXCEEDED")
        self.assertEqual(raised.exception.details["phase"], "file_header")
        self.assertFalse((self.directory / "engine.log").exists())

        module = load_ocr_module()
        with mock.patch.object(Image, "MAX_IMAGE_PIXELS", 0):
            with self.assertRaises(module.RequestError) as bomb:
                module._validate_decoded_image(
                    self.image, "image/png", time.monotonic() + 1
                )
        self.assertEqual(bomb.exception.code, "OCR.IMAGE_LIMIT_EXCEEDED")
        self.assertEqual(bomb.exception.data["reason"], "DecompressionBombError")

    def test_supported_format_headers_expose_dimensions_when_available(self) -> None:
        module = load_ocr_module()
        formats = (
            ("PNG", "png"),
            ("JPEG", "jpg"),
            ("GIF", "gif"),
            ("TIFF", "tiff"),
            ("BMP", "bmp"),
            ("WEBP", "webp"),
            ("PPM", "ppm"),
        )
        for image_format, extension in formats:
            path = self.directory / f"header.{extension}"
            Image.new("RGB", (321, 123)).save(path, image_format)
            payload = path.read_bytes()
            detected = module._media_type(payload[:4096], payload[-16:], len(payload))
            with self.subTest(image_format=image_format):
                self.assertIsNotNone(detected)
                self.assertEqual(
                    module._header_dimensions(detected, payload[:4096]), (321, 123)
                )

    def test_private_snapshot_survives_source_replacement(self) -> None:
        replacement = self.directory / "replacement.png"
        replacement.write_bytes(PNG_1X1 + b"changed")
        original_digest = hashlib.sha256(PNG_1X1).hexdigest()
        result = self.make_plugin(
            env_updates={
                "FAKE_TESSERACT_REPLACEMENT": str(replacement),
                "FAKE_TESSERACT_SOURCE": str(self.image),
            }
        ).invoke("vision.ocr.recognize@1", {"image": {"path": str(self.image)}})

        self.assertEqual(result["source"]["digest"], f"sha256:{original_digest}")
        self.assertEqual(
            (self.directory / "digest.log").read_text(encoding="ascii"),
            original_digest,
        )
        self.assertNotEqual(hashlib.sha256(self.image.read_bytes()).hexdigest(), original_digest)

    def test_invalid_utf8_nul_negative_coordinate_and_fake_magic_are_structured(self) -> None:
        header = TSV.splitlines()[0]
        cases = {
            "invalid_utf8": (b"\xff", "OCR.OUTPUT_INVALID"),
            "nul": ((header + "\n" + TSV.splitlines()[1] + "\x00\n").encode(), "OCR.OUTPUT_INVALID"),
            "negative": (
                (header + "\n5\t1\t1\t1\t1\t1\t-1\t0\t1\t1\t90\tbad\n").encode(),
                "OCR.OUTPUT_INVALID",
            ),
        }
        for name, (payload, code) in cases.items():
            with self.subTest(name=name), self.assertRaises(PluginError) as raised:
                self.make_plugin(
                    env_updates={"FAKE_TESSERACT_TSV_B64": base64.b64encode(payload).decode()}
                ).invoke("vision.ocr.recognize@1", {"image": {"path": str(self.image)}})
            self.assertEqual(raised.exception.code, code)

        fake = self.directory / "fake.png"
        fake.write_bytes(b"\x89PNG\r\n\x1a\nnot-a-png")
        with self.assertRaises(PluginError) as raised:
            self.make_plugin().invoke(
                "vision.ocr.recognize@1", {"image": {"path": str(fake)}}
            )
        self.assertEqual(raised.exception.code, "OCR.IMAGE_UNSUPPORTED")

    def test_engine_streams_are_bounded_while_running(self) -> None:
        cases = {
            "stdout": {"FAKE_TESSERACT_STDOUT_BYTES": str(5 * 1024 * 1024)},
            "stderr": {"FAKE_TESSERACT_STDERR_BYTES": str(128 * 1024)},
        }
        for stream, environment in cases.items():
            started = time.monotonic()
            with self.subTest(stream=stream), self.assertRaises(PluginError) as raised:
                self.make_plugin(env_updates=environment, timeout=2).invoke(
                    "vision.ocr.recognize@1", {"image": {"path": str(self.image)}}
                )
            self.assertEqual(raised.exception.code, "OCR.OUTPUT_INVALID")
            self.assertEqual(raised.exception.details["stream"], stream)
            self.assertLess(time.monotonic() - started, 2)

    def test_total_deadline_covers_version_and_recognition(self) -> None:
        with self.assertRaises(PluginError) as raised:
            self.make_plugin(
                env_updates={"FAKE_TESSERACT_VERSION_DELAY": "2"}, timeout=2
            ).invoke("vision.ocr.recognize@1", {"image": {"path": str(self.image)}})
        self.assertEqual(raised.exception.code, "OCR.TIMEOUT")
        self.assertTrue(raised.exception.retryable)

    def test_short_budget_does_not_launch_engine_and_provider_remains_usable(self) -> None:
        started_log = self.directory / "started.log"
        plugin = self.make_plugin(
            env_updates={"FAKE_TESSERACT_STARTED_LOG": str(started_log)},
            timeout=3,
        )
        plugin.start()
        provider_pid = plugin.pid

        with self.assertRaises(PluginError) as raised:
            plugin.invoke(
                "vision.ocr.recognize@1",
                {"image": {"path": str(self.image)}},
                timeout=0.35,
            )

        self.assertEqual(raised.exception.code, "OCR.TIMEOUT")
        self.assertTrue(raised.exception.retryable)
        self.assertFalse(started_log.exists())
        self.assertEqual(plugin.pid, provider_pid)
        result = plugin.invoke(
            "vision.ocr.recognize@1",
            {"image": {"path": str(self.image)}},
            timeout=2,
        )
        self.assertEqual(result["text"], "Invoice A-42\nTotal $12.50")
        self.assertEqual(plugin.pid, provider_pid)

    def test_slow_engine_termination_returns_ocr_timeout_and_reuses_provider(self) -> None:
        started_log = self.directory / "started.log"
        delay_once = self.directory / "version-delay-started"
        plugin = self.make_plugin(
            env_updates={
                "FAKE_TESSERACT_STARTED_LOG": str(started_log),
                "FAKE_TESSERACT_IGNORE_SIGTERM": "1",
                "FAKE_TESSERACT_VERSION_DELAY": "30",
                "FAKE_TESSERACT_VERSION_DELAY_ONCE": str(delay_once),
            },
            timeout=3,
        )
        plugin.start()
        provider_pid = plugin.pid

        with self.assertRaises(PluginError) as raised:
            plugin.invoke(
                "vision.ocr.recognize@1",
                {"image": {"path": str(self.image)}},
                timeout=2,
            )

        self.assertEqual(raised.exception.code, "OCR.TIMEOUT")
        self.assertTrue(raised.exception.retryable)
        self.assertEqual(plugin.pid, provider_pid)
        first_engine_pid = int(started_log.read_text(encoding="utf-8").split()[0])
        if os.name == "posix":
            with self.assertRaises(ProcessLookupError):
                os.kill(first_engine_pid, 0)

        result = plugin.invoke(
            "vision.ocr.recognize@1",
            {"image": {"path": str(self.image)}},
            timeout=2,
        )
        self.assertEqual(result["text"], "Invoice A-42\nTotal $12.50")
        self.assertEqual(plugin.pid, provider_pid)

    def test_windows_launcher_and_permission_documentation_exist(self) -> None:
        launcher = OCR_PLUGIN.with_name("run.cmd").read_text(encoding="utf-8")
        readme = OCR_PLUGIN.with_name("README.md").read_text(encoding="utf-8")
        self.assertIn("%~dp0ocr_tesseract_plugin.py", launcher)
        self.assertIn("py -3", launcher)
        self.assertIn("requires.permissions", readme)
        self.assertIn("--permission filesystem.read", readme)

    def test_engine_environment_does_not_inherit_unrelated_secrets(self) -> None:
        module = load_ocr_module()
        with mock.patch.dict(
            module.os.environ,
            {
                "AWS_SECRET_ACCESS_KEY": "secret",
                "TESSDATA_PREFIX": "/trusted/tessdata",
                "FAKE_TESSERACT_TSV": "fixture",
            },
            clear=True,
        ):
            environment = module._engine_environment("/usr/bin/tesseract")

        self.assertNotIn("AWS_SECRET_ACCESS_KEY", environment)
        self.assertEqual(environment["TESSDATA_PREFIX"], "/trusted/tessdata")
        self.assertEqual(environment["FAKE_TESSERACT_TSV"], "fixture")
        self.assertEqual(environment["OMP_NUM_THREADS"], "1")
        self.assertEqual(environment["OMP_THREAD_LIMIT"], "1")

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux prlimit contract")
    def test_linux_engine_launch_has_resource_limits_and_fails_closed(self) -> None:
        module = load_ocr_module()
        prefix = module._linux_prlimit_prefix(time.monotonic() + 5)
        self.assertTrue(prefix[0].endswith("prlimit"))
        self.assertTrue(any(item.startswith("--as=") for item in prefix))
        self.assertTrue(any(item.startswith("--cpu=") for item in prefix))
        self.assertTrue(any(item.startswith("--fsize=") for item in prefix))
        self.assertTrue(any(item.startswith("--nofile=") for item in prefix))
        self.assertFalse(any(item.startswith("--nproc=") for item in prefix))
        self.assertEqual(prefix[-1], "--")

        with mock.patch.object(module.shutil, "which", return_value=None):
            with self.assertRaises(module.RequestError) as raised:
                module._linux_prlimit_prefix(time.monotonic() + 5)
        self.assertEqual(raised.exception.code, "OCR.ENGINE_ISOLATION_UNAVAILABLE")

        log = self.directory / "rlimits.json"
        self.make_plugin(
            env_updates={"FAKE_TESSERACT_RLIMIT_LOG": str(log)}
        ).invoke(
            "vision.ocr.recognize@1", {"image": {"path": str(self.image)}}
        )
        applied = json.loads(log.read_text(encoding="utf-8"))
        self.assertEqual(applied["RLIMIT_AS"], [2 * 1024**3, 2 * 1024**3])
        self.assertEqual(applied["RLIMIT_FSIZE"], [16 * 1024**2, 16 * 1024**2])
        self.assertEqual(applied["RLIMIT_NOFILE"], [64, 64])
        self.assertGreaterEqual(applied["RLIMIT_CPU"][0], 1)
        self.assertLessEqual(applied["RLIMIT_CPU"][0], 30)

    def test_non_linux_engine_isolation_requires_explicit_operator_override(self) -> None:
        module = load_ocr_module()
        with (
            mock.patch.object(module.sys, "platform", "darwin"),
            mock.patch.dict(module.os.environ, {}, clear=True),
            self.assertRaises(module.RequestError) as raised,
        ):
            module._linux_prlimit_prefix(time.monotonic() + 5)
        self.assertEqual(raised.exception.code, "OCR.ENGINE_ISOLATION_UNAVAILABLE")
        self.assertEqual(
            raised.exception.data["operator_override"],
            "OCR_ALLOW_UNSANDBOXED_ENGINE",
        )
        with (
            mock.patch.object(module.sys, "platform", "win32"),
            mock.patch.dict(
                module.os.environ, {"OCR_ALLOW_UNSANDBOXED_ENGINE": "1"}, clear=True
            ),
        ):
            self.assertEqual(module._linux_prlimit_prefix(time.monotonic() + 5), [])

    def test_readme_documents_limits_isolation_and_durable_boundary(self) -> None:
        readme = OCR_PLUGIN.with_name("README.md").read_text(encoding="utf-8")
        spec = (
            PROJECT_ROOT / "docs/spec/workflow-descriptor-v1alpha1.md"
        ).read_text(encoding="utf-8")
        for marker in (
            "40,000,000",
            "OCR.IMAGE_LIMIT_EXCEEDED",
            "prlimit",
            "OMP_NUM_THREADS=1",
            "不是完整沙箱",
            "OCR_ALLOW_UNSANDBOXED_ENGINE=1",
            "sensitive: true",
            "OCR.NO_TEXT",
        ):
            self.assertIn(marker, readme)
        self.assertIn("durable start 必须在创建 run 前拒绝", spec)

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux Tesseract smoke")
    def test_real_tesseract_smoke_uses_single_thread_environment(self) -> None:
        if not Path("/usr/bin/tesseract").exists():
            self.skipTest("/usr/bin/tesseract is not installed")

        languages = subprocess.run(
            ["/usr/bin/tesseract", "--list-langs"],
            check=False,
            capture_output=True,
            text=True,
            env={"PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8", "LANG": "C.UTF-8"},
        )
        if languages.returncode != 0:
            self.skipTest(f"unable to query Tesseract languages: {languages.stderr.strip()}")
        available = {line.strip() for line in languages.stdout.splitlines() if line.strip()}
        if not {"eng", "chi_sim"}.issubset(available):
            self.skipTest(f"missing Tesseract languages: {sorted({'eng', 'chi_sim'} - available)}")

        font_query = subprocess.run(
            ["fc-match", "-f", "%{file}\n", "Noto Sans CJK SC"],
            check=False,
            capture_output=True,
            text=True,
            env={"PATH": "/usr/bin:/bin"},
        )
        font_path = Path(font_query.stdout.strip())
        if font_query.returncode != 0 or not font_path.exists():
            self.skipTest("missing a system font capable of rendering chi_sim smoke text")

        image = self.directory / "smoke.png"
        canvas = Image.new("RGB", (240, 80), "white")
        draw = ImageDraw.Draw(canvas)
        font = ImageFont.truetype(str(font_path), 24)
        draw.text((12, 8), "TEST 42", fill="black", font=font)
        draw.text((12, 40), "中文", fill="black", font=font)
        canvas.save(image, "PNG")

        plugin = ProcessPlugin(
            [sys.executable, str(OCR_PLUGIN)],
            env={
                "PATH": "/usr/bin:/bin",
                "TESSERACT_CMD": "/usr/bin/tesseract",
            },
            timeout=8,
            name="vision.ocr",
        )
        self.addCleanup(plugin.close)
        result = plugin.invoke(
            "vision.ocr.recognize@1",
            {
                "image": {"path": str(image)},
                "languages": ["eng", "chi_sim"],
                "minimum_confidence": 0.0,
                "patterns": [{"id": "ascii", "value": "TEST"}],
            },
        )

        self.assertEqual(result["provider"], "tesseract")
        self.assertIn("TEST", result["text"])
        self.assertEqual(result["matches"][0]["pattern_id"], "ascii")
        self.assertGreaterEqual(result["confidence"], 0.0)


if __name__ == "__main__":
    unittest.main()
