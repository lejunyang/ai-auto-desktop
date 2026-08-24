"""End-to-end tests against real Win32 controls and Windows UI Automation."""

from __future__ import annotations

import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time
import unittest
import uuid

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.plugin import PluginError, ProcessPlugin
from ai_auto_desktop.runtime import run_descriptor


PROJECT_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_APP = PROJECT_ROOT / "tests" / "windows" / "uia_fixture_app.py"
DRIVER_DIRECTORY = PROJECT_ROOT / "plugins" / "windows_uia"
DRIVER_LAUNCHER = DRIVER_DIRECTORY / "run.cmd"

ACTION_PREFIX = "desktop.windows_uia"
INITIAL_STATUS = "Status: idle"
INVOKED_STATUS = "Status: invoked"
DUPLICATE_BUTTON_NAME = "Duplicate action"
RUNTIME_EDIT_VALUE = "Observed through Runtime"


def action(name: str) -> str:
    return f"{ACTION_PREFIX}.{name}@1"


@unittest.skipUnless(sys.platform == "win32", "requires native Windows UI Automation")
class NativeWindowsUIATests(unittest.TestCase):
    """Exercise the packaged process launcher against a real Win32 tree."""

    def setUp(self) -> None:
        self.title = f"AI Auto Desktop UIA Fixture {uuid.uuid4().hex}"
        self.fixture = subprocess.Popen(
            [sys.executable, str(FIXTURE_APP), "--title", self.title],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        self.addCleanup(self._stop_fixture)
        self.ready = self._read_fixture_ready()

        # ProcessPlugin deliberately never invokes a shell itself.  cmd.exe is
        # therefore explicit, while run.cmd remains the tested entrypoint.
        command_processor = os.environ.get("COMSPEC", r"C:\Windows\System32\cmd.exe")
        self.plugin = ProcessPlugin(
            [command_processor, "/D", "/S", "/C", DRIVER_LAUNCHER.name],
            cwd=DRIVER_DIRECTORY,
            timeout=30.0,
            name="desktop.windows_uia native test",
        )
        self.addCleanup(self.plugin.close)

    def _read_fixture_ready(self) -> dict[str, object]:
        assert self.fixture.stdout is not None
        raw = self.fixture.stdout.readline()
        if not raw:
            stderr = self.fixture.stderr.read() if self.fixture.stderr is not None else ""
            self.fail(f"Win32 fixture exited before readiness: {stderr}")
        ready = json.loads(raw)
        self.assertIs(ready.get("ready"), True)
        self.assertEqual(ready.get("title"), self.title)
        return ready

    def _stop_fixture(self) -> None:
        fixture = getattr(self, "fixture", None)
        if fixture is None or fixture.poll() is not None:
            return
        fixture.terminate()
        try:
            fixture.communicate(timeout=5.0)
        except subprocess.TimeoutExpired:
            fixture.kill()
            fixture.communicate(timeout=5.0)

    def _wait_for_window(self) -> dict[str, object]:
        deadline = time.monotonic() + 10.0
        while True:
            result = self.plugin.invoke(action("list_windows"), {})
            matches = [
                item
                for item in result["windows"]
                if item["window"].get("title") == self.title
                and item["window"].get("process_id") == self.fixture.pid
            ]
            if len(matches) == 1:
                return matches[0]["window"]
            if time.monotonic() >= deadline:
                self.fail(f"expected one fixture window, found {len(matches)}")
            time.sleep(0.1)

    def _snapshot(self, selector: dict[str, object]) -> dict[str, object]:
        return self.plugin.invoke(
            action("snapshot"),
            {"window": selector, "max_depth": 12, "max_nodes": 100},
        )

    def _find(
        self, snapshot: dict[str, object], locator: dict[str, object]
    ) -> dict[str, object]:
        return self.plugin.invoke(
            action("find"),
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": locator,
            },
        )

    @staticmethod
    def _nodes(
        snapshot: dict[str, object], *, role: str, name: str | None = None
    ) -> list[dict[str, object]]:
        return [
            node
            for node in snapshot["nodes"]
            if node.get("role") == role
            and (name is None or node.get("name") == name)
        ]

    def test_real_process_driver_observes_and_operates_fixture(self) -> None:
        manifest = self.plugin.start()
        self.assertEqual(manifest["metadata"]["name"], ACTION_PREFIX)

        window = self._wait_for_window()
        self.assertEqual(window["handle"], self.ready["window_handle"])
        selector = {
            "handle": window["handle"],
            "title": self.title,
            "process_id": self.fixture.pid,
        }
        snapshot = self._snapshot(selector)
        self.assertFalse(snapshot["truncated"])
        self.assertEqual(snapshot["window"]["title"], self.title)

        duplicate_nodes = self._nodes(
            snapshot, role="button", name=DUPLICATE_BUTTON_NAME
        )
        self.assertEqual(len(duplicate_nodes), 2)
        duplicate_locator = {"role": "button", "name": DUPLICATE_BUTTON_NAME}
        with self.assertRaises(PluginError) as ambiguous_find:
            self._find(snapshot, duplicate_locator)
        self.assertEqual(ambiguous_find.exception.code, "DRIVER.AMBIGUOUS")
        self.assertEqual(ambiguous_find.exception.details["candidate_count"], 2)

        # A write with the same ambiguous locator must fail before native
        # InvokePattern dispatch.  The duplicate buttons would change the
        # status text if either one were invoked.
        duplicate_target = {
            "snapshot_id": snapshot["snapshot_id"],
            "revision": snapshot["revision"],
            "node_id": duplicate_nodes[0]["node_id"],
        }
        with self.assertRaises(PluginError) as ambiguous_invoke:
            self.plugin.invoke(
                action("invoke"),
                {"target": duplicate_target, "locator": duplicate_locator},
            )
        self.assertEqual(ambiguous_invoke.exception.code, "DRIVER.AMBIGUOUS")
        snapshot = self._snapshot(selector)
        self.assertEqual(len(self._nodes(snapshot, role="text", name=INITIAL_STATUS)), 1)

        edit_locator = {"role": "edit"}
        edit = self._find(snapshot, edit_locator)
        self.assertIn("set_value", edit["node"]["actions"])
        set_result = self.plugin.invoke(
            action("set_value"),
            {"target": edit["target"], "locator": edit_locator, "value": "Final"},
        )
        self.assertEqual(set_result["backend_result"]["native_pattern"], "ValuePattern")

        snapshot = self._snapshot(selector)
        edit = self._find(snapshot, edit_locator)
        self.assertEqual(edit["node"]["value"], "Final")
        focus_result = self.plugin.invoke(
            action("focus"),
            {"target": edit["target"], "locator": edit_locator},
        )
        self.assertEqual(focus_result["backend_result"]["native_pattern"], "SetFocus")

        snapshot = self._snapshot(selector)
        focused_edit = self._find(snapshot, edit_locator)
        self.assertIs(focused_edit["node"]["states"]["focused"], True)

        button_locator = {"role": "button", "name": "Apply fixture value"}
        button = self._find(snapshot, button_locator)
        self.assertIn("invoke", button["node"]["actions"])
        invoke_result = self.plugin.invoke(
            action("invoke"),
            {"target": button["target"], "locator": button_locator},
        )
        self.assertEqual(
            invoke_result["backend_result"]["native_pattern"], "InvokePattern"
        )

        final_snapshot = self._snapshot(selector)
        self.assertEqual(
            len(self._nodes(final_snapshot, role="text", name=INVOKED_STATUS)), 1
        )
        final_edit = self._find(final_snapshot, edit_locator)
        self.assertEqual(final_edit["node"]["value"], "Final")

    def test_runtime_write_postcondition_observes_fresh_native_snapshot(self) -> None:
        self.plugin.start()
        window = self._wait_for_window()
        selector = {
            "handle": window["handle"],
            "title": self.title,
            "process_id": self.fixture.pid,
        }
        initial_snapshot = self._snapshot(selector)
        self.assertFalse(initial_snapshot["truncated"])

        edit_locator = {"role": "edit"}
        edit = self._find(initial_snapshot, edit_locator)
        self.assertNotEqual(edit["node"]["value"], RUNTIME_EDIT_VALUE)
        edit_indexes = [
            index
            for index, node in enumerate(initial_snapshot["nodes"])
            if node.get("node_id") == edit["node"]["node_id"]
        ]
        self.assertEqual(len(edit_indexes), 1)
        edit_index = edit_indexes[0]

        descriptor = compile_descriptor(
            {
                "apiVersion": "ai-auto-desktop.dev/v1alpha1",
                "kind": "Workflow",
                "metadata": {"name": "native-windows-uia-observation"},
                "requires": {
                    "platforms": ["windows"],
                    "permissions": ["desktop.observe", "desktop.input"],
                },
                "budgets": {
                    "max_duration": "20s",
                    "max_executed_steps": 1,
                    "cleanup_timeout": "1s",
                },
                "steps": [
                    {
                        "id": "set_fixture_value",
                        "type": "action",
                        "uses": action("set_value"),
                        "with": {
                            "target": edit["target"],
                            "locator": edit_locator,
                            "value": RUNTIME_EDIT_VALUE,
                        },
                        "effect": {"class": "contextual"},
                        "risk": {"category": "input", "level": "high"},
                        "timeout": "15s",
                        "postcondition": {
                            "observe": {
                                "uses": action("snapshot"),
                                "with": {
                                    "window": selector,
                                    "max_depth": 12,
                                    "max_nodes": 100,
                                },
                            },
                            "condition": (
                                "${{ observation.truncated == False and "
                                f"observation.nodes[{edit_index}].node_id == "
                                f"'{edit['node']['node_id']}' and "
                                f"observation.nodes[{edit_index}].role == 'edit' and "
                                f"observation.nodes[{edit_index}].value == "
                                f"'{RUNTIME_EDIT_VALUE}'"
                                " }}"
                            ),
                            "timeout": "5s",
                            "poll_interval": "100ms",
                            "message": (
                                "fresh UIA snapshot did not contain the new value"
                            ),
                        },
                    }
                ],
            }
        )

        result = run_descriptor(
            descriptor,
            plugins={ACTION_PREFIX: self.plugin},
            granted_permissions=["desktop.observe", "desktop.input"],
        )

        self.assertTrue(result.ok, result.to_dict())
        self.assertEqual(result.steps["set_fixture_value"]["attempts"], 1)
        self.assertEqual(
            result.steps["set_fixture_value"]["output"]["backend_result"][
                "native_pattern"
            ],
            "ValuePattern",
        )


if __name__ == "__main__":
    unittest.main()
