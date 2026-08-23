"""Integration tests for the optional Tesseract OCR process plugin."""

from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import textwrap
import time
import unittest
from unittest import mock

from ai_auto_desktop.plugin import PluginError, ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
OCR_PLUGIN = PROJECT_ROOT / "plugins" / "ocr_tesseract" / "ocr_tesseract_plugin.py"
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
                import sys
                import time

                if "--version" in sys.argv:
                    replacement = os.environ.get("FAKE_TESSERACT_REPLACEMENT")
                    source = os.environ.get("FAKE_TESSERACT_SOURCE")
                    if replacement and source:
                        os.replace(replacement, source)
                    time.sleep(float(os.environ.get("FAKE_TESSERACT_VERSION_DELAY", "0")))
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

    def test_manifest_declares_explicit_read_only_recognize_contract(self) -> None:
        manifest = self.make_plugin().start()

        self.assertEqual(manifest["metadata"]["name"], "vision.ocr")
        self.assertEqual(set(manifest["actions"]), {"recognize"})
        contract = manifest["actions"]["recognize"]
        self.assertEqual(contract["contract_major"], 1)
        self.assertEqual(contract["effect"]["default_class"], "read_only")
        self.assertEqual(contract["risk"], {"category": "observe", "level": "low"})
        self.assertEqual(manifest["permissions"], ["filesystem.read"])
        self.assertIn("OCR.ENGINE_UNAVAILABLE", {item["code"] for item in contract["errors"]})

    def test_recognizes_tsv_lines_matches_confidence_and_source(self) -> None:
        result = self.make_plugin().invoke(
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
        engine_args = (self.directory / "engine.log").read_text(encoding="utf-8").splitlines()
        self.assertNotEqual(engine_args[0], str(self.image.resolve()))
        self.assertIn("aad-ocr-", engine_args[0])
        self.assertEqual(engine_args[1:4], ["stdout", "-l", "eng+deu"])
        self.assertEqual(engine_args[4], "tsv")
        self.assertEqual(
            (self.directory / "digest.log").read_text(encoding="ascii"),
            result["source"]["digest"].removeprefix("sha256:"),
        )

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
                env_updates={"FAKE_TESSERACT_VERSION_DELAY": "0.35"}, timeout=0.25
            ).invoke("vision.ocr.recognize@1", {"image": {"path": str(self.image)}})
        self.assertEqual(raised.exception.code, "OCR.TIMEOUT")
        self.assertTrue(raised.exception.retryable)

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


if __name__ == "__main__":
    unittest.main()
