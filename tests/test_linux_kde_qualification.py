"""Deterministic contract tests for the KDE/X11 application qualifier."""

from __future__ import annotations

import importlib.util
import copy
import os
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest import mock


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
        self.assertEqual(
            qualifier.sanitized_environment(environment)["XDG_RUNTIME_DIR"],
            "/tmp/private-runtime",
        )
        self.assertEqual(environment["LANG"], "C.UTF-8")
        self.assertNotIn("AWS_SECRET_ACCESS_KEY", environment)
        self.assertNotIn("TOKEN", environment)

    def test_inside_private_bus_exit_reflects_qualification_result(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "result.json"
            base = {
                "status": "completed",
                "summary": {
                    "total": len(qualifier.APP_SPECS),
                    "supported": len(qualifier.APP_SPECS),
                    "unsupported": 0,
                    "error": 0,
                },
                "applications": [
                    {
                        "application": name,
                        "status": "supported",
                        "support_level": "observed_read_only",
                        "snapshot": {"truncated": False},
                        "writes_dispatched": [],
                        "cleanup": {"owned_process_group_stopped": True},
                    }
                    for name in qualifier.APP_SPECS
                ],
            }
            args = ["--inside-private-bus", "--output", str(output)]
            with mock.patch.object(qualifier, "run_qualification", return_value=base), mock.patch("builtins.print"):
                self.assertEqual(qualifier.main(args), 0)
            self.assertEqual(
                json.loads(output.read_text(encoding="utf-8"))["status"],
                "completed",
            )

            for mutation in (
                lambda report: report["summary"].update(unsupported=1, supported=len(qualifier.APP_SPECS) - 1),
                lambda report: report["applications"][0]["snapshot"].update(truncated=True),
                lambda report: report["applications"][0].update(writes_dispatched=["invoke"]),
                lambda report: report["applications"][0]["cleanup"].update(owned_process_group_stopped=False),
            ):
                with self.subTest(mutation=mutation):
                    failed = copy.deepcopy(base)
                    mutation(failed)
                    with mock.patch.object(qualifier, "run_qualification", return_value=failed), mock.patch("builtins.print"):
                        self.assertNotEqual(qualifier.main(args), 0)
                    self.assertEqual(
                        json.loads(output.read_text(encoding="utf-8"))["status"],
                        "completed",
                    )

            subset = copy.deepcopy(base)
            subset["applications"] = [
                application for application in subset["applications"]
                if application["application"] == "konsole"
            ]
            subset["summary"].update(total=1, supported=1)
            with mock.patch.object(qualifier, "run_qualification", return_value=subset), mock.patch("builtins.print"):
                self.assertNotEqual(
                    qualifier.main([*args, "--app", "konsole"]), 0
                )
            self.assertEqual(
                json.loads(output.read_text(encoding="utf-8"))["applications"][0]["application"],
                "konsole",
            )

    @unittest.skipUnless(Path("/proc").is_dir(), "requires Linux /proc")
    def test_private_qualifier_timeout_kills_exact_descendant_tree(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            child_pid_file = Path(temporary) / "child.pid"
            script = (
                "import pathlib, subprocess, sys, time; "
                "child=subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], "
                "start_new_session=True); "
                "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); time.sleep(60)"
            )
            returncode, timed_out, cleanup_succeeded = qualifier._run_private_qualifier(
                [sys.executable, "-c", script, str(child_pid_file)],
                os.environ.copy(),
                0.5,
                termination_grace=0.5,
            )
            self.assertIsNone(returncode)
            self.assertIs(timed_out, True)
            self.assertIs(cleanup_succeeded, True)
            child_pid = int(child_pid_file.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline:
                try:
                    os.kill(child_pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.02)
            else:
                self.fail(f"timeout left descendant PID {child_pid} alive")

    def test_outer_private_runner_failure_still_replaces_output_with_json(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "result.json"
            output.write_text('{"status":"stale"}', encoding="utf-8")
            with mock.patch.object(
                qualifier, "discover_kde_x11_session",
                return_value={
                    "DISPLAY": ":1",
                    "XDG_CURRENT_DESKTOP": "KDE",
                    "XDG_SESSION_TYPE": "x11",
                },
            ), mock.patch.object(
                qualifier.shutil, "which", return_value="/usr/bin/dbus-run-session"
            ), mock.patch.object(
                qualifier, "_run_private_qualifier",
                return_value=(None, True, True),
            ), mock.patch("builtins.print"):
                self.assertNotEqual(
                    qualifier.main(["--output", str(output)]), 0
                )
            report = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "error")
            self.assertEqual(
                report["errors"][0]["code"], "private_qualifier_timeout"
            )
            self.assertIs(report["errors"][0]["cleanup_succeeded"], True)

    def test_no_write_action_is_present_in_qualification_source(self) -> None:
        source = QUALIFIER_PATH.read_text(encoding="utf-8")
        for action in ("focus", "invoke", "set_text", "type_text", "toggle", "expand", "collapse"):
            self.assertNotIn(f'linux_atspi.{action}@1"', source)


if __name__ == "__main__":
    unittest.main()
