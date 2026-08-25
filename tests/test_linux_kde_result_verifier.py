"""Synthetic contract and security tests for the KDE result verifier."""

from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
VERIFIER = PROJECT_ROOT / "tests" / "linux" / "verify-kde-result.sh"
VERIFIER_PY = PROJECT_ROOT / "tests" / "linux" / "verify-kde-result.py"
SPEC = importlib.util.spec_from_file_location(
    "testable_kde_result_verifier", VERIFIER_PY
)
assert SPEC is not None and SPEC.loader is not None
verifier = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verifier
SPEC.loader.exec_module(verifier)


def _field(non_null: int, non_empty: int, total: int) -> dict[str, object]:
    def ratio(value: int) -> float | None:
        return None if total == 0 else round(value / total, 4)

    return {
        "non_null": non_null,
        "non_empty": non_empty,
        "non_null_ratio": ratio(non_null),
        "non_empty_ratio": ratio(non_empty),
    }


def _application(name: str, pid: int) -> dict[str, object]:
    element_count = 2
    known_by_state = {state: element_count for state in verifier.STATE_NAMES}
    return {
        "application": name,
        "status": "supported",
        "support_level": "observed_read_only",
        "executable": f"/usr/bin/{name}",
        "version": {"available": True, "returncode": 0, "output": "1.0"},
        "launch_pid": pid,
        "pid_selection": "exact_popen_pid",
        "registration_latency_ms": 20.0,
        "snapshot_latency_ms": 30.0,
        "snapshot": {
            "selector": {
                "process_id": pid,
                "bus_name": f":1.{pid}",
                "toolkit_name": "Qt",
            },
            "max_depth": 32,
            "max_nodes": 2000,
            "truncated": False,
            "encoded_bytes": 1234,
            "completeness": {
                "element_count": element_count,
                "role": {
                    "non_empty": element_count,
                    "non_empty_ratio": 1.0,
                    "counts": {"application": 1, "push_button": 1},
                },
                "name": _field(2, 1, element_count),
                "value": _field(1, 1, element_count),
                "description": _field(2, 0, element_count),
                "state": {
                    "known": element_count * len(verifier.STATE_NAMES),
                    "possible": element_count * len(verifier.STATE_NAMES),
                    "known_ratio": 1.0,
                    "known_by_state": known_by_state,
                },
                "semantic_actions": {
                    "node_action_counts": {"focus": 1, "invoke": 1},
                    "native_action_name_counts": {"Press": 1},
                },
            },
            "content_retention": "aggregate_only_no_ui_text",
        },
        "errors": [],
        "writes_dispatched": [],
        "driver_version": "0.1.0",
        "backend": "pygobject_atspi",
        "private_registry_baseline_count": 0,
        "launch_args": [],
        "atspi_application": {
            "bus_name": f":1.{pid}",
            "object_path": "/org/a11y/atspi/accessible/root",
            "name": name,
            "process_id": pid,
            "toolkit_name": "Qt",
            "toolkit_version": "5.15.8",
            "atspi_version": None,
            "locale": None,
        },
        "cleanup": {
            "owned_process_group_stopped": True,
            "returncode": -15,
        },
    }


def _report() -> dict[str, object]:
    applications = [
        _application(name, 4100 + index)
        for index, name in enumerate(verifier._expected_applications())
    ]
    return {
        "schema_version": verifier.REPORT_SCHEMA_VERSION,
        "generated_at": "2026-08-25T02:18:15.527030+00:00",
        "status": "completed",
        "environment": {
            "session_type": "x11",
            "desktop": "KDE",
            "display": ":10.0",
            "private_session_bus": True,
            "private_home_and_xdg_dirs": True,
            "inherited_at_spi_bus_address": False,
        },
        "host": {"os": "test"},
        "safety": {
            "existing_windows_selected": False,
            "application_selector": verifier.EXACT_SELECTOR_DESCRIPTION,
            "write_actions_enabled": False,
            "write_actions_dispatched": 0,
            "screenshots_or_ocr": False,
            "node_ui_text_retained": False,
        },
        "limits": {
            "registration_timeout_seconds": 15.0,
            "snapshot_timeout_seconds": 15.0,
            "max_depth": 32,
            "max_nodes": 2000,
        },
        "summary": {
            "total": len(applications),
            "supported": len(applications),
            "unsupported": 0,
            "error": 0,
            "duration_ms": 123.4,
        },
        "applications": applications,
    }


