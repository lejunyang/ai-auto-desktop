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
    protected: bool = False,
    offscreen: bool = False,
    bounds: dict[str, int] | None = None,
    process_id: int = 7,
) -> object:
    return uia.BackendNode(
        native=key,
        parent_index=parent_index,
        role=role,
        name=name,
        value=value,
        states={
            "enabled": True,
            "offscreen": offscreen,
            "focusable": "focus" in actions,
            "focused": False,
            "read_only": False if "set_value" in actions else None,
        },
        bounds={"x": 10, "y": 20, "width": 120, "height": 30} if bounds is None else bounds,
        actions=actions,
        provenance={
            "automation_id": automation_id,
            "class_name": "FakeControl",
            "framework_id": "Fake",
            "process_id": process_id,
            "runtime_id": [42, key],
            "value_redacted": protected,
        },
    )


def default_tree() -> list[object]:
    return [
        node("root", None, "window", "Editor", actions=("focus", "pointer_click"), automation_id="main"),
        node(
            "save",
            0,
            "button",
            "Save",
            actions=("focus", "invoke", "pointer_click"),
            automation_id="save",
        ),
        node(
            "title",
            0,
            "edit",
            "Title",
            value="Draft",
            actions=("focus", "set_value", "type_text", "pointer_click"),
            automation_id="title",
        ),
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

    def type_text(
        self,
        native: object,
        text: str,
        *,
        window_handle: int,
        deadline: float,
    ) -> object:
        self.calls.append(("type_text", native, text, window_handle))
        if self.fail == "type_text":
            raise RuntimeError("synthetic native failure")
        return {"native_pattern": "type_text"}

    def pointer_click(
        self,
        native: object,
        *,
        target_process_id: int,
        window_handle: int,
        x: int,
        y: int,
        deadline: float,
    ) -> object:
        self.calls.append(
            ("pointer_click", native, target_process_id, window_handle, x, y)
        )
        if self.fail == "pointer_click":
            raise RuntimeError("synthetic native failure")
        return {
            "native_pattern": "SendInput",
            "input_mode": "mouse",
            "submitted": True,
            "events_submitted": 3,
            "point": {"x": x, "y": y},
        }

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
        self.assertEqual(save["actions"], ["focus", "invoke", "pointer_click"])
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
            ("type_text", {"automation_id": "title"}, {"text": "中文 A😀"}),
            (
                "pointer_click",
                {"automation_id": "save", "actions": ["pointer_click"]},
                {"button": "left", "position": "center"},
            ),
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
                elif action == "type_text":
                    self.assertEqual(
                        action_calls[0], ("type_text", "title", "中文 A😀", 101)
                    )
                elif action == "pointer_click":
                    self.assertEqual(
                        action_calls[0], ("pointer_click", "save", 7, 101, 70, 35)
                    )

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

    def test_type_text_rejects_invalid_text_before_capture_or_dispatch(self) -> None:
        snapshot = self.snapshot()
        found = self.find(snapshot, {"automation_id": "title"})
        capture_count = self.backend.capture_count
        cases = (
            "",
            "x" * (uia.MAX_TYPE_TEXT_CHARS + 1),
            "line\nbreak",
            "tab\tcharacter",
            "zero-width-\u200bspace",
            "private-use-\ue000",
            "bad-surrogate-\ud800",
        )
        for text in cases:
            with self.subTest(text=repr(text)):
                with self.assertRaises(uia.DriverError) as raised:
                    self.driver.execute(
                        "type_text",
                        {
                            "target": found["target"],
                            "locator": {"automation_id": "title"},
                            "text": text,
                        },
                        deadline=deadline(),
                    )
                self.assertEqual(raised.exception.code, "DRIVER.INVALID_REQUEST")
                self.assertEqual(self.backend.capture_count, capture_count)
                self.assertFalse(any(call[0] == "type_text" for call in self.backend.calls))

    def test_type_text_rejects_protected_and_non_focusable_targets(self) -> None:
        protected_tree = default_tree()
        protected_tree[2] = node(
            "secret",
            0,
            "edit",
            "Password",
            actions=("focus", "type_text"),
            automation_id="secret",
            protected=True,
        )
        backend = FakeBackend([protected_tree])
        driver = uia.WindowsUIADriver(backend)
        snapshot = driver.execute(
            "snapshot", {"window": {"handle": 101}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"automation_id": "secret"},
            },
            deadline=deadline(),
        )
        self.assertNotIn("pointer_click", found["node"]["actions"])
        with self.assertRaises(uia.DriverError) as protected:
            driver.execute(
                "type_text",
                {
                    "target": found["target"],
                    "locator": {"automation_id": "secret"},
                    "text": "do not send",
                },
                deadline=deadline(),
            )
        self.assertEqual(protected.exception.code, "DRIVER.PROTECTED_ELEMENT")
        self.assertEqual(protected.exception.data["phase"], "before_dispatch")
        self.assertFalse(any(call[0] == "type_text" for call in backend.calls))

        snapshot = driver.execute(
            "snapshot", {"window": {"handle": 101}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"automation_id": "secret"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(uia.DriverError) as protected_click:
            driver.execute(
                "pointer_click",
                {
                    "target": found["target"],
                    "locator": {"automation_id": "secret"},
                },
                deadline=deadline(),
            )
        self.assertEqual(
            protected_click.exception.code, "DRIVER.PROTECTED_ELEMENT"
        )
        self.assertEqual(
            protected_click.exception.data["effect"], "not_applied"
        )
        self.assertFalse(
            any(call[0] == "pointer_click" for call in backend.calls)
        )

        snapshot = self.snapshot()
        save = self.find(snapshot, {"automation_id": "save"})
        with self.assertRaises(uia.DriverError) as unsupported:
            self.driver.execute(
                "type_text",
                {
                    "target": save["target"],
                    "locator": {"automation_id": "save"},
                    "text": "ordinary",
                },
                deadline=deadline(),
            )
        self.assertEqual(unsupported.exception.code, "DRIVER.ACTION_UNSUPPORTED")
        self.assertFalse(any(call[0] == "type_text" for call in self.backend.calls))

    def test_type_text_requires_fresh_top_level_window_handle(self) -> None:
        backend = FakeBackend()
        original_capture = backend.capture

        def capture_without_handle(*args: object, **kwargs: object) -> object:
            result = original_capture(*args, **kwargs)
            if backend.capture_count >= 2:
                result.window = {"title": "Editor", "process_id": 7}
            return result

        backend.capture = capture_without_handle  # type: ignore[method-assign]
        driver = uia.WindowsUIADriver(backend)
        snapshot = driver.execute(
            "snapshot", {"window": {"handle": 101}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"automation_id": "title"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(uia.DriverError) as raised:
            driver.execute(
                "type_text",
                {
                    "target": found["target"],
                    "locator": {"automation_id": "title"},
                    "text": "ordinary",
                },
                deadline=deadline(),
            )
        self.assertEqual(raised.exception.code, "DRIVER.ACTION_FAILED")
        self.assertEqual(raised.exception.data["phase"], "before_dispatch")
        self.assertEqual(raised.exception.data["events_submitted"], 0)
        self.assertFalse(any(call[0] == "type_text" for call in backend.calls))

    def test_pointer_click_rejects_invalid_shape_and_unadvertised_targets(self) -> None:
        snapshot = self.snapshot()
        save = self.find(snapshot, {"automation_id": "save"})
        with self.assertRaises(uia.DriverError) as bad_button:
            self.driver.execute(
                "pointer_click",
                {
                    "target": save["target"],
                    "locator": {"automation_id": "save"},
                    "button": "right",
                },
                deadline=deadline(),
            )
        self.assertEqual(bad_button.exception.code, "DRIVER.INVALID_REQUEST")

        with self.assertRaises(uia.DriverError) as bad_position:
            self.driver.execute(
                "pointer_click",
                {
                    "target": save["target"],
                    "locator": {"automation_id": "save"},
                    "position": "top_left",
                },
                deadline=deadline(),
            )
        self.assertEqual(bad_position.exception.code, "DRIVER.INVALID_REQUEST")

        offscreen_tree = default_tree()
        offscreen_tree[1] = node(
            "save",
            0,
            "button",
            "Save",
            actions=("focus", "invoke"),
            automation_id="save",
            offscreen=True,
        )
        backend = FakeBackend([offscreen_tree])
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
        self.assertNotIn("pointer_click", found["node"]["actions"])
        with self.assertRaises(uia.DriverError) as unsupported:
            driver.execute(
                "pointer_click",
                {
                    "target": found["target"],
                    "locator": {"automation_id": "save"},
                },
                deadline=deadline(),
            )
        self.assertEqual(unsupported.exception.code, "DRIVER.ACTION_UNSUPPORTED")
        self.assertFalse(any(call[0] == "pointer_click" for call in backend.calls))

    def test_pointer_click_requires_positive_bounds_and_window_process_identity(self) -> None:
        zero_bounds_tree = default_tree()
        zero_bounds_tree[1] = node(
            "save",
            0,
            "button",
            "Save",
            actions=("focus", "invoke"),
            automation_id="save",
            bounds={"x": 10, "y": 20, "width": 0, "height": 30},
        )
        backend = FakeBackend([zero_bounds_tree])
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
        self.assertNotIn("pointer_click", found["node"]["actions"])
        with self.assertRaises(uia.DriverError) as unsupported:
            driver.execute(
                "pointer_click",
                {
                    "target": found["target"],
                    "locator": {"automation_id": "save"},
                },
                deadline=deadline(),
            )
        self.assertEqual(unsupported.exception.code, "DRIVER.ACTION_UNSUPPORTED")

        backend = FakeBackend()
        original_capture = backend.capture

        def capture_mismatched_pid(*args: object, **kwargs: object) -> object:
            result = original_capture(*args, **kwargs)
            if backend.capture_count >= 2:
                result.window = {
                    "handle": 101,
                    "title": "Editor",
                    "process_id": 9,
                }
            return result

        backend.capture = capture_mismatched_pid  # type: ignore[method-assign]
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
        with self.assertRaises(uia.DriverError) as stale:
            driver.execute(
                "pointer_click",
                {
                    "target": found["target"],
                    "locator": {"automation_id": "save"},
                },
                deadline=deadline(),
            )
        self.assertEqual(stale.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertFalse(any(call[0] == "pointer_click" for call in backend.calls))

    def test_type_text_distinguishes_pre_dispatch_and_unknown_effect(self) -> None:
        for failure, expected_code, expected_phase, expected_effect in (
            (
                uia.DriverError(
                    "DRIVER.ACTION_FAILED",
                    "no INPUT submitted",
                    data={
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                        "events_submitted": 0,
                    },
                ),
                "DRIVER.ACTION_FAILED",
                "before_dispatch",
                "not_applied",
            ),
            (
                uia.DriverError(
                    "DRIVER.UNKNOWN_EFFECT",
                    "partial INPUT submitted",
                    data={
                        "phase": "post_dispatch",
                        "effect": "unknown",
                        "events_submitted": 1,
                    },
                ),
                "DRIVER.UNKNOWN_EFFECT",
                "post_dispatch",
                "unknown",
            ),
        ):
            with self.subTest(expected_code=expected_code):
                backend = FakeBackend()

                def fail_type_text(
                    native: object,
                    text: str,
                    *,
                    window_handle: int,
                    deadline: float,
                ) -> object:
                    backend.calls.append(("type_text", native, text, window_handle))
                    raise failure

                backend.type_text = fail_type_text  # type: ignore[method-assign]
                driver = uia.WindowsUIADriver(backend)
                snapshot = driver.execute(
                    "snapshot", {"window": {"handle": 101}}, deadline=deadline()
                )
                found = driver.execute(
                    "find",
                    {
                        "snapshot_id": snapshot["snapshot_id"],
                        "revision": snapshot["revision"],
                        "locator": {"automation_id": "title"},
                    },
                    deadline=deadline(),
                )
                with self.assertRaises(uia.DriverError) as raised:
                    driver.execute(
                        "type_text",
                        {
                            "target": found["target"],
                            "locator": {"automation_id": "title"},
                            "text": "ordinary",
                        },
                        deadline=deadline(),
                    )
                self.assertEqual(raised.exception.code, expected_code)
                self.assertEqual(raised.exception.data["phase"], expected_phase)
                self.assertEqual(raised.exception.data["effect"], expected_effect)
                self.assertFalse(raised.exception.retryable)
                self.assertIsNone(driver._current)

    def test_unicode_send_input_adapter_encodes_surrogate_pairs_and_effect_boundary(
        self,
    ) -> None:
        pointer_size = uia.ctypes.sizeof(uia.ctypes.c_void_p)
        self.assertIn(pointer_size, {4, 8})
        self.assertEqual(
            uia.ctypes.sizeof(uia._KeyboardInput), 16 if pointer_size == 4 else 24
        )
        self.assertEqual(
            uia.ctypes.sizeof(uia._MouseInput), 24 if pointer_size == 4 else 32
        )
        self.assertEqual(uia._Input.payload.offset, 4 if pointer_size == 4 else 8)
        self.assertEqual(
            uia.ctypes.sizeof(uia._Input), 28 if pointer_size == 4 else 40
        )
        captured: list[tuple[int, list[tuple[int, int]], int]] = []

        def accept_all(count: int, events: object, size: int) -> int:
            captured.append(
                (
                    count,
                    [
                        (events[index].ki.wScan, events[index].ki.dwFlags)
                        for index in range(count)
                    ],
                    size,
                )
            )
            return count

        adapter = uia.UnicodeSendInputAdapter(accept_all, lambda: 0)
        checks: list[int] = []
        result = adapter.send_text(
            "A😀", before_batch=checks.append, deadline=deadline()
        )
        self.assertEqual(result["utf16_units"], 3)
        self.assertEqual(result["unicode_scalars"], 2)
        self.assertEqual(result["events_submitted"], 6)
        self.assertEqual(checks, [0, 2])
        self.assertEqual([batch[0] for batch in captured], [2, 4])
        self.assertEqual(
            captured[0][1] + captured[1][1],
            [
                (0x0041, uia.KEYEVENTF_UNICODE),
                (0x0041, uia.KEYEVENTF_UNICODE | uia.KEYEVENTF_KEYUP),
                (0xD83D, uia.KEYEVENTF_UNICODE),
                (0xD83D, uia.KEYEVENTF_UNICODE | uia.KEYEVENTF_KEYUP),
                (0xDE00, uia.KEYEVENTF_UNICODE),
                (0xDE00, uia.KEYEVENTF_UNICODE | uia.KEYEVENTF_KEYUP),
            ],
        )
        self.assertTrue(
            all(batch[2] == uia.ctypes.sizeof(uia._Input) for batch in captured)
        )

        for sent, expected_code, expected_effect in (
            (0, "DRIVER.ACTION_FAILED", "not_applied"),
            (1, "DRIVER.UNKNOWN_EFFECT", "unknown"),
        ):
            with self.subTest(sent=sent):
                failing = uia.UnicodeSendInputAdapter(
                    lambda count, events, size, sent=sent: sent, lambda: 5
                )
                with self.assertRaises(uia.DriverError) as raised:
                    failing.send_text("A", deadline=deadline())
                self.assertEqual(raised.exception.code, expected_code)
                self.assertEqual(raised.exception.data["effect"], expected_effect)
                self.assertFalse(raised.exception.retryable)

        calls = 0

        def lose_context_after_first_batch(events_submitted: int) -> None:
            if events_submitted:
                raise uia.DriverError(
                    "DRIVER.ACTION_FAILED",
                    "foreground changed",
                    data={
                        "phase": "before_dispatch",
                        "effect": "not_applied",
                    },
                )

        def submit_batch(count: int, events: object, size: int) -> int:
            nonlocal calls
            calls += 1
            return count

        drifting = uia.UnicodeSendInputAdapter(submit_batch, lambda: 0)
        with self.assertRaises(uia.DriverError) as drifted:
            drifting.send_text(
                "AB",
                before_batch=lose_context_after_first_batch,
                deadline=deadline(),
            )
        self.assertEqual(calls, 1)
        self.assertEqual(drifted.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertEqual(drifted.exception.data["events_submitted"], 2)
        self.assertEqual(drifted.exception.data["effect"], "unknown")

    def test_pointer_send_input_adapter_normalizes_virtual_desktop_and_effect_boundary(
        self,
    ) -> None:
        captured: list[tuple[int, list[tuple[int, int, int]], int]] = []

        def accept_all(count: int, events: object, size: int) -> int:
            captured.append(
                (
                    count,
                    [
                        (events[index].mi.dx, events[index].mi.dy, events[index].mi.dwFlags)
                        for index in range(count)
                    ],
                    size,
                )
            )
            return count

        metrics = {
            uia.SM_XVIRTUALSCREEN: -100,
            uia.SM_YVIRTUALSCREEN: -50,
            uia.SM_CXVIRTUALSCREEN: 400,
            uia.SM_CYVIRTUALSCREEN: 300,
        }
        adapter = uia.UnicodeSendInputAdapter(
            accept_all,
            lambda: 0,
            lambda metric: metrics[metric],
        )
        result = adapter.send_pointer_click(-100, 249, deadline=deadline())
        self.assertTrue(result["submitted"])
        self.assertEqual(result["events_submitted"], 3)
        self.assertEqual([batch[0] for batch in captured], [3])
        self.assertEqual(
            captured[0][1],
            [
                (
                    0,
                    65535,
                    uia.MOUSEEVENTF_MOVE
                    | uia.MOUSEEVENTF_ABSOLUTE
                    | uia.MOUSEEVENTF_VIRTUALDESK,
                ),
                (
                    0,
                    65535,
                    uia.MOUSEEVENTF_LEFTDOWN
                    | uia.MOUSEEVENTF_ABSOLUTE
                    | uia.MOUSEEVENTF_VIRTUALDESK,
                ),
                (
                    0,
                    65535,
                    uia.MOUSEEVENTF_LEFTUP
                    | uia.MOUSEEVENTF_ABSOLUTE
                    | uia.MOUSEEVENTF_VIRTUALDESK,
                ),
            ],
        )

        for sent, expected_code, expected_effect in (
            (0, "DRIVER.ACTION_FAILED", "not_applied"),
            (1, "DRIVER.UNKNOWN_EFFECT", "unknown"),
        ):
            with self.subTest(sent=sent):
                failing = uia.UnicodeSendInputAdapter(
                    lambda count, events, size, sent=sent: sent,
                    lambda: 5,
                    lambda metric: metrics[metric],
                )
                with self.assertRaises(uia.DriverError) as raised:
                    failing.send_pointer_click(0, 0, deadline=deadline())
                self.assertEqual(raised.exception.code, expected_code)
                self.assertEqual(raised.exception.data["effect"], expected_effect)
                self.assertFalse(raised.exception.retryable)

    def test_native_pointer_click_requires_expected_foreground_window(self) -> None:
        calls: list[tuple[object, ...]] = []

        class Native:
            CurrentProcessId = 7
            CurrentIsEnabled = True
            CurrentIsOffscreen = False

        class Adapter:
            def foreground_window_identity(self) -> tuple[int, int]:
                calls.append(("foreground_window_identity",))
                return 101, 7

            def send_pointer_click(
                self,
                x: int,
                y: int,
                *,
                before_dispatch: object,
                deadline: float,
            ) -> dict[str, object]:
                before_dispatch(0)
                calls.append(("send_pointer_click", x, y))
                return {"native_pattern": "SendInput", "submitted": True}

        backend = object.__new__(uia.ComtypesUIABackend)
        backend.input_adapter = Adapter()
        backend._point_hits_target = lambda native, x, y, deadline: True  # type: ignore[method-assign]
        result = backend.pointer_click(
            Native(),
            target_process_id=7,
            window_handle=101,
            x=70,
            y=35,
            deadline=deadline(),
        )
        self.assertEqual(
            calls,
            [
                ("foreground_window_identity",),
                ("send_pointer_click", 70, 35),
            ],
        )
        self.assertEqual(result["native_pattern"], "SendInput")

        class MismatchAdapter(Adapter):
            def foreground_window_identity(self) -> tuple[int, int]:
                calls.append(("foreground_window_identity",))
                return 102, 7

        calls.clear()
        backend.input_adapter = MismatchAdapter()
        backend._point_hits_target = lambda native, x, y, deadline: True  # type: ignore[method-assign]
        with self.assertRaises(uia.DriverError) as raised:
            backend.pointer_click(
                Native(),
                target_process_id=7,
                window_handle=101,
                x=70,
                y=35,
                deadline=deadline(),
            )
        self.assertEqual(raised.exception.code, "DRIVER.ACTION_FAILED")
        self.assertEqual(raised.exception.data["phase"], "before_dispatch")
        self.assertFalse(any(call[0] == "send_pointer_click" for call in calls))

    def test_native_pointer_click_requires_hit_test_match_before_sendinput(self) -> None:
        calls: list[tuple[object, ...]] = []

        class Native:
            CurrentProcessId = 7
            CurrentIsEnabled = True
            CurrentIsOffscreen = False

        class Adapter:
            def foreground_window_identity(self) -> tuple[int, int]:
                calls.append(("foreground_window_identity",))
                return 101, 7

            def send_pointer_click(
                self,
                x: int,
                y: int,
                *,
                before_dispatch: object,
                deadline: float,
            ) -> dict[str, object]:
                before_dispatch(0)
                calls.append(("send_pointer_click", x, y))
                return {"native_pattern": "SendInput", "submitted": True}

        backend = object.__new__(uia.ComtypesUIABackend)
        backend.input_adapter = Adapter()

        calls.clear()
        backend._point_hits_target = lambda native, x, y, deadline: False  # type: ignore[method-assign]
        with self.assertRaises(uia.DriverError) as stale:
            backend.pointer_click(
                Native(),
                target_process_id=7,
                window_handle=101,
                x=70,
                y=35,
                deadline=deadline(),
            )
        self.assertEqual(stale.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertEqual(stale.exception.data["phase"], "before_dispatch")
        self.assertFalse(any(call[0] == "send_pointer_click" for call in calls))

        def fail_hit_test(native: object, x: int, y: int, *, deadline: float) -> bool:
            raise uia.DriverError(
                "DRIVER.ACTION_FAILED",
                "ElementFromPoint failed",
                data={"operation": "ElementFromPoint"},
            )

        calls.clear()
        backend._point_hits_target = fail_hit_test  # type: ignore[method-assign]
        with self.assertRaises(uia.DriverError) as failed:
            backend.pointer_click(
                Native(),
                target_process_id=7,
                window_handle=101,
                x=70,
                y=35,
                deadline=deadline(),
            )
        self.assertEqual(failed.exception.code, "DRIVER.ACTION_FAILED")
        self.assertEqual(failed.exception.data["phase"], "before_dispatch")
        self.assertFalse(any(call[0] == "send_pointer_click" for call in calls))

    def test_element_from_point_uses_comtypes_generated_point_type(self) -> None:
        observed: list[object] = []

        class Automation:
            def ElementFromPoint(self, point: object) -> str:
                observed.append(point)
                if type(point) is not uia.wintypes.POINT:
                    raise TypeError("incompatible POINT type")
                return "hit"

        backend = object.__new__(uia.ComtypesUIABackend)
        backend.automation = Automation()

        self.assertEqual(backend._element_from_point(-25, 40), "hit")
        self.assertEqual(len(observed), 1)
        self.assertIs(type(observed[0]), uia.wintypes.POINT)
        self.assertEqual((observed[0].x, observed[0].y), (-25, 40))

    def test_native_pointer_click_runs_hit_test_immediately_before_dispatch(self) -> None:
        calls: list[tuple[object, ...]] = []

        class Native:
            CurrentProcessId = 7
            CurrentIsEnabled = True
            CurrentIsOffscreen = False

        class Adapter:
            def foreground_window_identity(self) -> tuple[int, int]:
                calls.append(("foreground_window_identity",))
                return 101, 7

            def send_pointer_click(
                self,
                x: int,
                y: int,
                *,
                before_dispatch: object,
                deadline: float,
            ) -> dict[str, object]:
                calls.append(("send_pointer_click_enter", x, y))
                before_dispatch(0)
                calls.append(("send_pointer_click_after_check", x, y))
                return {"native_pattern": "SendInput", "submitted": True}

        backend = object.__new__(uia.ComtypesUIABackend)
        backend.input_adapter = Adapter()

        def hit_test(native: object, x: int, y: int, *, deadline: float) -> bool:
            calls.append(("point_hits_target", x, y))
            return True

        backend._point_hits_target = hit_test  # type: ignore[method-assign]
        result = backend.pointer_click(
            Native(),
            target_process_id=7,
            window_handle=101,
            x=70,
            y=35,
            deadline=deadline(),
        )
        self.assertEqual(
            calls,
            [
                ("send_pointer_click_enter", 70, 35),
                ("point_hits_target", 70, 35),
                ("foreground_window_identity",),
                ("send_pointer_click_after_check", 70, 35),
            ],
        )
        self.assertEqual(result["native_pattern"], "SendInput")

    def test_native_type_text_focuses_before_sendinput_and_focus_failure_is_safe(
        self,
    ) -> None:
        calls: list[tuple[object, ...]] = []

        class Native:
            CurrentHasKeyboardFocus = True
            CurrentProcessId = 7

            def SetFocus(self) -> None:
                calls.append(("focus",))

        class Adapter:
            def foreground_window_identity(self) -> tuple[int, int]:
                calls.append(("foreground_window_identity",))
                return 101, 7

            def send_text(
                self,
                text: str,
                *,
                before_batch: object,
                deadline: float,
            ) -> dict[str, object]:
                before_batch(0)
                calls.append(("send_text", text))
                return {"native_pattern": "SendInput"}

        backend = object.__new__(uia.ComtypesUIABackend)
        backend.input_adapter = Adapter()
        result = backend.type_text(
            Native(), "ordinary", window_handle=101, deadline=deadline()
        )
        self.assertEqual(
            calls,
            [
                ("focus",),
                ("foreground_window_identity",),
                ("send_text", "ordinary"),
            ],
        )
        self.assertEqual(result["native_pattern"], "SendInput")

        class FailingNative:
            def SetFocus(self) -> None:
                calls.append(("failing_focus",))
                raise RuntimeError("focus denied")

        calls.clear()
        with self.assertRaises(uia.DriverError) as raised:
            backend.type_text(
                FailingNative(), "ordinary", window_handle=101, deadline=deadline()
            )
        self.assertEqual(raised.exception.code, "DRIVER.ACTION_FAILED")
        self.assertEqual(raised.exception.data["phase"], "before_dispatch")
        self.assertEqual(raised.exception.data["effect"], "not_applied")
        self.assertEqual(raised.exception.data["events_submitted"], 0)
        self.assertFalse(raised.exception.retryable)
        self.assertEqual(calls, [("failing_focus",)])

    def test_native_type_text_rejects_focus_or_foreground_mismatch_before_input(
        self,
    ) -> None:
        calls: list[tuple[object, ...]] = []

        class Native:
            CurrentProcessId = 7

            def __init__(self, focused: bool) -> None:
                self.CurrentHasKeyboardFocus = focused

            def SetFocus(self) -> None:
                calls.append(("focus",))

        class Adapter:
            def __init__(self, window_handle: int, process_id: int) -> None:
                self.window_handle = window_handle
                self.process_id = process_id

            def foreground_window_identity(self) -> tuple[int, int]:
                calls.append(("foreground_window_identity",))
                return self.window_handle, self.process_id

            def send_text(
                self,
                text: str,
                *,
                before_batch: object,
                deadline: float,
            ) -> object:
                before_batch(0)
                calls.append(("send_text", text))
                return {}

        backend = object.__new__(uia.ComtypesUIABackend)
        for focused, foreground_hwnd, foreground_pid in (
            (False, 101, 7),
            (True, 102, 7),
            (True, 101, 8),
        ):
            with self.subTest(
                focused=focused,
                foreground_hwnd=foreground_hwnd,
                foreground_pid=foreground_pid,
            ):
                calls.clear()
                backend.input_adapter = Adapter(foreground_hwnd, foreground_pid)
                with self.assertRaises(uia.DriverError) as raised:
                    backend.type_text(
                        Native(focused),
                        "ordinary",
                        window_handle=101,
                        deadline=deadline(),
                    )
                self.assertEqual(raised.exception.code, "DRIVER.ACTION_FAILED")
                self.assertEqual(raised.exception.data["phase"], "before_dispatch")
                self.assertEqual(raised.exception.data["effect"], "not_applied")
                self.assertFalse(raised.exception.retryable)
                self.assertFalse(any(call[0] == "send_text" for call in calls))

    def test_type_text_timeout_is_never_retryable(self) -> None:
        with self.assertRaises(uia.DriverError) as raised:
            self.driver.execute(
                "type_text",
                {
                    "target": {
                        "snapshot_id": "expired:1",
                        "revision": 1,
                        "node_id": "n0",
                    },
                    "locator": {"role": "edit"},
                    "text": "ordinary",
                },
                deadline=time.monotonic() - 1,
            )
        self.assertEqual(raised.exception.code, "DRIVER.TIMEOUT")
        self.assertEqual(raised.exception.data["phase"], "before_dispatch")
        self.assertEqual(raised.exception.data["effect"], "not_applied")
        self.assertFalse(raised.exception.retryable)

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
        self.assertFalse(any(call[0] == "type_text" for call in self.backend.calls))

        fallback_backend = FakeBackend()
        fallback_backend.fail = "set_value"
        fallback_driver = uia.WindowsUIADriver(fallback_backend)
        fallback_snapshot = fallback_driver.execute(
            "snapshot", {"window": {"handle": 101}}, deadline=deadline()
        )
        fallback_target = fallback_driver.execute(
            "find",
            {
                "snapshot_id": fallback_snapshot["snapshot_id"],
                "revision": fallback_snapshot["revision"],
                "locator": {"automation_id": "title"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(uia.DriverError) as no_fallback:
            fallback_driver.execute(
                "set_value",
                {
                    "target": fallback_target["target"],
                    "locator": {"automation_id": "title"},
                    "value": "x",
                },
                deadline=deadline(),
            )
        self.assertEqual(no_fallback.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertFalse(any(call[0] == "type_text" for call in fallback_backend.calls))

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
        type_text = manifest["actions"]["type_text"]
        self.assertEqual(
            type_text["permissions"], ["desktop.observe", "desktop.input"]
        )
        self.assertEqual(type_text["effect"]["default_class"], "contextual")
        self.assertEqual(type_text["risk"], {"category": "input", "level": "high"})
        self.assertEqual(
            type_text["input_schema"]["required"], ["target", "locator", "text"]
        )
        self.assertEqual(
            type_text["input_schema"]["properties"]["text"]["maxLength"],
            uia.MAX_TYPE_TEXT_CHARS,
        )
        self.assertTrue(all(not error["retryable"] for error in type_text["errors"]))
        self.assertEqual(
            set(manifest["actions"]),
            {
                "list_windows",
                "snapshot",
                "find",
                "focus",
                "invoke",
                "set_value",
                "type_text",
                "pointer_click",
            },
        )
        for name, contract in manifest["actions"].items():
            self.assertEqual(contract["contract_major"], 1, name)
            self.assertIn(f"desktop.windows_uia.{name}@1", uia.ACTION_NAMES)

    @unittest.skipIf(sys.platform == "win32", "non-Windows unavailable smoke")
    def test_non_windows_runtime_is_structured_unavailable(self) -> None:
        plugin = self.make_plugin()
        for action_name, args in (
            ("list_windows", {}),
            (
                "type_text",
                {
                    "target": {
                        "snapshot_id": "unavailable:1",
                        "revision": 1,
                        "node_id": "n0",
                    },
                    "locator": {"role": "edit"},
                    "text": "ordinary",
                },
            ),
        ):
            with self.subTest(action=action_name):
                with self.assertRaises(PluginError) as raised:
                    plugin.invoke(f"desktop.windows_uia.{action_name}@1", args)
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

        expired_type_text = io.BytesIO()
        original_stdout = sys.stdout

        class BinaryStdout:
            buffer = expired_type_text

        try:
            sys.stdout = BinaryStdout()
            uia.handle_request(
                {
                    "type": "invoke",
                    "id": "expired-type-text",
                    "action": "desktop.windows_uia.type_text@1",
                    "deadline_ms": int((time.time() - 1) * 1000),
                    "args": {},
                },
                driver,
            )
        finally:
            sys.stdout = original_stdout
        response = json.loads(expired_type_text.getvalue())
        self.assertEqual(response["error"]["code"], "DRIVER.TIMEOUT")
        self.assertFalse(response["error"]["retryable"])
        self.assertEqual(response["error"]["data"]["phase"], "before_dispatch")

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
