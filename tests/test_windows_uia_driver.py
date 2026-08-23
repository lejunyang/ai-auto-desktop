"""Contract tests for the Windows UIA process driver vertical slice."""

from __future__ import annotations

import copy
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import time
import unittest

from ai_auto_desktop.plugin import PluginError, ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
DRIVER_PATH = PROJECT_ROOT / "plugins" / "windows_uia" / "windows_uia_driver.py"

SPEC = importlib.util.spec_from_file_location("testable_windows_uia_driver", DRIVER_PATH)
assert SPEC is not None and SPEC.loader is not None
uia = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = uia
SPEC.loader.exec_module(uia)


def deadline() -> float:
    return time.monotonic() + 5.0


def node(
    key: str,
    parent_index: int | None,
    role: str,
    name: str | None,
    *,
    value: str | None = None,
    actions: tuple[str, ...] = (),
    automation_id: str | None = None,
) -> object:
    return uia.BackendNode(
        native=key,
        parent_index=parent_index,
        role=role,
        name=name,
        value=value,
        states={
            "enabled": True,
            "offscreen": False,
            "focusable": "focus" in actions,
            "focused": False,
            "read_only": False if "set_value" in actions else None,
        },
        bounds={"x": 10, "y": 20, "width": 120, "height": 30},
        actions=actions,
        provenance={
            "automation_id": automation_id,
            "class_name": "FakeControl",
            "framework_id": "Fake",
            "process_id": 7,
            "runtime_id": [42, key],
        },
    )


def default_tree() -> list[object]:
    return [
        node("root", None, "window", "Editor", actions=("focus",), automation_id="main"),
        node("save", 0, "button", "Save", actions=("focus", "invoke"), automation_id="save"),
        node("title", 0, "edit", "Title", value="Draft", actions=("focus", "set_value"), automation_id="title"),
    ]


class FakeBackend:
    name = "fake_windows_uia"

    def __init__(self, snapshots: list[list[object]] | None = None) -> None:
        self.snapshots = snapshots or [default_tree()]
        self.capture_count = 0
        self.calls: list[tuple[object, ...]] = []
        self.fail: str | None = None

    def list_windows(self, *, include_invisible: bool, deadline: float) -> list[dict[str, object]]:
        self.calls.append(("list_windows", include_invisible))
        return [
            {
                "app": {"process_id": 7, "executable": "fake.exe"},
                "window": {
                    "handle": 101,
                    "title": "Editor",
                    "class_name": "FakeWindow",
                    "process_id": 7,
                    "visible": True,
                    "enabled": True,
                },
            }
        ]

    def capture(
        self, window: object, *, max_depth: int, max_nodes: int, deadline: float
    ) -> object:
        self.calls.append(("capture", copy.deepcopy(window), max_depth, max_nodes))
        index = min(self.capture_count, len(self.snapshots) - 1)
        self.capture_count += 1
        return uia.BackendSnapshot(
            app={"process_id": 7, "executable": "fake.exe"},
            window={"handle": 101, "title": "Editor", "process_id": 7},
            nodes=copy.deepcopy(self.snapshots[index]),
        )

    def _action(self, action: str, native: object, value: str | None = None) -> dict[str, object]:
        self.calls.append((action, native, value))
        if self.fail == action:
            raise RuntimeError("synthetic native failure")
        return {"native_pattern": action}

    def focus(self, native: object, *, deadline: float) -> object:
        return self._action("focus", native)

    def invoke(self, native: object, *, deadline: float) -> object:
        return self._action("invoke", native)

    def set_value(self, native: object, value: str, *, deadline: float) -> object:
        return self._action("set_value", native, value)

    def same_element(
        self, previous: object, current: object, *, deadline: float
    ) -> bool:
        self.calls.append(("same_element", previous, current))
        return previous == current


class WindowsUIADriverCoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.backend = FakeBackend()
        self.driver = uia.WindowsUIADriver(self.backend)

    def snapshot(self) -> dict[str, object]:
        return self.driver.execute(
            "snapshot", {"window": {"handle": 101}}, deadline=deadline()
        )

    def find(self, snapshot: dict[str, object], locator: dict[str, object]) -> dict[str, object]:
        return self.driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": locator,
            },
            deadline=deadline(),
        )

    def test_list_windows_and_normalized_snapshot_contract(self) -> None:
        windows = self.driver.execute("list_windows", {}, deadline=deadline())
        self.assertEqual(windows["windows"][0]["window"]["handle"], 101)

        snapshot = self.snapshot()
        self.assertEqual(snapshot["revision"], 1)
        self.assertTrue(str(snapshot["snapshot_id"]).endswith(":1"))
        self.assertEqual(snapshot["app"]["process_id"], 7)
        self.assertEqual(snapshot["window"]["title"], "Editor")
        self.assertEqual([item["node_id"] for item in snapshot["nodes"]], ["n0", "n1", "n2"])
        save = snapshot["nodes"][1]
        self.assertEqual(save["parent_id"], "n0")
        self.assertEqual(save["role"], "button")
        self.assertEqual(save["name"], "Save")
        self.assertEqual(save["value"], None)
        self.assertEqual(save["states"]["enabled"], True)
        self.assertEqual(save["bounds"], {"x": 10, "y": 20, "width": 120, "height": 30})
        self.assertEqual(save["actions"], ["focus", "invoke"])
        self.assertEqual(save["provenance"]["backend"], "fake_windows_uia")

    def test_find_defaults_to_exact_and_never_selects_first_ambiguous_node(self) -> None:
        duplicate = default_tree()
        duplicate.append(
            node("save2", 0, "button", "Save", actions=("invoke",), automation_id="save2")
        )
        self.driver = uia.WindowsUIADriver(FakeBackend([duplicate]))
        snapshot = self.snapshot()

        with self.assertRaises(uia.DriverError) as missing:
            self.find(snapshot, {"name": "save"})
        self.assertEqual(missing.exception.code, "DRIVER.NOT_FOUND")

        with self.assertRaises(uia.DriverError) as ambiguous:
            self.find(snapshot, {"role": "button", "name": "Save"})
        self.assertEqual(ambiguous.exception.code, "DRIVER.AMBIGUOUS")
        self.assertEqual(ambiguous.exception.data["candidate_count"], 2)
        self.assertFalse(any(call[0] == "invoke" for call in self.driver.backend.calls))

        found = self.find(snapshot, {"automation_id": "save2"})
        self.assertEqual(found["target"]["node_id"], "n3")

    def test_all_native_write_actions_re_resolve_before_dispatch(self) -> None:
        cases = (
            ("focus", {"automation_id": "save"}, {}),
            ("invoke", {"automation_id": "save"}, {}),
            ("set_value", {"automation_id": "title"}, {"value": "Final"}),
        )
        for action, locator, extra in cases:
            with self.subTest(action=action):
                backend = FakeBackend()
                driver = uia.WindowsUIADriver(backend)
                snapshot = driver.execute(
                    "snapshot", {"window": {"handle": 101}}, deadline=deadline()
                )
                found = driver.execute(
                    "find",
                    {
                        "snapshot_id": snapshot["snapshot_id"],
                        "revision": snapshot["revision"],
                        "locator": locator,
                    },
                    deadline=deadline(),
                )
                result = driver.execute(
                    action,
                    {"target": found["target"], "locator": locator, **extra},
                    deadline=deadline(),
                )
                self.assertTrue(result["ok"])
                self.assertEqual(result["action"], action)
                self.assertEqual(backend.capture_count, 2)
                action_calls = [call for call in backend.calls if call[0] == action]
                self.assertEqual(len(action_calls), 1)
                if action == "set_value":
                    self.assertEqual(action_calls[0], ("set_value", "title", "Final"))

    def test_stale_snapshot_and_replaced_target_are_rejected_without_write(self) -> None:
        first = default_tree()
        replacement = default_tree()
        replacement[1] = node(
            "replacement",
            0,
            "button",
            "Save",
            actions=("focus", "invoke"),
            automation_id="save",
        )
        backend = FakeBackend([first, replacement])
        driver = uia.WindowsUIADriver(backend)
        snapshot = driver.execute("snapshot", {"window": {"handle": 101}}, deadline=deadline())
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"automation_id": "save"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(uia.DriverError) as stale:
            driver.execute(
                "invoke",
                {"target": found["target"], "locator": {"automation_id": "save"}},
                deadline=deadline(),
            )
        self.assertEqual(stale.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertFalse(any(call[0] == "invoke" for call in backend.calls))

        backend = FakeBackend()
        driver = uia.WindowsUIADriver(backend)
        old = driver.execute("snapshot", {"window": {"handle": 101}}, deadline=deadline())
        driver.execute("snapshot", {"window": {"handle": 101}}, deadline=deadline())
        with self.assertRaises(uia.DriverError) as stale_revision:
            driver.execute(
                "find",
                {
                    "snapshot_id": old["snapshot_id"],
                    "revision": old["revision"],
                    "locator": {"automation_id": "save"},
                },
                deadline=deadline(),
            )
        self.assertEqual(stale_revision.exception.code, "DRIVER.STALE_SNAPSHOT")

    def test_truncated_snapshot_cannot_resolve_or_dispatch(self) -> None:
        backend = FakeBackend()
        original_capture = backend.capture

        def truncated_capture(*args: object, **kwargs: object) -> object:
            result = original_capture(*args, **kwargs)
            result.truncated = True
            return result

        backend.capture = truncated_capture  # type: ignore[method-assign]
        driver = uia.WindowsUIADriver(backend)
        snapshot = driver.execute(
            "snapshot", {"window": {"handle": 101}}, deadline=deadline()
        )
        with self.assertRaises(uia.DriverError) as raised:
            driver.execute(
                "find",
                {
                    "snapshot_id": snapshot["snapshot_id"],
                    "revision": snapshot["revision"],
                    "locator": {"automation_id": "save"},
                },
                deadline=deadline(),
            )
        self.assertEqual(raised.exception.code, "DRIVER.SNAPSHOT_TRUNCATED")
        self.assertFalse(any(call[0] == "invoke" for call in backend.calls))

    def test_write_reuses_snapshot_bounds_and_requires_native_identity(self) -> None:
        backend = FakeBackend()
        driver = uia.WindowsUIADriver(backend)
        snapshot = driver.execute(
            "snapshot",
            {"window": {"handle": 101}, "max_depth": 64, "max_nodes": 2000},
            deadline=deadline(),
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"automation_id": "save"},
            },
            deadline=deadline(),
        )
        driver.execute(
            "invoke",
            {"target": found["target"], "locator": {"automation_id": "save"}},
            deadline=deadline(),
        )
        captures = [call for call in backend.calls if call[0] == "capture"]
        self.assertEqual(captures[-1][2:], (64, 2000))
        self.assertIn(("same_element", "save", "save"), backend.calls)

        replacement = default_tree()
        replacement[1] = node(
            "new-native", 0, "button", "Save", actions=("focus", "invoke"),
            automation_id="save",
        )
        backend = FakeBackend([default_tree(), replacement])
        driver = uia.WindowsUIADriver(backend)
        snapshot = driver.execute(
            "snapshot", {"window": {"handle": 101}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"automation_id": "save"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(uia.DriverError) as raised:
            driver.execute(
                "invoke",
                {"target": found["target"], "locator": {"automation_id": "save"}},
                deadline=deadline(),
            )
        self.assertEqual(raised.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertFalse(any(call[0] == "invoke" for call in backend.calls))

    def test_unsupported_action_native_failure_and_deadline_are_structured(self) -> None:
        snapshot = self.snapshot()
        found = self.find(snapshot, {"automation_id": "save"})
        with self.assertRaises(uia.DriverError) as unsupported:
            self.driver.execute(
                "set_value",
                {
                    "target": found["target"],
                    "locator": {"automation_id": "save"},
                    "value": "x",
                },
                deadline=deadline(),
            )
        self.assertEqual(unsupported.exception.code, "DRIVER.ACTION_UNSUPPORTED")

        backend = FakeBackend()
        backend.fail = "invoke"
        driver = uia.WindowsUIADriver(backend)
        snapshot = driver.execute("snapshot", {"window": {"handle": 101}}, deadline=deadline())
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"automation_id": "save"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(uia.DriverError) as failed:
            driver.execute(
                "invoke",
                {"target": found["target"], "locator": {"automation_id": "save"}},
                deadline=deadline(),
            )
        self.assertEqual(failed.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertEqual(failed.exception.data["effect"], "unknown")
        self.assertFalse(failed.exception.retryable)

        with self.assertRaises(uia.DriverError) as timed_out:
            self.driver.execute("list_windows", {}, deadline=time.monotonic() - 1)
        self.assertEqual(timed_out.exception.code, "DRIVER.TIMEOUT")
        self.assertTrue(timed_out.exception.retryable)


class WindowsUIAProcessTests(unittest.TestCase):
    def make_plugin(self) -> ProcessPlugin:
        plugin = ProcessPlugin(
            [sys.executable, str(DRIVER_PATH)], timeout=3, name="desktop.windows_uia"
        )
        self.addCleanup(plugin.close)
        return plugin

    def test_manifest_is_canonical_windows_only_and_uses_full_action_ids(self) -> None:
        manifest = self.make_plugin().start()
        self.assertEqual(manifest["metadata"]["name"], "desktop.windows_uia")
        self.assertEqual(manifest["runtime"]["platforms"], ["windows"])
        self.assertEqual(manifest["runtime"]["entrypoint"], "./run.cmd")
        self.assertEqual(
            manifest["actions"]["snapshot"]["permissions"],
            ["desktop.observe"],
        )
        self.assertEqual(
            manifest["actions"]["invoke"]["permissions"],
            ["desktop.observe", "desktop.input"],
        )
        self.assertEqual(
            set(manifest["actions"]),
            {"list_windows", "snapshot", "find", "focus", "invoke", "set_value"},
        )
        for name, contract in manifest["actions"].items():
            self.assertEqual(contract["contract_major"], 1, name)
            self.assertIn(f"desktop.windows_uia.{name}@1", uia.ACTION_NAMES)

    @unittest.skipIf(sys.platform == "win32", "non-Windows unavailable smoke")
    def test_non_windows_runtime_is_structured_unavailable(self) -> None:
        plugin = self.make_plugin()
        with self.assertRaises(PluginError) as raised:
            plugin.invoke("desktop.windows_uia.list_windows@1", {})
        self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
        self.assertEqual(raised.exception.details["reason"], "platform")

    def test_expired_deadline_and_unknown_action_are_structured(self) -> None:
        driver = uia.WindowsUIADriver(FakeBackend())
        with self.assertRaises(uia.DriverError) as expired:
            uia._wire_deadline(int((time.time() - 1) * 1000))
        self.assertEqual(expired.exception.code, "DRIVER.TIMEOUT")

        process = subprocess.run(
            [sys.executable, str(DRIVER_PATH)],
            input=json.dumps({"type": "invoke", "id": "x", "action": "snapshot", "args": {}}) + "\n",
            text=True,
            capture_output=True,
            check=True,
            timeout=5,
        )
        response = json.loads(process.stdout)
        self.assertEqual(response["error"]["code"], "PROTOCOL.ACTION_NOT_FOUND")

    def test_request_reader_rejects_oversize_and_continues(self) -> None:
        oversized = b"{" + b"x" * uia.MAX_REQUEST_BYTES + b"}\n"
        valid = json.dumps({"type": "manifest", "id": "after"}).encode() + b"\n"
        input_stream = io.BytesIO(oversized + valid)
        output = io.BytesIO()
        original_stdout = sys.stdout

        class BinaryStdout:
            buffer = output

        try:
            sys.stdout = BinaryStdout()
            uia.serve(uia.WindowsUIADriver(FakeBackend()), input_stream)
        finally:
            sys.stdout = original_stdout
        messages = [json.loads(line) for line in output.getvalue().decode().splitlines()]
        self.assertEqual(messages[0]["error"]["code"], "PROTOCOL.REQUEST_TOO_LARGE")
        self.assertEqual(messages[1]["id"], "after")
        self.assertEqual(messages[1]["result"]["metadata"]["name"], "desktop.windows_uia")

    @unittest.skipUnless(sys.platform == "win32", "Windows-only dependency smoke")
    def test_windows_backend_dependency_smoke_is_conservative(self) -> None:
        backend = uia.create_default_backend()
        if isinstance(backend, uia.UnavailableBackend):
            with self.assertRaises(uia.DriverError) as raised:
                backend.list_windows(include_invisible=False, deadline=deadline())
            self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
        else:
            windows = backend.list_windows(include_invisible=False, deadline=deadline())
            self.assertIsInstance(windows, list)


if __name__ == "__main__":
    unittest.main()
