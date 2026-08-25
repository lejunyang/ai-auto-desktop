"""Deterministic contract tests for the KDE/X11 application qualifier."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


QUALIFIER_PATH = Path(__file__).parent / "linux" / "kde_app_qualifier.py"
SPEC = importlib.util.spec_from_file_location("testable_kde_app_qualifier", QUALIFIER_PATH)
assert SPEC is not None and SPEC.loader is not None
qualifier = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = qualifier
SPEC.loader.exec_module(qualifier)


class KdeAppQualifierContractTests(unittest.TestCase):
    def test_semantic_completeness_is_aggregate_and_counts_actions(self) -> None:
        states = {name: None for name in qualifier.STATE_NAMES}
        states["enabled"] = True
        states["focused"] = False
        summary = qualifier.summarize_nodes(
            [
                {
                    "role": "push_button",
                    "name": "Secret UI label is not retained",
                    "description": None,
                    "value": None,
                    "states": states,
                    "actions": ["focus", "invoke"],
                    "provenance": {"native_action_name": "Press"},
                },
                {
                    "role": "text",
                    "name": "",
                    "description": "description",
                    "value": "value",
                    "states": {name: None for name in qualifier.STATE_NAMES},
                    "actions": ["focus", "set_text"],
                    "provenance": {},
                },
            ]
        )
        self.assertEqual(summary["element_count"], 2)
        self.assertEqual(summary["role"]["counts"], {"push_button": 1, "text": 1})
        self.assertEqual(summary["name"]["non_empty"], 1)
        self.assertEqual(summary["value"]["non_empty"], 1)
        self.assertEqual(summary["state"]["known"], 2)
        self.assertEqual(
            summary["semantic_actions"]["node_action_counts"],
            {"focus": 2, "invoke": 1, "set_text": 1},
        )
        self.assertNotIn("Secret UI label is not retained", repr(summary))

    def test_cli_caps_registration_wait_and_snapshot_bounds(self) -> None:
        with self.assertRaises(SystemExit):
            qualifier._parse_args(["--registration-timeout", "15.01"])
        with self.assertRaises(SystemExit):
            qualifier._parse_args(["--max-nodes", "5001"])
        parsed = qualifier._parse_args([])
        self.assertEqual(parsed.registration_timeout, 15.0)
        self.assertEqual(
            parsed.app,
            ["dolphin", "konsole", "qml-fixture", "system-settings"],
        )

    def test_dolphin_is_scoped_to_a_private_fixture_directory(self) -> None:
        spec = qualifier.APP_SPECS["dolphin"]
        self.assertEqual(spec["executables"], ("dolphin",))
        self.assertIn("{fixture_dir}", spec["launch_args"])
        self.assertNotIn("~", spec["launch_args"])

    def test_qml_fixture_is_an_owned_local_source(self) -> None:
        spec = qualifier.APP_SPECS["qml-fixture"]
        self.assertEqual(spec["executables"], ("qmlscene", "qmlscene-qt5"))
        source = Path(spec["launch_args"][0])
        self.assertEqual(source, qualifier.QML_FIXTURE_PATH)
        self.assertTrue(source.is_file())
        self.assertIn("Accessible.name", source.read_text(encoding="utf-8"))

    def test_qualification_environment_drops_unrelated_credentials(self) -> None:
        environment = qualifier.sanitized_environment({
            "PATH": "/usr/bin:/bin",
            "DISPLAY": ":10.0",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/tmp/bus",
            "XDG_RUNTIME_DIR": "/tmp/private-runtime",
            "AWS_SECRET_ACCESS_KEY": "must-not-reach-fixtures",
            "TOKEN": "must-not-reach-fixtures",
        })
        self.assertEqual(environment["DISPLAY"], ":10.0")
        self.assertEqual(
            environment["PATH"],
            "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin",
        )
        self.assertIn("DBUS_SESSION_BUS_ADDRESS", environment)
        self.assertEqual(environment["XDG_RUNTIME_DIR"], "/tmp/private-runtime")
        self.assertEqual(environment["LANG"], "C.UTF-8")
        self.assertNotIn("AWS_SECRET_ACCESS_KEY", environment)
        self.assertNotIn("TOKEN", environment)

    def test_no_write_action_is_present_in_qualification_source(self) -> None:
        source = QUALIFIER_PATH.read_text(encoding="utf-8")
        for action in ("focus", "invoke", "set_text", "type_text", "toggle", "expand", "collapse"):
            self.assertNotIn(f'linux_atspi.{action}@1"', source)


if __name__ == "__main__":
    unittest.main()
