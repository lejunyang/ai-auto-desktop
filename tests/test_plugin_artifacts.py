"""Contracts for artifact slot declarations in capability manifests."""

from __future__ import annotations

from copy import deepcopy
import json
from pathlib import Path
import unittest

import jsonschema

from ai_auto_desktop.plugin import PluginError, ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCHEMA_PATHS = (
    PROJECT_ROOT / "schemas/capabilities/v1alpha1/capability-manifest.schema.json",
    PROJECT_ROOT
    / "src/ai_auto_desktop/schemas/capabilities/v1alpha1/capability-manifest.schema.json",
)


def artifact_slot(pointer: str) -> dict[str, object]:
    return {
        "pointer": pointer,
        "media_types": ["image/png", "image/jpeg"],
        "max_size_bytes": 4 * 1024 * 1024,
    }


def manifest() -> dict[str, object]:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": "artifact.fixture", "version": "1.0.0"},
        "runtime": {
            "kind": "process",
            "protocol": "ndjson-stdio-v1",
            "entrypoint": "fixture",
        },
        "actions": {
            "transform": {
                "contract_major": 1,
                "effect": {"default_class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
                "input_schema": {"type": "object"},
                "output_schema": {"type": "object"},
                "artifacts": {
                    "inputs": {"source": artifact_slot("/source")},
                    "outputs": {"result": artifact_slot("/result")},
                },
            }
        },
    }


class ArtifactManifestSchemaTests(unittest.TestCase):
    def setUp(self) -> None:
        self.plugin = ProcessPlugin(["unused"])
        self.addCleanup(self.plugin.close)

    def assert_invalid(self, value: dict[str, object]) -> None:
        with self.assertRaises(PluginError) as raised:
            self.plugin._validate_manifest(value)
        self.assertEqual(raised.exception.code, "PLUGIN.HOST_PROTOCOL_ERROR")
        self.assertFalse(raised.exception.dispatched)

    def test_source_and_packaged_schema_are_identical_and_valid(self) -> None:
        documents = [json.loads(path.read_text(encoding="utf-8")) for path in SCHEMA_PATHS]
        self.assertEqual(documents[0], documents[1])
        jsonschema.Draft202012Validator.check_schema(documents[0])

    def test_accepts_closed_input_and_output_slot_maps(self) -> None:
        self.plugin._validate_manifest(manifest())

    def test_artifacts_are_optional_for_legacy_actions(self) -> None:
        value = manifest()
        del value["actions"]["transform"]["artifacts"]
        self.plugin._validate_manifest(value)

    def test_rejects_empty_or_open_artifact_declarations(self) -> None:
        for artifacts in (
            {},
            {"inputs": {}},
            {"outputs": {}},
            {"inputs": {"source": artifact_slot("/source")}, "extra": {}},
        ):
            with self.subTest(artifacts=artifacts):
                value = manifest()
                value["actions"]["transform"]["artifacts"] = artifacts
                self.assert_invalid(value)

    def test_rejects_invalid_slot_names_and_open_slot_definitions(self) -> None:
        for name, mutation in (
            ("bad-name!", None),
            ("source", ("unknown", True)),
        ):
            with self.subTest(name=name, mutation=mutation):
                value = manifest()
                slot = artifact_slot("/source")
                if mutation is not None:
                    slot[mutation[0]] = mutation[1]
                value["actions"]["transform"]["artifacts"] = {
                    "inputs": {name: slot}
                }
                self.assert_invalid(value)

    def test_rejects_missing_or_invalid_slot_limits(self) -> None:
        candidates = []
        for missing in ("pointer", "media_types", "max_size_bytes"):
            slot = artifact_slot("/source")
            del slot[missing]
            candidates.append(slot)
        for pointer in (
            "",
            "source",
            "/bad~2escape",
            "/source\n",
            "/source\r",
            "/source\x00",
            "/source\x7f",
        ):
            slot = artifact_slot(pointer)
            candidates.append(slot)
        for media_types in ([], ["image/png", "image/png"], ["text/plain"]):
            slot = artifact_slot("/source")
            slot["media_types"] = media_types
            candidates.append(slot)
        for size in (0, True, 64 * 1024 * 1024 + 1):
            slot = artifact_slot("/source")
            slot["max_size_bytes"] = size
            candidates.append(slot)

        for slot in candidates:
            with self.subTest(slot=slot):
                value = manifest()
                value["actions"]["transform"]["artifacts"] = {
                    "inputs": {"source": slot}
                }
                self.assert_invalid(value)

    def test_rejects_duplicate_names_across_directions(self) -> None:
        value = manifest()
        value["actions"]["transform"]["artifacts"] = {
            "inputs": {"frame": artifact_slot("/source")},
            "outputs": {"frame": artifact_slot("/result")},
        }
        self.assert_invalid(value)

    def test_rejects_duplicate_pointers_within_a_direction(self) -> None:
        for direction in ("inputs", "outputs"):
            with self.subTest(direction=direction):
                value = manifest()
                value["actions"]["transform"]["artifacts"] = {
                    direction: {
                        "first": artifact_slot("/image"),
                        "second": artifact_slot("/image"),
                    }
                }
                self.assert_invalid(value)

    def test_rejects_ancestor_and_descendant_pointers_within_a_direction(self) -> None:
        conflicting_pairs = (
            ("/frame", "/frame/digest"),
            ("/a~0b", "/a~0b/image"),
            ("/a~1b", "/a~1b/image"),
        )
        for direction in ("inputs", "outputs"):
            for first, second in conflicting_pairs:
                with self.subTest(direction=direction, first=first, second=second):
                    value = manifest()
                    value["actions"]["transform"]["artifacts"] = {
                        direction: {
                            "first": artifact_slot(first),
                            "second": artifact_slot(second),
                        }
                    }
                    self.assert_invalid(value)

    def test_accepts_non_overlapping_and_escaped_pointer_boundaries(self) -> None:
        for first, second in (
            ("/a", "/ab"),
            ("/a~1b", "/a/b"),
            ("/a~0b", "/a/b"),
        ):
            with self.subTest(first=first, second=second):
                value = manifest()
                value["actions"]["transform"]["artifacts"] = {
                    "inputs": {
                        "first": artifact_slot(first),
                        "second": artifact_slot(second),
                    }
                }
                self.plugin._validate_manifest(value)


if __name__ == "__main__":
    unittest.main()
