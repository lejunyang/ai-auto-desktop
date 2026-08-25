"""Tracked workflow example compilation and equivalence tests."""

from __future__ import annotations

from collections import defaultdict
import subprocess
from pathlib import Path
import unittest

from ai_auto_desktop.compiler import load_descriptor


PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXAMPLES_DIR = PROJECT_ROOT / "examples" / "workflows"


def _tracked_example_paths() -> list[Path]:
    completed = subprocess.run(
        ["git", "ls-files", "examples/workflows"],
        cwd=PROJECT_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    paths = [
        PROJECT_ROOT / line
        for line in completed.stdout.splitlines()
        if line.endswith((".yaml", ".yml", ".json"))
    ]
    return sorted(paths)


class WorkflowExampleTests(unittest.TestCase):
    def test_all_tracked_examples_compile(self) -> None:
        example_paths = _tracked_example_paths()
        self.assertTrue(example_paths, "expected tracked workflow examples")
        for path in example_paths:
            with self.subTest(example=path.relative_to(PROJECT_ROOT).as_posix()):
                descriptor = load_descriptor(path)
                self.assertEqual(descriptor.source, path.resolve())

    def test_yaml_json_pairs_are_raw_equivalent(self) -> None:
        grouped: dict[str, dict[str, Path]] = defaultdict(dict)
        for path in _tracked_example_paths():
            stem = str(path.relative_to(EXAMPLES_DIR).with_suffix(""))
            grouped[stem][path.suffix.lower()] = path

        paired = 0
        for stem, variants in sorted(grouped.items()):
            yaml_path = variants.get(".yaml") or variants.get(".yml")
            json_path = variants.get(".json")
            if yaml_path is None or json_path is None:
                continue
            paired += 1
            with self.subTest(example=stem):
                yaml_descriptor = load_descriptor(yaml_path)
                json_descriptor = load_descriptor(json_path)
                self.assertEqual(yaml_descriptor.raw, json_descriptor.raw)

        self.assertGreater(paired, 0, "expected at least one JSON/YAML example pair")


if __name__ == "__main__":
    unittest.main()
