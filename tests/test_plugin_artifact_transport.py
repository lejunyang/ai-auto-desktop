"""Host-side contracts for process-plugin artifact transport."""

from __future__ import annotations

import io
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest import mock

from PIL import Image

from ai_auto_desktop.artifacts import ArtifactStore
from ai_auto_desktop.plugin import PluginError, ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_PLUGIN = PROJECT_ROOT / "plugins" / "fixture" / "fixture_plugin.py"


def png_bytes(*, color: str = "#235789") -> bytes:
    output = io.BytesIO()
    Image.new("RGB", (3, 2), color).save(output, format="PNG")
    return output.getvalue()


@unittest.skipUnless(
    os.name in {"posix", "nt"}, "artifact byte-stream transport is unavailable"
)
class ProcessPluginArtifactTransportTests(unittest.TestCase):
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

    def test_round_trip_never_exposes_a_host_path(self) -> None:
        payload = png_bytes()
        source = self.store.import_bytes(payload, media_type="image/png")

        result = self.plugin.invoke_with_artifacts(
            "fixture.artifact_copy@1", {"source": source.to_dict()}, self.store
        )

        self.assertEqual(set(result), {"result"})
        copied = result["result"]
        self.assertNotEqual(copied["artifactId"], source.artifact_id)
        self.assertEqual(copied["digest"], source.digest)
        with self.store.resolve(copied) as handle:
            self.assertEqual(handle.read(), payload)
        wire = repr(result) + self.plugin.stderr
        if self.store._root is not None:
            self.assertNotIn(os.fspath(self.store._root), wire)
        self.assertNotIn("storageKey", wire)

    def test_input_is_resolved_before_request_dispatch(self) -> None:
        options = (
            {"temporary_parent": self.temporary.name}
            if os.name == "posix"
            else {}
        )
        other = ArtifactStore(**options)
        self.addCleanup(other.cleanup)
        foreign = other.import_bytes(png_bytes())

        with self.assertRaises(PluginError) as raised:
            self.plugin.invoke_with_artifacts(
                "fixture.artifact_copy@1",
                {"source": foreign.to_dict()},
                self.store,
            )

        self.assertEqual(raised.exception.code, "ARTIFACT.SCOPE_MISMATCH")
        self.assertFalse(raised.exception.dispatched)
        self.assertIsNotNone(self.plugin.pid)
        self.assertIsNone(self.plugin._process.poll())

    def test_undeclared_artifact_and_placeholder_are_rejected_pre_dispatch(self) -> None:
        source = self.store.import_bytes(png_bytes())
        self.plugin.start()
        for args, code in (
            (
                {"source": source.to_dict(), "extra": source.to_dict()},
                "PLUGIN.ARTIFACT_INPUT_UNDECLARED",
            ),
            (
                {"source": {"$hostArtifact": {}}},
                "PLUGIN.ARTIFACT_INPUT_INVALID",
            ),
        ):
            with self.subTest(code=code), self.assertRaises(PluginError) as raised:
                self.plugin.invoke_with_artifacts(
                    "fixture.artifact_copy@1", args, self.store
                )
            self.assertEqual(raised.exception.code, code)
            self.assertFalse(raised.exception.dispatched)

    def test_output_schema_failure_rolls_back_all_outputs(self) -> None:
        source = self.store.import_bytes(png_bytes())
        before = set(self.store._records)

        def reject(_value: object) -> None:
            raise PluginError("TEST.REJECTED", "reject materialized output")

        with self.assertRaises(PluginError) as raised:
            self.plugin.invoke_with_artifacts(
                "fixture.artifact_copy@1", {"source": source.to_dict()},
                self.store, result_validator=reject,
            )

        self.assertEqual(raised.exception.code, "TEST.REJECTED")
        self.assertTrue(raised.exception.dispatched)
        self.assertEqual(set(self.store._records), before)
        self.assertFalse(self.plugin.closed)

    def test_legacy_invocation_remains_compatible_after_artifact_roundtrip(self) -> None:
        source = self.store.import_bytes(png_bytes())
        self.plugin.invoke_with_artifacts(
            "fixture.artifact_copy@1", {"source": source.to_dict()}, self.store
        )
        result = self.plugin.invoke("ocr", {"text": "still alive"})
        self.assertEqual(result["text"], "still alive")

    def test_early_worker_error_aborts_without_waiting_for_large_input_deadline(self) -> None:
        payload = png_bytes()
        source = self.store.import_bytes(payload)
        real_send = self.plugin._send_artifact_inputs

        def corrupt_first_token(channel, request_id, values, deadline):
            changed = list(values)
            slot, reference, _token, data = changed[0]
            changed[0] = (slot, reference, "9" * 32, data)
            return real_send(channel, request_id, changed, deadline)

        started = time.monotonic()
        with mock.patch.object(
            self.plugin, "_send_artifact_inputs", side_effect=corrupt_first_token
        ), self.assertRaises(PluginError) as raised:
            self.plugin.invoke_with_artifacts(
                "fixture.artifact_copy@1", {"source": source.to_dict()},
                self.store, timeout=1.5,
            )

        self.assertEqual(raised.exception.code, "FIXTURE.ARTIFACT_IPC")
        self.assertTrue(raised.exception.dispatched)
        self.assertTrue(self.plugin.closed)
        self.assertLess(time.monotonic() - started, 1.4)

    def test_post_input_error_keeps_artifact_channel_reusable(self) -> None:
        payload = png_bytes()
        source = self.store.import_bytes(payload)
        contract = self.plugin.start()["actions"]["artifact_copy"]
        original_limit = contract["artifacts"]["outputs"]["result"][
            "max_size_bytes"
        ]
        contract["artifacts"]["outputs"]["result"]["max_size_bytes"] = 1
        with self.assertRaises(PluginError) as first:
            self.plugin.invoke_with_artifacts(
                "fixture.artifact_copy@1", {"source": source.to_dict()},
                self.store,
            )
        self.assertEqual(first.exception.code, "FIXTURE.ARTIFACT_IPC")
        self.assertFalse(self.plugin.closed)

        contract["artifacts"]["outputs"]["result"][
            "max_size_bytes"
        ] = original_limit
        result = self.plugin.invoke_with_artifacts(
            "fixture.artifact_copy@1", {"source": source.to_dict()}, self.store
        )
        with self.store.resolve(result["result"]) as handle:
            self.assertEqual(handle.read(), payload)


if __name__ == "__main__":
    unittest.main()