class KdeResultVerifierTests(unittest.TestCase):
    maxDiff = None

    def _run_document(
        self, document: dict[str, object]
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "qualification.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            completed = subprocess.run(
                [str(VERIFIER), str(path)], text=True, capture_output=True, check=False
            )
        self.assertEqual(completed.stderr, "")
        return completed, json.loads(completed.stdout)

    def _assert_rejected(
        self, document: dict[str, object], code: str
    ) -> dict[str, object]:
        completed, result = self._run_document(document)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["qualified"], False)
        self.assertEqual(result["error"]["code"], code)
        return result

    def test_complete_read_only_report_is_qualified(self) -> None:
        completed, result = self._run_document(_report())
        self.assertEqual(completed.returncode, 0)
        self.assertEqual(result["schema_version"], verifier.VERIFIER_SCHEMA_VERSION)
        self.assertIs(result["report_valid"], True)
        self.assertIs(result["qualified"], True)
        self.assertEqual(
            result["expected_applications"], list(verifier._expected_applications())
        )

    def test_expected_application_set_tracks_current_qualifier(self) -> None:
        names = verifier._expected_applications()
        self.assertIn("dolphin", names)
        self.assertIn("qml-fixture", names)
        self.assertEqual(len(names), len(set(names)))

    def test_wrong_schema_and_duplicate_json_key_are_rejected(self) -> None:
        report = _report()
        report["schema_version"] = "future"
        self._assert_rejected(report, "unsupported_report_schema")
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "duplicate.json"
            path.write_text(
                '{"schema_version":"a","schema_version":"b"}',
                encoding="utf-8",
            )
            completed = subprocess.run(
                [str(VERIFIER), str(path)], text=True, capture_output=True, check=False
            )
        result = json.loads(completed.stdout)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(result["error"]["code"], "duplicate_json_key")

    def test_summary_and_application_set_must_match(self) -> None:
        report = _report()
        report["summary"]["supported"] -= 1
        self._assert_rejected(report, "summary_inconsistent")

        report = _report()
        report["applications"].pop()
        report["summary"]["total"] -= 1
        report["summary"]["supported"] -= 1
        self._assert_rejected(report, "application_set_mismatch")

        report = _report()
        report["applications"].append(copy.deepcopy(report["applications"][0]))
        report["summary"]["total"] += 1
        report["summary"]["supported"] += 1
        self._assert_rejected(report, "duplicate_application")

    def test_unsupported_and_error_application_fail_closed(self) -> None:
        for status in ("unsupported", "error"):
            with self.subTest(status=status):
                report = _report()
                application = report["applications"][0]
                application["status"] = status
                application["support_level"] = "none"
                application["errors"] = [{"stage": "qualify", "code": "failed"}]
                report["summary"]["supported"] -= 1
                report["summary"][status] += 1
                self._assert_rejected(report, f"application_{status}")

    def test_support_snapshot_bounds_and_exact_pid_fail_closed(self) -> None:
        cases = (
            ("support_level", "none", "invalid_support_level"),
            ("snapshot.truncated", True, "snapshot_truncated"),
            ("snapshot.max_nodes", 5001, "snapshot_bounds_invalid"),
            ("snapshot.selector.process_id", 9999, "inexact_pid_selector"),
        )
        for dotted_path, value, code in cases:
            with self.subTest(path=dotted_path):
                report = _report()
                target = report["applications"][0]
                parts = dotted_path.split(".")
                for part in parts[:-1]:
                    target = target[part]
                target[parts[-1]] = value
                self._assert_rejected(report, code)

    def test_content_retention_safety_and_cleanup_fail_closed(self) -> None:
        cases = (
            ("snapshot.content_retention", "full_nodes", "unsafe_content_retention"),
            ("snapshot.nodes", [], "unsafe_content_retention"),
            ("cleanup.owned_process_group_stopped", False, "cleanup_failed"),
            ("cleanup.returncode", None, "cleanup_not_proven"),
        )
        for dotted_path, value, code in cases:
            with self.subTest(path=dotted_path):
                report = _report()
                target = report["applications"][0]
                parts = dotted_path.split(".")
                for part in parts[:-1]:
                    target = target[part]
                target[parts[-1]] = value
                self._assert_rejected(report, code)

    def test_top_level_safety_and_write_evidence_fail_closed(self) -> None:
        report = _report()
        report["safety"]["existing_windows_selected"] = True
        self._assert_rejected(report, "existing_windows_selected")

        report = _report()
        report["applications"][0]["writes_dispatched"] = [{"action": "invoke"}]
        report["safety"]["write_actions_dispatched"] = 1
        self._assert_rejected(report, "write_actions_dispatched")

    def test_top_level_unsupported_report_is_valid_but_not_qualified(self) -> None:
        completed, result = self._run_document({
            "schema_version": verifier.REPORT_SCHEMA_VERSION,
            "generated_at": "2026-08-25T02:18:15+00:00",
            "status": "unsupported",
            "errors": [{"stage": "preflight", "code": "no_session"}],
            "applications": [],
        })
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["report_valid"], True)
        self.assertEqual(result["error"]["code"], "report_unsupported")

    def test_symlink_non_regular_empty_and_oversize_inputs_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            valid = root / "valid.json"
            valid.write_text(json.dumps(_report()), encoding="utf-8")
            link = root / "link.json"
            link.symlink_to(valid)
            directory = root / "directory"
            directory.mkdir()
            empty = root / "empty.json"
            empty.touch()
            large = root / "large.json"
            large.write_bytes(b" " * (verifier.MAX_REPORT_BYTES + 1))
            cases = (
                (link, "input_symlink"),
                (directory, "input_not_regular"),
                (empty, "input_empty"),
                (large, "input_too_large"),
            )
            for path, code in cases:
                with self.subTest(code=code):
                    completed = subprocess.run(
                        [str(VERIFIER), str(path)],
                        text=True, capture_output=True, check=False,
                    )
                    result = json.loads(completed.stdout)
                    self.assertNotEqual(completed.returncode, 0)
                    self.assertEqual(result["error"]["code"], code)


if __name__ == "__main__":
    unittest.main()
