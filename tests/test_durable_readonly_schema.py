"""Schema contracts for opt-in durable read-only action metadata."""

from __future__ import annotations

from copy import deepcopy
import json
from pathlib import Path
import unittest

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.errors import DescriptorError
from ai_auto_desktop.plugin import PluginError, ProcessPlugin


ROOT = Path(__file__).resolve().parents[1]


def _workflow() -> dict[str, object]:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "durable-readonly-schema"},
        "budgets": {"max_duration": "5s", "max_executed_steps": 2},
        "steps": [{
            "id": "observe",
            "type": "action",
            "uses": "fixture.read@1",
            "with": {},
            "effect": {"class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
            "sensitivity": {
                "input": "public", "output": "public",
                "error": "public",
            },
            "checkpoint": {
                "output": {"mode": "project", "fields": ["title"]}
            },
        }],
    }


def _manifest(pointer: str = "/window/title") -> dict[str, object]:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": "fixture", "version": "1.0.0"},
        "actions": {
            "read": {
                "contract_major": 1,
                "effect": {"default_class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
                "input_schema": {"type": "object"},
                "output_schema": {"type": "object"},
                "errors": [{
                    "code": "FIXTURE.NOT_READY", "retryable": False,
                    "effect": "not_applied",
                }],
                "sensitivity": {
                    "input": "public", "output": "public",
                    "error": "public",
                },
                "durability": {
                    "checkpoint_fields": {
                        "title": {
                            "pointer": pointer,
                            "schema": {"type": "string"},
                            "missing": "error",
                        }
                    }
                },
            }
        },
    }


class DurableReadOnlySchemaTests(unittest.TestCase):
    def test_workflow_schema_pins_current_v0_script_and_set_subset(self) -> None:
        schema = json.loads(
            (
                ROOT
                / "schemas"
                / "workflow"
                / "v1alpha1"
                / "workflow.schema.json"
            ).read_text(encoding="utf-8")
        )
        script = schema["$defs"]["scriptStep"]["allOf"][1]["properties"]
        sandbox = schema["$defs"]["sandbox"]["properties"]
        set_assign = schema["$defs"]["setStep"]["allOf"][1]["properties"]["assign"]

        self.assertEqual(script["runtime"], {"const": "python"})
        self.assertNotIn("capabilities", script)
        self.assertEqual(sandbox["network"]["properties"]["mode"], {"const": "deny"})
        self.assertEqual(
            sandbox["filesystem"]["properties"]["mode"], {"const": "deny"}
        )
        self.assertEqual(
            sandbox["environment"]["properties"]["mode"], {"const": "deny"}
        )
        self.assertEqual(
            set_assign["propertyNames"]["pattern"],
            r"^vars\.[A-Za-z_][A-Za-z0-9_]*$",
        )

    def test_workflow_fields_compile_and_are_frozen(self) -> None:
        compiled = compile_descriptor(_workflow())
        action = compiled.steps[0]
        self.assertEqual(action.params["sensitivity"]["output"], "public")
        self.assertEqual(action.params["checkpoint"]["output"]["fields"], ("title",))

    def test_workflow_checkpoint_shapes_are_closed(self) -> None:
        valid = _workflow()
        valid["steps"][0]["checkpoint"] = {"output": {"mode": "omit"}}
        compile_descriptor(valid)
        invalid_outputs = (
            {"mode": "omit", "fields": ["title"]},
            {"mode": "project"},
            {"mode": "project", "fields": []},
            {"mode": "project", "fields": ["title", "title"]},
            {"mode": "other"},
        )
        for output in invalid_outputs:
            with self.subTest(output=output):
                value = _workflow()
                value["steps"][0]["checkpoint"] = {"output": output}
                with self.assertRaises(DescriptorError):
                    compile_descriptor(value)

    def test_workflow_sensitivity_is_closed(self) -> None:
        for sensitivity in (
            {"input": "private"},
            {"input": "public", "unknown": "public"},
        ):
            with self.subTest(sensitivity=sensitivity):
                value = _workflow()
                value["steps"][0]["sensitivity"] = sensitivity
                with self.assertRaises(DescriptorError):
                    compile_descriptor(value)

    def test_manifest_accepts_escaped_nested_and_array_pointers(self) -> None:
        plugin = ProcessPlugin(["unused"])
        for pointer in ("/window/title", "/a~1b/~0key/0", "//0"):
            with self.subTest(pointer=pointer):
                plugin._validate_manifest(_manifest(pointer))

    def test_manifest_rejects_root_bad_escape_long_and_deep_pointers(self) -> None:
        plugin = ProcessPlugin(["unused"])
        pointers = (
            "", "relative", "/bad~2escape",
            "/" + "x" * 1024,
            "/" + "/".join(["x"] * 65),
        )
        for pointer in pointers:
            with self.subTest(pointer=pointer):
                with self.assertRaises(PluginError):
                    plugin._validate_manifest(_manifest(pointer))

    def test_manifest_durability_shape_is_closed(self) -> None:
        plugin = ProcessPlugin(["unused"])
        valid = _manifest()
        valid["actions"]["read"]["durability"]["checkpoint_fields"] = {}
        plugin._validate_manifest(valid)
        mutations = []
        value = _manifest(); value["actions"]["read"]["durability"] = {}
        mutations.append(value)
        value = _manifest(); value["actions"]["read"]["durability"]["checkpoint_fields"] = {"bad-name": {"pointer": "/x", "schema": True}}
        mutations.append(value)
        value = _manifest(); value["actions"]["read"]["durability"]["checkpoint_fields"]["title"]["extra"] = True
        mutations.append(value)
        value = _manifest(); value["actions"]["read"]["sensitivity"]["other"] = "public"
        mutations.append(value)
        for candidate in mutations:
            with self.subTest(candidate=candidate):
                with self.assertRaises(PluginError):
                    plugin._validate_manifest(candidate)

    def test_canonical_and_packaged_schema_mirrors_match(self) -> None:
        for relative in (
            Path("workflow/v1alpha1/workflow.schema.json"),
            Path("capabilities/v1alpha1/capability-manifest.schema.json"),
        ):
            canonical = ROOT / "schemas" / relative
            packaged = ROOT / "src" / "ai_auto_desktop" / "schemas" / relative
            self.assertEqual(canonical.read_bytes(), packaged.read_bytes())
            json.loads(canonical.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
