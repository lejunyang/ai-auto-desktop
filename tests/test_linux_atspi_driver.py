"""Linux AT-SPI 进程驱动的跨平台契约测试。"""

from __future__ import annotations

import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import sys
import time
import unittest
from unittest import mock

from ai_auto_desktop.plugin import ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
DRIVER_PATH = PROJECT_ROOT / "plugins" / "linux_atspi" / "linux_atspi_driver.py"

SPEC = importlib.util.spec_from_file_location("testable_linux_atspi_driver", DRIVER_PATH)
assert SPEC is not None and SPEC.loader is not None
atspi = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = atspi
SPEC.loader.exec_module(atspi)


def deadline() -> float:
    return time.monotonic() + 5.0


def node(
    key: str,
    parent_index: int | None,
    role: str,
    name: str | None,
    *,
    description: str | None = None,
    value: str | None = None,
    actions: tuple[str, ...] = (),
    protected: bool = False,
    checked: bool | None = None,
    expandable: bool | None = None,
    expanded: bool | None = None,
    selectable: bool | None = None,
    selected: bool | None = None,
    attributes: dict[str, str] | None = None,
) -> object:
    return atspi.BackendNode(
        native=key,
        parent_index=parent_index,
        role=role,
        name=name,
        description=description,
        value=value,
        attributes=attributes or {"class": "FakeControl"},
        states={
            "enabled": True,
            "visible": True,
            "showing": True,
            "focusable": "focus" in actions,
            "focused": False,
            "editable": bool({"set_text", "type_text"}.intersection(actions)),
            "sensitive": True,
            "protected": protected,
            "checked": checked,
            "expandable": expandable,
            "expanded": expanded,
            "selectable": selectable,
            "selected": selected,
        },
        bounds={"x": 10, "y": 20, "width": 120, "height": 30},
        actions=actions,
        provenance={
            "bus_name": ":1.42",
            "object_path": f"/org/a11y/atspi/accessible/{key}",
            "application_name": "Editor",
            "toolkit_name": "Qt",
            "process_id": 7,
            "value_redacted": protected,
            "coordinate_space": "screen",
        },
    )


def default_tree() -> list[object]:
    return [
        node("root", None, "application", "Editor"),
        node("frame", 0, "frame", "Editor", actions=("focus",)),
        node(
            "save",
            1,
            "push_button",
            "Save",
            description="Save document",
            actions=("focus", "invoke", "pointer_click"),
            attributes={"class": "QPushButton", "id": "save"},
        ),
        node(
            "title",
            1,
            "text",
            "Title",
            value="Draft",
            actions=("focus", "set_text", "type_text"),
            attributes={"class": "QLineEdit", "id": "title"},
        ),
        node(
            "autosave",
            1,
            "check_box",
            "Autosave",
            actions=("toggle",),
            checked=False,
            attributes={"class": "GtkCheckButton", "id": "autosave"},
        ),
        node(
            "details",
            1,
            "toggle_button",
            "Details",
            actions=("expand", "collapse"),
            expandable=True,
            expanded=False,
            attributes={"class": "GtkExpander", "id": "details"},
        ),
    ]


class FakeBackend:
    name = "fake_linux_atspi"

    def __init__(self, snapshots: list[list[object]] | None = None) -> None:
        self.snapshots = snapshots or [default_tree()]
        self.capture_count = 0
        self.calls: list[tuple[object, ...]] = []
        self.fail: str | None = None
        self.truncated = False
        self.point_target: object | None = None

    def session_info(self) -> dict[str, object]:
        return {
            "session_type": "x11",
            "desktop": "KDE",
            "display": ":0",
            "session_bus": True,
        }

    def list_applications(self, *, deadline: float) -> list[dict[str, object]]:
        self.calls.append(("list_applications",))
        return [
            {
                "name": "Editor",
                "bus_name": ":1.42",
                "process_id": 7,
                "toolkit_name": "Qt",
            }
        ]

    def capture(
        self, application: object, *, max_depth: int, max_nodes: int, deadline: float
    ) -> object:
        self.calls.append(
            ("capture", copy.deepcopy(application), max_depth, max_nodes)
        )
        index = min(self.capture_count, len(self.snapshots) - 1)
        self.capture_count += 1
        return atspi.BackendSnapshot(
            application={
                "name": "Editor",
                "bus_name": ":1.42",
                "process_id": 7,
                "toolkit_name": "Qt",
            },
            nodes=copy.deepcopy(self.snapshots[index]),
            truncated=self.truncated,
        )

    def _action(
        self, action: str, native: object, text: str | None = None
    ) -> dict[str, object]:
        self.calls.append((action, native, text))
        if self.fail == action:
            raise RuntimeError("synthetic native failure")
        return {"native_interface": action}

    def focus(self, native: object, *, deadline: float) -> object:
        return self._action("focus", native)

    def invoke(self, native: object, *, deadline: float) -> object:
        return self._action("invoke", native)

    def set_text(self, native: object, text: str, *, deadline: float) -> object:
        return self._action("set_text", native, text)

    def toggle(self, native: object, *, deadline: float) -> object:
        return self._action("toggle", native)

    def expand(self, native: object, *, deadline: float) -> object:
        return self._action("expand", native)

    def collapse(self, native: object, *, deadline: float) -> object:
        return self._action("collapse", native)

    def same_element(
        self, previous: object, current: object, *, deadline: float
    ) -> bool:
        self.calls.append(("same_element", previous, current))
        return previous == current

    def accessible_at_point(
        self, root: object, x: int, y: int, *, deadline: float
    ) -> object | None:
        self.calls.append(("accessible_at_point", root, x, y))
        if self.point_target is not None:
            return self.point_target
        return "save"


class FakeXTestHelper:
    def __init__(self) -> None:
        self.calls: list[tuple[object, ...]] = []
        self.failure: atspi.DriverError | None = None

    def _qualified_environment(self) -> dict[str, str]:
        self.calls.append(("qualified_environment",))
        return {"DISPLAY": ":0"}

    def _validated_path(self) -> str:
        self.calls.append(("validated_path",))
        return "/fixed/x11_xtest_helper"

    def preflight(self) -> None:
        self._qualified_environment()
        self._validated_path()

    def type_text(
        self, text: str, *, expected_process_id: int, deadline: float
    ) -> dict[str, object]:
        self.calls.append(("type_text", text, expected_process_id))
        if self.failure is not None:
            raise self.failure
        return {
            "native_interface": "XTEST",
            "synthetic_input": True,
            "submitted": True,
            "events": max(2, len(text) * 2),
            "codepoints": len(text),
        }

    def pointer_click(
        self, *, expected_process_id: int, x: int, y: int, deadline: float
    ) -> dict[str, object]:
        self.calls.append(("pointer_click", expected_process_id, x, y))
        if self.failure is not None:
            raise self.failure
        return {
            "native_interface": "XTEST",
            "synthetic_input": True,
            "submitted": True,
            "events": 3,
        }


class IdentityFailureBackend(FakeBackend):
    def same_element(
        self, previous: object, current: object, *, deadline: float
    ) -> bool:
        raise atspi.DriverError(
            "DRIVER.ACTION_FAILED",
            "synthetic identity check failure",
            data={"operation": "same_element"},
        )


class LinuxAtspiDriverCoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.backend = FakeBackend()
        self.driver = atspi.LinuxAtspiDriver(self.backend)

    def snapshot(
        self, *, max_depth: int = 32, max_nodes: int = 1000
    ) -> dict[str, object]:
        return self.driver.execute(
            "snapshot",
            {
                "application": {"bus_name": ":1.42"},
                "max_depth": max_depth,
                "max_nodes": max_nodes,
            },
            deadline=deadline(),
        )

    def find(
        self, snapshot: dict[str, object], locator: dict[str, object]
    ) -> dict[str, object]:
        return self.driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": locator,
            },
            deadline=deadline(),
        )

    def test_list_applications_and_snapshot_report_session_and_backend(self) -> None:
        session = self.driver.execute("inspect_session", {}, deadline=deadline())
        self.assertEqual(session, {
            "backend": "fake_linux_atspi",
            "session_type": "x11",
            "desktop": "KDE",
        })
        result = self.driver.execute("list_applications", {}, deadline=deadline())
        self.assertEqual(result["backend"], "fake_linux_atspi")
        self.assertEqual(result["session"]["session_type"], "x11")
        self.assertEqual(result["session"]["desktop"], "KDE")
        self.assertEqual(result["applications"][0]["bus_name"], ":1.42")

        snapshot = self.snapshot()
        self.assertEqual(snapshot["revision"], 1)
        self.assertTrue(str(snapshot["snapshot_id"]).endswith(":1"))
        self.assertEqual(snapshot["backend"], "fake_linux_atspi")
        self.assertEqual(snapshot["session"]["display"], ":0")
        self.assertEqual(snapshot["application"]["name"], "Editor")
        self.assertFalse(snapshot["truncated"])
        self.assertEqual(
            [item["node_id"] for item in snapshot["nodes"]],
            ["n0", "n1", "n2", "n3", "n4", "n5"],
        )
        save = snapshot["nodes"][2]
        self.assertEqual(save["parent_id"], "n1")
        self.assertEqual(save["description"], "Save document")
        self.assertEqual(save["actions"], ["focus", "invoke", "pointer_click"])
        self.assertEqual(save["attributes"]["class"], "QPushButton")
        self.assertEqual(save["provenance"]["backend"], "fake_linux_atspi")
        self.assertFalse(snapshot["nodes"][4]["states"]["checked"])
        self.assertEqual(snapshot["nodes"][4]["actions"], ["toggle"])
        self.assertTrue(snapshot["nodes"][5]["states"]["expandable"])
        self.assertFalse(snapshot["nodes"][5]["states"]["expanded"])
        self.assertEqual(snapshot["nodes"][5]["actions"], ["collapse", "expand"])

    def test_locator_is_exact_supports_atspi_fields_and_rejects_ambiguity(self) -> None:
        duplicate = default_tree()
        duplicate.append(
            node(
                "save2",
                1,
                "push_button",
                "Save",
                actions=("invoke",),
                attributes={"class": "QPushButton", "id": "save2"},
            )
        )
        self.driver = atspi.LinuxAtspiDriver(FakeBackend([duplicate]))
        snapshot = self.snapshot()

        with self.assertRaises(atspi.DriverError) as missing:
            self.find(snapshot, {"name": "save"})
        self.assertEqual(missing.exception.code, "DRIVER.NOT_FOUND")

        with self.assertRaises(atspi.DriverError) as ambiguous:
            self.find(snapshot, {"role": "push_button", "name": "Save"})
        self.assertEqual(ambiguous.exception.code, "DRIVER.AMBIGUOUS")
        self.assertEqual(ambiguous.exception.data["candidate_count"], 2)

        found = self.find(snapshot, {"attributes": {"id": "save2"}})
        self.assertEqual(found["target"]["node_id"], "n6")
        found = self.find(snapshot, {"description": "Save document"})
        self.assertEqual(found["target"]["node_id"], "n2")
        found = self.find(
            snapshot,
            {"object_path": "/org/a11y/atspi/accessible/save"},
        )
        self.assertEqual(found["target"]["node_id"], "n2")
        found = self.find(
            snapshot,
            {"states": {"checked": False}, "actions": ["toggle"]},
        )
        self.assertEqual(found["target"]["node_id"], "n4")
        found = self.find(
            snapshot,
            {"states": {"expandable": True, "expanded": False}},
        )
        self.assertEqual(found["target"]["node_id"], "n5")

    def test_invalid_locator_shapes_fail_closed(self) -> None:
        snapshot = self.snapshot()
        invalid = (
            {},
            {"match": "exact"},
            {"match": "contains", "name": "Save"},
            {"attributes": {}},
            {"states": {}},
            {"actions": []},
            {"attributes": {"id": 7}},
            {"states": {"imaginary": True}},
            {"actions": ["click"]},
        )
        for locator in invalid:
            with self.subTest(locator=locator), self.assertRaises(
                atspi.DriverError
            ) as raised:
                self.find(snapshot, locator)
            self.assertEqual(raised.exception.code, "DRIVER.INVALID_REQUEST")

    def test_each_write_action_re_resolves_before_semantic_dispatch(self) -> None:
        cases = (
            ("focus", {"object_path": "/org/a11y/atspi/accessible/save"}, {}),
            ("invoke", {"attributes": {"id": "save"}}, {}),
            (
                "pointer_click",
                {"attributes": {"id": "save"}},
                {"button": "left", "position": "center"},
            ),
            ("set_text", {"attributes": {"id": "title"}}, {"text": "Final"}),
            (
                "type_text",
                {"attributes": {"id": "title"}},
                {"text": "UTF-8: 你好"},
            ),
            ("toggle", {"attributes": {"id": "autosave"}}, {}),
            ("expand", {"attributes": {"id": "details"}}, {}),
            (
                "collapse",
                {"attributes": {"id": "details"}},
                {},
            ),
        )
        for action, locator, extra in cases:
            with self.subTest(action=action):
                tree = default_tree()
                if action == "collapse":
                    tree[5] = node(
                        "details",
                        1,
                        "toggle_button",
                        "Details",
                        actions=("expand", "collapse"),
                        expandable=True,
                        expanded=True,
                        attributes={"class": "GtkExpander", "id": "details"},
                    )
                backend = FakeBackend([tree])
                helper = FakeXTestHelper()
                driver = atspi.LinuxAtspiDriver(backend, xtest_helper=helper)
                snapshot = driver.execute(
                    "snapshot",
                    {"application": {"bus_name": ":1.42"}},
                    deadline=deadline(),
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
                self.assertEqual(
                    len(action_calls),
                    0 if action in {"type_text", "pointer_click"} else 1,
                )
                if action == "set_text":
                    self.assertEqual(action_calls[0], ("set_text", "title", "Final"))
                if action == "type_text":
                    self.assertEqual(action_calls, [])
                    self.assertEqual(
                        [call for call in backend.calls if call[0] == "focus"],
                        [("focus", "title", None)],
                    )
                    self.assertEqual(
                        helper.calls[-1], ("type_text", "UTF-8: 你好", 7)
                    )
                    self.assertTrue(result["backend_result"]["synthetic_input"])
                if action == "pointer_click":
                    self.assertEqual(action_calls, [])
                    self.assertEqual(
                        [call for call in backend.calls if call[0] == "focus"],
                        [("focus", "save", None)],
                    )
                    self.assertEqual(
                        helper.calls[-1], ("pointer_click", 7, 70, 35)
                    )
                    self.assertEqual(result["backend_result"]["button"], "left")
                    self.assertEqual(result["backend_result"]["position"], "center")
                    self.assertEqual(
                        result["backend_result"]["click_point"], {"x": 70, "y": 35}
                    )
                    self.assertTrue(result["backend_result"]["synthetic_input"])

    def test_type_text_preflights_and_never_implicitly_falls_back(self) -> None:
        helper = FakeXTestHelper()
        driver = atspi.LinuxAtspiDriver(self.backend, xtest_helper=helper)
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "title"}},
            },
            deadline=deadline(),
        )
        invalid_texts = ("", "tab\tforbidden", "nul\0forbidden", "x" * 1025)
        for text in invalid_texts:
            current = driver.execute(
                "snapshot",
                {"application": {"name": "Editor"}},
                deadline=deadline(),
            )
            current_target = driver.execute(
                "find",
                {
                    "snapshot_id": current["snapshot_id"],
                    "revision": current["revision"],
                    "locator": {"attributes": {"id": "title"}},
                },
                deadline=deadline(),
            )["target"]
            with self.subTest(text_length=len(text)), self.assertRaises(
                atspi.DriverError
            ) as raised:
                driver.execute(
                    "type_text",
                    {
                        "target": current_target,
                        "locator": {"attributes": {"id": "title"}},
                        "text": text,
                    },
                    deadline=deadline(),
                )
            self.assertEqual(raised.exception.code, "DRIVER.INVALID_REQUEST")
        self.assertFalse(any(call[0] in {"focus", "set_text"} for call in self.backend.calls))
        self.assertFalse(any(call[0] == "type_text" for call in helper.calls))

        fresh = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        button = driver.execute(
            "find",
            {
                "snapshot_id": fresh["snapshot_id"],
                "revision": fresh["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as unsupported:
            driver.execute(
                "type_text",
                {
                    "target": button["target"],
                    "locator": {"attributes": {"id": "save"}},
                    "text": "never typed",
                },
                deadline=deadline(),
            )
        self.assertEqual(unsupported.exception.code, "DRIVER.ACTION_UNSUPPORTED")
        self.assertFalse(any(call[0] == "set_text" for call in self.backend.calls))

    def test_type_text_after_focus_failure_is_unknown_and_never_retried(self) -> None:
        helper = FakeXTestHelper()
        helper.failure = atspi.DriverError(
            "DRIVER.TIMEOUT",
            "synthetic helper timeout",
            retryable=True,
            data={"phase": "before_input_dispatch", "dispatch_started": False},
        )
        driver = atspi.LinuxAtspiDriver(FakeBackend(), xtest_helper=helper)
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "title"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as raised:
            driver.execute(
                "type_text",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "title"}},
                    "text": "one attempt",
                },
                deadline=deadline(),
            )
        self.assertEqual(raised.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertFalse(raised.exception.retryable)
        self.assertEqual(raised.exception.data["effect"], "unknown")
        self.assertEqual(
            len([call for call in helper.calls if call[0] == "type_text"]), 1
        )

    def test_pointer_click_validates_contract_and_distinguishes_pre_dispatch_failure(self) -> None:
        helper = FakeXTestHelper()
        driver = atspi.LinuxAtspiDriver(FakeBackend(), xtest_helper=helper)
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        for extra in (
            {"button": "right"},
            {"position": "top_left"},
            {"x": 12, "y": 34},
        ):
            current_snapshot = driver.execute(
                "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
            )
            current_found = driver.execute(
                "find",
                {
                    "snapshot_id": current_snapshot["snapshot_id"],
                    "revision": current_snapshot["revision"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
            with self.subTest(extra=extra), self.assertRaises(atspi.DriverError) as invalid:
                driver.execute(
                    "pointer_click",
                    {
                        "target": current_found["target"],
                        "locator": {"attributes": {"id": "save"}},
                        **extra,
                    },
                    deadline=deadline(),
                )
            self.assertEqual(invalid.exception.code, "DRIVER.INVALID_REQUEST")

        helper.failure = atspi.DriverError(
            "DRIVER.TIMEOUT",
            "synthetic pointer helper timeout",
            retryable=True,
            data={"phase": "before_pointer_dispatch", "dispatch_started": False},
        )
        failure_snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        failure_found = driver.execute(
            "find",
            {
                "snapshot_id": failure_snapshot["snapshot_id"],
                "revision": failure_snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as raised:
            driver.execute(
                "pointer_click",
                {
                    "target": failure_found["target"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(raised.exception.code, "DRIVER.TIMEOUT")
        self.assertFalse(raised.exception.retryable)
        self.assertEqual(raised.exception.data["effect"], "contextual")
        self.assertTrue(raised.exception.data["focus_changed"])
        self.assertFalse(raised.exception.data["dispatch_started"])
        self.assertEqual(
            len([call for call in helper.calls if call[0] == "pointer_click"]), 1
        )

    def test_pointer_click_stale_and_bounds_fail_closed_before_helper(self) -> None:
        replacement = default_tree()
        replacement[2] = node(
            "replacement",
            1,
            "push_button",
            "Save",
            actions=("focus", "invoke", "pointer_click"),
            attributes={"class": "QPushButton", "id": "save"},
        )
        helper = FakeXTestHelper()
        backend = FakeBackend([default_tree(), replacement])
        driver = atspi.LinuxAtspiDriver(backend, xtest_helper=helper)
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as stale:
            driver.execute(
                "pointer_click",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(stale.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertFalse(any(call[0] == "pointer_click" for call in helper.calls))

        missing_bounds = default_tree()
        missing_bounds[2] = atspi.BackendNode(
            native="save",
            parent_index=1,
            role="push_button",
            name="Save",
            description="Save document",
            attributes={"class": "QPushButton", "id": "save"},
            states={
                "enabled": True,
                "visible": True,
                "showing": True,
                "focusable": True,
                "focused": False,
                "editable": False,
                "sensitive": True,
                "protected": False,
                "checked": None,
                "expandable": None,
                "expanded": None,
                "selectable": None,
                "selected": None,
            },
            bounds=None,
            actions=("focus", "invoke", "pointer_click"),
            provenance={
                "bus_name": ":1.42",
                "object_path": "/org/a11y/atspi/accessible/save",
                "application_name": "Editor",
                "toolkit_name": "Qt",
                "process_id": 7,
                "value_redacted": False,
                "coordinate_space": "screen",
            },
        )
        missing_backend = FakeBackend([missing_bounds])
        missing_helper = FakeXTestHelper()
        missing_driver = atspi.LinuxAtspiDriver(
            missing_backend, xtest_helper=missing_helper
        )
        missing_snapshot = missing_driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        missing_found = missing_driver.execute(
            "find",
            {
                "snapshot_id": missing_snapshot["snapshot_id"],
                "revision": missing_snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as unsupported:
            missing_driver.execute(
                "pointer_click",
                {
                    "target": missing_found["target"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(unsupported.exception.code, "DRIVER.ACTION_UNSUPPORTED")
        self.assertFalse(any(call[0] == "pointer_click" for call in missing_helper.calls))

    def test_pointer_click_is_not_advertised_for_protected_nodes(self) -> None:
        protected_tree = default_tree()
        protected_tree[2] = atspi.BackendNode(
            native="save",
            parent_index=1,
            role="push_button",
            name="Save",
            description="Save document",
            attributes={"class": "QPushButton", "id": "save"},
            states={
                "enabled": True,
                "visible": True,
                "showing": True,
                "focusable": True,
                "focused": False,
                "editable": False,
                "sensitive": True,
                "protected": True,
                "checked": None,
                "expandable": None,
                "expanded": None,
                "selectable": None,
                "selected": None,
            },
            bounds={"x": 10, "y": 20, "width": 120, "height": 30},
            actions=("focus", "invoke"),
            provenance={
                "bus_name": ":1.42",
                "object_path": "/org/a11y/atspi/accessible/save",
                "application_name": "Editor",
                "toolkit_name": "Qt",
                "process_id": 7,
                "value_redacted": False,
                "coordinate_space": "screen",
            },
        )
        driver = atspi.LinuxAtspiDriver(FakeBackend([protected_tree]))
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        self.assertNotIn("pointer_click", found["node"]["actions"])

    def test_pointer_click_fails_closed_when_atspi_point_hits_same_process_sibling(self) -> None:
        overlay_tree = default_tree()
        overlay_tree.append(
            atspi.BackendNode(
                native="overlay",
                parent_index=1,
                role="panel",
                name="Overlay",
                description="Sibling overlay",
                attributes={"class": "QWidget", "id": "overlay"},
                states={
                    "enabled": True,
                    "visible": True,
                    "showing": True,
                    "focusable": False,
                    "focused": False,
                    "editable": False,
                    "sensitive": True,
                    "protected": False,
                    "checked": None,
                    "expandable": None,
                    "expanded": None,
                    "selectable": None,
                    "selected": None,
                },
                bounds={"x": 10, "y": 20, "width": 120, "height": 30},
                actions=(),
                provenance={
                    "bus_name": ":1.42",
                    "object_path": "/org/a11y/atspi/accessible/overlay",
                    "application_name": "Editor",
                    "toolkit_name": "Qt",
                    "process_id": 7,
                    "value_redacted": False,
                    "coordinate_space": "screen",
                },
            )
        )
        backend = FakeBackend([overlay_tree])
        backend.point_target = "overlay"
        helper = FakeXTestHelper()
        driver = atspi.LinuxAtspiDriver(backend, xtest_helper=helper)
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as blocked:
            driver.execute(
                "pointer_click",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(blocked.exception.code, "DRIVER.ACTION_UNSUPPORTED")
        self.assertEqual(blocked.exception.data["effect"], "not_applied")
        self.assertFalse(any(call[0] == "pointer_click" for call in helper.calls))

    def test_pointer_click_allows_atspi_descendant_hit_within_target_subtree(self) -> None:
        subtree = default_tree()
        subtree.append(
            atspi.BackendNode(
                native="save-icon",
                parent_index=2,
                role="icon",
                name="Save icon",
                description="Icon child",
                attributes={"class": "QIcon", "id": "save-icon"},
                states={
                    "enabled": True,
                    "visible": True,
                    "showing": True,
                    "focusable": False,
                    "focused": False,
                    "editable": False,
                    "sensitive": True,
                    "protected": False,
                    "checked": None,
                    "expandable": None,
                    "expanded": None,
                    "selectable": None,
                    "selected": None,
                },
                bounds={"x": 30, "y": 25, "width": 20, "height": 20},
                actions=(),
                provenance={
                    "bus_name": ":1.42",
                    "object_path": "/org/a11y/atspi/accessible/save-icon",
                    "application_name": "Editor",
                    "toolkit_name": "Qt",
                    "process_id": 7,
                    "value_redacted": False,
                    "coordinate_space": "screen",
                },
            )
        )
        backend = FakeBackend([subtree])
        backend.point_target = "save-icon"
        helper = FakeXTestHelper()
        driver = atspi.LinuxAtspiDriver(backend, xtest_helper=helper)
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        result = driver.execute(
            "pointer_click",
            {
                "target": found["target"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        self.assertTrue(result["ok"])
        self.assertEqual(helper.calls[-1], ("pointer_click", 7, 70, 35))

    def test_type_text_stale_or_protected_target_never_reaches_helper(self) -> None:
        replacement = default_tree()
        replacement[3] = node(
            "replacement",
            1,
            "text",
            "Title",
            value="Draft",
            actions=("focus", "set_text", "type_text"),
            attributes={"class": "QLineEdit", "id": "title"},
        )
        helper = FakeXTestHelper()
        backend = FakeBackend([default_tree(), replacement])
        driver = atspi.LinuxAtspiDriver(backend, xtest_helper=helper)
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "title"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as stale:
            driver.execute(
                "type_text",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "title"}},
                    "text": "never",
                },
                deadline=deadline(),
            )
        self.assertEqual(stale.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertFalse(any(call[0] == "focus" for call in backend.calls))
        self.assertFalse(any(call[0] == "type_text" for call in helper.calls))

        protected_tree = default_tree()
        protected_tree[3] = node(
            "password",
            1,
            "password_text",
            "Password",
            actions=("focus", "type_text"),
            protected=True,
            attributes={"id": "password"},
        )
        protected_backend = FakeBackend([protected_tree])
        protected_helper = FakeXTestHelper()
        protected_driver = atspi.LinuxAtspiDriver(
            protected_backend, xtest_helper=protected_helper
        )
        protected_snapshot = protected_driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        protected_found = protected_driver.execute(
            "find",
            {
                "snapshot_id": protected_snapshot["snapshot_id"],
                "revision": protected_snapshot["revision"],
                "locator": {"attributes": {"id": "password"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as protected:
            protected_driver.execute(
                "type_text",
                {
                    "target": protected_found["target"],
                    "locator": {"attributes": {"id": "password"}},
                    "text": "secret",
                },
                deadline=deadline(),
            )
        self.assertEqual(protected.exception.code, "DRIVER.PROTECTED_ELEMENT")
        self.assertFalse(any(call[0] == "focus" for call in protected_backend.calls))
        self.assertFalse(any(call[0] == "type_text" for call in protected_helper.calls))

    def test_stale_revision_and_replaced_native_target_never_dispatch(self) -> None:
        replacement = default_tree()
        replacement[2] = node(
            "replacement",
            1,
            "push_button",
            "Save",
            actions=("focus", "invoke"),
            attributes={"class": "QPushButton", "id": "save"},
        )
        backend = FakeBackend([default_tree(), replacement])
        driver = atspi.LinuxAtspiDriver(backend)
        snapshot = driver.execute(
            "snapshot",
            {"application": {"bus_name": ":1.42"}},
            deadline=deadline(),
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as replaced:
            driver.execute(
                "invoke",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(replaced.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertFalse(any(call[0] == "invoke" for call in backend.calls))

        backend = FakeBackend()
        driver = atspi.LinuxAtspiDriver(backend)
        old = driver.execute(
            "snapshot",
            {"application": {"name": "Editor"}},
            deadline=deadline(),
        )
        driver.execute(
            "snapshot",
            {"application": {"name": "Editor"}},
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as stale:
            self.find(old, {"attributes": {"id": "save"}})
        self.assertEqual(stale.exception.code, "DRIVER.STALE_SNAPSHOT")

    def test_expand_and_collapse_no_op_only_after_fresh_state_observation(self) -> None:
        for action, expanded in (("expand", True), ("collapse", False)):
            with self.subTest(action=action):
                initial = default_tree()
                fresh = default_tree()
                fresh[5] = node(
                    "details",
                    1,
                    "toggle_button",
                    "Details",
                    actions=("expand", "collapse"),
                    expandable=True,
                    expanded=expanded,
                    attributes={"class": "GtkExpander", "id": "details"},
                )
                backend = FakeBackend([initial, fresh])
                driver = atspi.LinuxAtspiDriver(backend)
                captured = driver.execute(
                    "snapshot",
                    {"application": {"name": "Editor"}},
                    deadline=deadline(),
                )
                located = driver.execute(
                    "find",
                    {
                        "snapshot_id": captured["snapshot_id"],
                        "revision": captured["revision"],
                        "locator": {"attributes": {"id": "details"}},
                    },
                    deadline=deadline(),
                )
                result = driver.execute(
                    action,
                    {
                        "target": located["target"],
                        "locator": {"attributes": {"id": "details"}},
                    },
                    deadline=deadline(),
                )
                self.assertTrue(result["backend_result"]["no_op"])
                self.assertFalse(result["backend_result"]["dispatched"])
                self.assertEqual(
                    result["backend_result"]["native_action_name"], "activate"
                )
                self.assertFalse(any(call[0] == action for call in backend.calls))
                # A no-op never enters the dispatch boundary: the fresh snapshot
                # remains current and can still be used for exact resolution.
                still_current = driver.execute(
                    "find",
                    {
                        "snapshot_id": result["resolved"]["snapshot_id"],
                        "revision": result["resolved"]["revision"],
                        "locator": {"attributes": {"id": "details"}},
                    },
                    deadline=deadline(),
                )
                self.assertEqual(
                    still_current["target"]["node_id"],
                    result["resolved"]["node_id"],
                )

    def test_toggle_remains_non_idempotent_when_checked(self) -> None:
        tree = default_tree()
        tree[4] = node(
            "autosave",
            1,
            "check_box",
            "Autosave",
            actions=("toggle",),
            checked=True,
            attributes={"class": "GtkCheckButton", "id": "autosave"},
        )
        backend = FakeBackend([tree])
        driver = atspi.LinuxAtspiDriver(backend)
        captured = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        located = driver.execute(
            "find",
            {
                "snapshot_id": captured["snapshot_id"],
                "revision": captured["revision"],
                "locator": {"attributes": {"id": "autosave"}},
            },
            deadline=deadline(),
        )
        driver.execute(
            "toggle",
            {
                "target": located["target"],
                "locator": {"attributes": {"id": "autosave"}},
            },
            deadline=deadline(),
        )
        self.assertEqual(
            [call for call in backend.calls if call[0] == "toggle"],
            [("toggle", "autosave", None)],
        )

    def test_identity_check_failure_is_normalized_to_stale_without_dispatch(self) -> None:
        backend = IdentityFailureBackend()
        driver = atspi.LinuxAtspiDriver(backend)
        snapshot = driver.execute(
            "snapshot",
            {"application": {"name": "Editor"}},
            deadline=deadline(),
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as raised:
            driver.execute(
                "invoke",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(raised.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertEqual(raised.exception.data["reason"], "DRIVER.ACTION_FAILED")
        self.assertFalse(any(call[0] == "invoke" for call in backend.calls))

    def test_truncated_snapshot_fails_closed_for_find_and_write(self) -> None:
        backend = FakeBackend()
        backend.truncated = True
        driver = atspi.LinuxAtspiDriver(backend)
        snapshot = driver.execute(
            "snapshot",
            {"application": {"name": "Editor"}},
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as found:
            driver.execute(
                "find",
                {
                    "snapshot_id": snapshot["snapshot_id"],
                    "revision": snapshot["revision"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(found.exception.code, "DRIVER.SNAPSHOT_TRUNCATED")

        target = {
            "snapshot_id": snapshot["snapshot_id"],
            "revision": snapshot["revision"],
            "node_id": "n2",
        }
        with self.assertRaises(atspi.DriverError) as written:
            driver.execute(
                "invoke",
                {
                    "target": target,
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(written.exception.code, "DRIVER.SNAPSHOT_TRUNCATED")
        self.assertFalse(any(call[0] == "invoke" for call in backend.calls))

    def test_write_reuses_bounds_and_protected_text_is_rejected(self) -> None:
        snapshot = self.snapshot(max_depth=64, max_nodes=2000)
        found = self.find(snapshot, {"attributes": {"id": "save"}})
        self.driver.execute(
            "invoke",
            {
                "target": found["target"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        captures = [call for call in self.backend.calls if call[0] == "capture"]
        self.assertEqual(captures[-1][2:], (64, 2000))
        self.assertIn(("same_element", "save", "save"), self.backend.calls)

        protected_tree = default_tree()
        protected_tree[3] = node(
            "title",
            1,
            "password_text",
            "Password",
            value=None,
            actions=("focus", "set_text"),
            protected=True,
            attributes={"id": "title"},
        )
        backend = FakeBackend([protected_tree])
        driver = atspi.LinuxAtspiDriver(backend)
        snapshot = driver.execute(
            "snapshot",
            {"application": {"name": "Editor"}},
            deadline=deadline(),
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "title"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as protected:
            driver.execute(
                "set_text",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "title"}},
                    "text": "secret",
                },
                deadline=deadline(),
            )
        self.assertEqual(protected.exception.code, "DRIVER.PROTECTED_ELEMENT")
        self.assertFalse(any(call[0] == "set_text" for call in backend.calls))

    def test_native_adapter_treats_password_role_as_protected_without_state(self) -> None:
        class States:
            def contains(self, state: object) -> bool:
                return False

        class StateType:
            ENABLED = "enabled"
            VISIBLE = "visible"
            SHOWING = "showing"
            FOCUSABLE = "focusable"
            FOCUSED = "focused"
            EDITABLE = "editable"
            SENSITIVE = "sensitive"

        class TextInterface:
            def get_character_count(self) -> int:
                return 12

            def get_text(self, start: int, end: int) -> str:
                return "never-expose"

        class Accessible:
            app = type(
                "Application",
                (),
                {
                    "bus_name": ":1.9",
                    "get_toolkit_name": lambda self: "Qt",
                    "get_toolkit_version": lambda self: "5.15.8",
                },
            )()
            path = "/org/a11y/atspi/accessible/password"

            def get_state_set(self) -> object:
                return States()

            def get_role(self) -> object:
                return type("Role", (), {"value_nick": "password-text"})()

            def get_role_name(self) -> str:
                return "password text"

            def get_editable_text_iface(self) -> object:
                return object()

            def get_component_iface(self) -> None:
                return None

            def get_action_iface(self) -> None:
                return None

            def get_text_iface(self) -> object:
                return TextInterface()

            def get_attributes(self) -> dict[str, str]:
                return {}

            def get_accessible_id(self) -> str:
                return "password"

            def get_name(self) -> str:
                return "Password"

            def get_description(self) -> str:
                return ""

        backend = object.__new__(atspi.PyGObjectAtspiBackend)
        backend.Atspi = type(
            "Atspi",
            (),
            {"StateType": StateType, "set_timeout": staticmethod(lambda *_: None)},
        )
        native_node = backend._read_node(
            Accessible(),
            None,
            {"name": "Login", "toolkit_name": "Qt", "process_id": 9},
            deadline=deadline(),
        )
        self.assertEqual(native_node.role, "password_text")
        self.assertIsNone(native_node.value)
        self.assertTrue(native_node.states["protected"])
        self.assertTrue(native_node.provenance["value_redacted"])
        self.assertNotIn("set_text", native_node.actions)

    def test_pygobject_adapter_methods_receive_deadline_before_native_dispatch(self) -> None:
        calls: list[tuple[object, ...]] = []

        class StateType:
            PROTECTED = "protected"

        class StateSet:
            def contains(self, state: object) -> bool:
                calls.append(("contains", state))
                return False

        class Component:
            def grab_focus(self) -> bool:
                calls.append(("grab_focus",))
                return True

        class Action:
            def get_n_actions(self) -> int:
                return 1

            def get_action_name(self, index: int) -> str:
                return "press"

            def get_localized_name(self, index: int) -> str:
                return "press"

            def get_action_description(self, index: int) -> str:
                return "press"

            def get_key_binding(self, index: int) -> str:
                return ""

            def do_action(self, index: int) -> bool:
                calls.append(("do_action", index))
                return True

        class EditableText:
            def set_text_contents(self, text: str) -> bool:
                calls.append(("set_text_contents", text))
                return True

        class Accessible:
            def get_component_iface(self) -> object:
                return Component()

            def get_action_iface(self) -> object:
                return Action()

            def get_editable_text_iface(self) -> object:
                return EditableText()

            def get_state_set(self) -> object:
                return StateSet()

            def get_role(self) -> object:
                return type("Role", (), {"value_nick": "entry"})()

        backend = object.__new__(atspi.PyGObjectAtspiBackend)
        backend.Atspi = type(
            "Atspi",
            (),
            {
                "StateType": StateType,
                "set_timeout": staticmethod(
                    lambda value, startup: calls.append(
                        ("set_timeout", value, startup)
                    )
                ),
            },
        )
        native = Accessible()
        self.assertTrue(backend.focus(native, deadline=deadline())["accepted"])
        self.assertTrue(backend.invoke(native, deadline=deadline())["accepted"])
        self.assertTrue(
            backend.set_text(native, "Final", deadline=deadline())["accepted"]
        )
        self.assertIn(("grab_focus",), calls)
        self.assertIn(("do_action", 0), calls)
        self.assertIn(("set_text_contents", "Final"), calls)
        self.assertGreaterEqual(
            sum(1 for call in calls if call[0] == "set_timeout"), 6
        )

    def test_pygobject_point_lookup_stops_on_repeated_accessible(self) -> None:
        calls: list[str] = []

        class Component:
            def __init__(self, hit: object | None = None) -> None:
                self.hit = hit

            def get_extents(self, coord_type: object) -> object:
                return type(
                    "Rectangle", (),
                    {"x": 0, "y": 0, "width": 100, "height": 100},
                )()

            def get_accessible_at_point(
                self, x: int, y: int, coord_type: object
            ) -> object:
                calls.append("hit")
                assert self.hit is not None
                return self.hit

        class Accessible:
            app = type("Application", (), {"bus_name": ":1.9"})()

            def __init__(self, path: str) -> None:
                self.path = path
                self.component = Component()

            def get_component_iface(self) -> object:
                return self.component

        first = Accessible("/first")
        second = Accessible("/second")
        first.component.hit = second
        second.component.hit = first
        backend = object.__new__(atspi.PyGObjectAtspiBackend)
        backend.Atspi = type(
            "Atspi",
            (),
            {
                "CoordType": type("CoordType", (), {"SCREEN": "screen"}),
                "set_timeout": staticmethod(lambda *_: None),
            },
        )
        backend.same_element = lambda previous, current, **_: previous is current
        hit = backend.accessible_at_point(first, 50, 50, deadline=deadline())
        self.assertIs(hit, first)
        self.assertEqual(calls, ["hit", "hit"])

    def test_pygobject_point_lookup_skips_zero_area_container(self) -> None:
        class Component:
            calls = 0

            @staticmethod
            def get_extents(coord_type: object) -> object:
                return type(
                    "Rectangle", (),
                    {"x": 0, "y": 0, "width": 0, "height": 100},
                )()

            @classmethod
            def get_accessible_at_point(
                cls, x: int, y: int, coord_type: object
            ) -> object:
                cls.calls += 1
                raise AssertionError("zero-area component must not be queried")

        class Accessible:
            app = type("Application", (), {"bus_name": ":1.9"})()
            path = "/zero-area"

            @staticmethod
            def get_component_iface() -> object:
                return Component()

        backend = object.__new__(atspi.PyGObjectAtspiBackend)
        backend.Atspi = type(
            "Atspi",
            (),
            {
                "CoordType": type("CoordType", (), {"SCREEN": "screen"}),
                "set_timeout": staticmethod(lambda *_: None),
            },
        )
        self.assertIsNone(
            backend.accessible_at_point(
                Accessible(), 50, 50, deadline=deadline()
            )
        )
        self.assertEqual(Component.calls, 0)

    def test_pygobject_point_lookup_has_a_hard_hop_limit(self) -> None:
        class Component:
            def __init__(self, owner: object) -> None:
                self.owner = owner

            @staticmethod
            def get_extents(coord_type: object) -> object:
                return type(
                    "Rectangle", (),
                    {"x": 0, "y": 0, "width": 100, "height": 100},
                )()

            def get_accessible_at_point(
                self, x: int, y: int, coord_type: object
            ) -> object:
                return type(self.owner)(self.owner.index + 1)

        class Accessible:
            app = type("Application", (), {"bus_name": ":1.9"})()

            def __init__(self, index: int) -> None:
                self.index = index
                self.path = f"/node/{index}"

            def get_component_iface(self) -> object:
                return Component(self)

        backend = object.__new__(atspi.PyGObjectAtspiBackend)
        backend.Atspi = type(
            "Atspi",
            (),
            {
                "CoordType": type("CoordType", (), {"SCREEN": "screen"}),
                "set_timeout": staticmethod(lambda *_: None),
            },
        )
        backend.same_element = lambda previous, current, **_: previous is current
        hit = backend.accessible_at_point(
            Accessible(0), 50, 50, deadline=deadline()
        )
        self.assertEqual(hit.index, atspi.MAX_DEPTH + 1)

    def test_pygobject_named_actions_match_only_exact_gtk3_canonical_names(self) -> None:
        calls: list[tuple[object, ...]] = []

        class Action:
            names = ("press", "click", "activate")

            def get_n_actions(self) -> int:
                return len(self.names)

            def get_action_name(self, index: int) -> str:
                return self.names[index]

            def get_localized_name(self, index: int) -> str:
                return "click" if index == 0 else self.names[index].title()

            def get_action_description(self, index: int) -> str:
                return f"description {self.names[index]}"

            def get_key_binding(self, index: int) -> str:
                return ""

            def do_action(self, index: int) -> bool:
                calls.append(("do_action", index))
                return True

        class Accessible:
            def get_action_iface(self) -> object:
                return Action()

        backend = object.__new__(atspi.PyGObjectAtspiBackend)
        backend.Atspi = type(
            "Atspi",
            (),
            {"set_timeout": staticmethod(lambda *_: None)},
        )
        toggle = backend.toggle(Accessible(), deadline=deadline())
        expanded = backend.expand(Accessible(), deadline=deadline())
        collapsed = backend.collapse(Accessible(), deadline=deadline())
        self.assertEqual(toggle["native_action_name"], "click")
        self.assertEqual(expanded["native_action_name"], "activate")
        self.assertEqual(collapsed["native_action_name"], "activate")
        self.assertEqual(calls, [("do_action", 1), ("do_action", 2), ("do_action", 2)])

        Action.names = ("Click", " click ", "Activate")
        with self.assertRaises(atspi.DriverError) as unsupported:
            backend.toggle(Accessible(), deadline=deadline())
        self.assertEqual(unsupported.exception.code, "DRIVER.ACTION_UNSUPPORTED")
        self.assertEqual(unsupported.exception.data["native_action_name"], "click")
        Action.names = ("click", "click")
        with self.assertRaises(atspi.DriverError) as duplicate:
            backend.toggle(Accessible(), deadline=deadline())
        self.assertEqual(duplicate.exception.code, "DRIVER.ACTION_UNSUPPORTED")

    def test_pygobject_qt5_invoke_selects_only_exact_press(self) -> None:
        calls: list[tuple[str, int]] = []

        class Action:
            names = ("Press", "SetFocus")

            def get_n_actions(self) -> int:
                return len(self.names)

            def get_action_name(self, index: int) -> str:
                return self.names[index]

            def get_localized_name(self, index: int) -> str:
                return self.names[index]

            def get_action_description(self, index: int) -> str:
                return ""

            def get_key_binding(self, index: int) -> str:
                return ""

            def do_action(self, index: int) -> bool:
                calls.append(("do_action", index))
                return True

        class Accessible:
            app = type(
                "Application",
                (),
                {
                    "bus_name": ":1.9",
                    "get_toolkit_name": lambda self: "Qt",
                    "get_toolkit_version": lambda self: "5.15.8",
                },
            )()
            path = "/org/a11y/atspi/accessible/button"

            def get_action_iface(self) -> object:
                return Action()

            def get_role(self) -> object:
                return type("Role", (), {"value_nick": "push-button"})()

            def get_role_name(self) -> str:
                return "push button"

        backend = object.__new__(atspi.PyGObjectAtspiBackend)
        backend.Atspi = type(
            "Atspi", (), {"set_timeout": staticmethod(lambda *_: None)}
        )
        backend._applications = lambda **_: [
            (object(), {
                "bus_name": ":1.9",
                "toolkit_name": "Qt",
                "toolkit_version": "5.15.8",
                "process_id": 42,
            })
        ]
        result = backend.invoke(Accessible(), deadline=deadline())
        self.assertEqual(result["native_action_name"], "Press")
        self.assertEqual(calls, [("do_action", 0)])

        Action.names = ("press", "SetFocus")
        with self.assertRaises(atspi.DriverError) as wrong_case:
            backend.invoke(Accessible(), deadline=deadline())
        self.assertEqual(wrong_case.exception.code, "DRIVER.ACTION_UNSUPPORTED")

    def test_pygobject_gtk3_state_actions_are_observed_without_guessing(self) -> None:
        class StateType:
            ENABLED = "enabled"
            VISIBLE = "visible"
            SHOWING = "showing"
            FOCUSABLE = "focusable"
            FOCUSED = "focused"
            EDITABLE = "editable"
            SENSITIVE = "sensitive"
            PROTECTED = "protected"
            CHECKED = "checked"
            EXPANDABLE = "expandable"
            EXPANDED = "expanded"
            SELECTABLE = "selectable"
            SELECTED = "selected"

        class States:
            values = {
                "enabled",
                "visible",
                "showing",
                "sensitive",
                "checked",
                "selectable",
                "selected",
            }

            def contains(self, state: object) -> bool:
                return state in self.values

        class Action:
            def get_n_actions(self) -> int:
                return 1

            def get_action_name(self, index: int) -> str:
                return "click"

            def get_localized_name(self, index: int) -> str:
                return "Toggle"

            def get_action_description(self, index: int) -> str:
                return ""

            def get_key_binding(self, index: int) -> str:
                return ""

        class Accessible:
            app = type("Application", (), {"bus_name": ":1.9"})()
            path = "/org/a11y/atspi/accessible/check"

            def get_state_set(self) -> object:
                return States()

            def get_role(self) -> object:
                return type("Role", (), {"value_nick": "check-box"})()

            def get_role_name(self) -> str:
                return "check box"

            def get_action_iface(self) -> object:
                return Action()

            def get_editable_text_iface(self) -> None:
                return None

            def get_component_iface(self) -> None:
                return None

            def get_text_iface(self) -> None:
                return None

            def get_attributes(self) -> dict[str, str]:
                return {}

            def get_accessible_id(self) -> str:
                return "fixture-check"

            def get_name(self) -> str:
                return "Check"

            def get_description(self) -> str:
                return ""

        backend = object.__new__(atspi.PyGObjectAtspiBackend)
        backend.Atspi = type(
            "Atspi",
            (),
            {
                "StateType": StateType,
                "set_timeout": staticmethod(lambda *_: None),
            },
        )
        native_node = backend._read_node(
            Accessible(),
            None,
            {
                "name": "Fixture",
                "toolkit_name": "gtk",
                "toolkit_version": "3.24.33",
                "process_id": 9,
            },
            deadline=deadline(),
        )
        self.assertTrue(native_node.states["checked"])
        self.assertFalse(native_node.states["expandable"])
        self.assertTrue(native_node.states["selectable"])
        self.assertTrue(native_node.states["selected"])
        self.assertIn("toggle", native_node.actions)
        self.assertEqual(native_node.provenance["native_action_name"], "click")
        self.assertEqual(
            native_node.provenance["native_action_names"], {"toggle": "click"}
        )

    def test_unsupported_action_unknown_effect_and_deadline_are_structured(self) -> None:
        snapshot = self.snapshot()
        found = self.find(snapshot, {"attributes": {"id": "save"}})
        with self.assertRaises(atspi.DriverError) as unsupported:
            self.driver.execute(
                "set_text",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "save"}},
                    "text": "x",
                },
                deadline=deadline(),
            )
        self.assertEqual(unsupported.exception.code, "DRIVER.ACTION_UNSUPPORTED")

        backend = FakeBackend()
        backend.fail = "invoke"
        driver = atspi.LinuxAtspiDriver(backend)
        snapshot = driver.execute(
            "snapshot",
            {"application": {"name": "Editor"}},
            deadline=deadline(),
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "save"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as failed:
            driver.execute(
                "invoke",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "save"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(failed.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertEqual(failed.exception.data["effect"], "unknown")
        self.assertFalse(failed.exception.retryable)

        backend = FakeBackend()
        backend.fail = "toggle"
        driver = atspi.LinuxAtspiDriver(backend)
        snapshot = driver.execute(
            "snapshot", {"application": {"name": "Editor"}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"attributes": {"id": "autosave"}},
            },
            deadline=deadline(),
        )
        with self.assertRaises(atspi.DriverError) as toggle_failed:
            driver.execute(
                "toggle",
                {
                    "target": found["target"],
                    "locator": {"attributes": {"id": "autosave"}},
                },
                deadline=deadline(),
            )
        self.assertEqual(toggle_failed.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertEqual(toggle_failed.exception.data["action"], "toggle")
        self.assertFalse(toggle_failed.exception.retryable)

        with self.assertRaises(atspi.DriverError) as timed_out:
            self.driver.execute(
                "list_applications", {}, deadline=time.monotonic() - 1
            )
        self.assertEqual(timed_out.exception.code, "DRIVER.TIMEOUT")
        self.assertTrue(timed_out.exception.retryable)


class LinuxAtspiProcessTests(unittest.TestCase):
    def make_plugin(self) -> ProcessPlugin:
        plugin = ProcessPlugin(
            [sys.executable, str(DRIVER_PATH)],
            timeout=3,
            name="desktop.linux_atspi",
        )
        self.addCleanup(plugin.close)
        return plugin

    def test_xtest_helper_uses_stdin_and_post_dispatch_timeout_is_unknown(self) -> None:
        class TimedOutProcess:
            returncode: int | None = None

            def __init__(self) -> None:
                self.calls = 0
                self.killed = False

            def communicate(
                self, input: bytes | None = None, timeout: float | None = None
            ) -> tuple[bytes, bytes]:
                self.calls += 1
                if self.calls == 1:
                    self.assert_input = input
                    raise atspi.subprocess.TimeoutExpired(
                        ["helper"],
                        timeout or 0,
                        output=b'{"event":"dispatch_started"}\n',
                    )
                return b"", b""

            def kill(self) -> None:
                self.killed = True

        process = TimedOutProcess()
        helper = atspi.XTestHelper(Path("/fixed/x11_xtest_helper"))
        with (
            mock.patch.object(
                helper,
                "_qualified_environment",
                return_value={"DISPLAY": ":99", "XDG_SESSION_TYPE": "x11"},
            ),
            mock.patch.object(
                helper, "_validated_path", return_value=str(helper.path)
            ),
            mock.patch.object(
                atspi.subprocess, "Popen", return_value=process
            ) as popen,
        ):
            with self.assertRaises(atspi.DriverError) as raised:
                helper.type_text(
                    "secret-free text",
                    expected_process_id=77,
                    deadline=time.monotonic() + 1,
                )
        self.assertEqual(raised.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertFalse(raised.exception.retryable)
        self.assertEqual(raised.exception.data["effect"], "unknown")
        self.assertTrue(process.killed)
        command = popen.call_args.args[0]
        self.assertNotIn("secret-free text", command)
        self.assertEqual(process.assert_input, b"secret-free text")
        self.assertNotIn("shell", popen.call_args.kwargs)

    def test_xtest_helper_success_reports_submitted_not_accepted(self) -> None:
        class SuccessfulProcess:
            returncode = 0

            @staticmethod
            def communicate(
                input: bytes | None = None, timeout: float | None = None
            ) -> tuple[bytes, bytes]:
                return (
                    b'{"event":"dispatch_started"}\n'
                    b'{"ok":true,"dispatch_started":true,"events":2,'
                    b'"codepoints":1}\n',
                    b"",
                )

        helper = atspi.XTestHelper(Path("/fixed/x11_xtest_helper"))
        with (
            mock.patch.object(helper, "_qualified_environment", return_value={"DISPLAY": ":99"}),
            mock.patch.object(helper, "_validated_path", return_value=str(helper.path)),
            mock.patch.object(atspi.subprocess, "Popen", return_value=SuccessfulProcess()),
        ):
            result = helper.type_text(
                "x", expected_process_id=77, deadline=time.monotonic() + 1
            )
        self.assertTrue(result["submitted"])
        self.assertNotIn("accepted", result)

    def test_xtest_helper_session_gate_is_fail_closed(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "DISPLAY": ":1",
                "XDG_SESSION_TYPE": "wayland",
                "XDG_CURRENT_DESKTOP": "KDE",
            },
            clear=True,
        ), self.assertRaises(atspi.DriverError) as raised:
            atspi.XTestHelper._qualified_environment()
        self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
        self.assertEqual(raised.exception.data["reason"], "unsupported_session")

    def test_xtest_helper_rejects_untrusted_executable(self) -> None:
        helper_path = Path(self.id().replace(".", "_"))
        helper = atspi.XTestHelper(helper_path)
        with mock.patch.object(Path, "lstat") as lstat, mock.patch.object(
            os, "access", return_value=True
        ):
            lstat.return_value = type(
                "Details",
                (),
                {
                    "st_mode": stat.S_IFREG | stat.S_IRWXU | stat.S_IWGRP,
                    "st_uid": os.geteuid(),
                },
            )()
            with self.assertRaises(atspi.DriverError) as raised:
                helper._validated_path()
        self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
        self.assertEqual(raised.exception.data["reason"], "helper_untrusted")

    def test_manifest_is_linux_only_and_uses_action_specific_permissions(self) -> None:
        manifest = self.make_plugin().start()
        self.assertEqual(manifest["metadata"]["name"], "desktop.linux_atspi")
        self.assertEqual(manifest["runtime"]["platforms"], ["linux"])
        self.assertEqual(manifest["runtime"]["entrypoint"], "./run.sh")
        self.assertEqual(
            set(manifest["actions"]),
            {
                "inspect_session",
                "list_applications",
                "snapshot",
                "find",
                "focus",
                "invoke",
                "pointer_click",
                "set_text",
                "type_text",
                "toggle",
                "expand",
                "collapse",
            },
        )
        for name in ("inspect_session", "list_applications", "snapshot", "find"):
            self.assertEqual(
                manifest["actions"][name]["permissions"],
                ["desktop.observe"],
            )
        durable = manifest["actions"]["inspect_session"]
        self.assertEqual(
            durable["sensitivity"],
            {"input": "public", "output": "public", "error": "public"},
        )
        self.assertEqual(
            set(durable["durability"]["checkpoint_fields"]),
            {"backend", "session_type", "desktop"},
        )
        for name in ("list_applications", "snapshot", "find"):
            self.assertNotIn("durability", manifest["actions"][name])
        for name in (
            "focus",
            "invoke",
            "pointer_click",
            "set_text",
            "type_text",
            "toggle",
            "expand",
            "collapse",
        ):
            self.assertEqual(
                manifest["actions"][name]["permissions"],
                ["desktop.observe", "desktop.input"],
            )
        self.assertEqual(
            manifest["actions"]["toggle"]["effect"]["default_class"],
            "non_idempotent",
        )
        self.assertEqual(
            manifest["actions"]["type_text"]["effect"]["default_class"],
            "contextual",
        )
        self.assertEqual(
            manifest["actions"]["type_text"]["risk"],
            {"category": "input", "level": "high"},
        )
        self.assertEqual(
            manifest["actions"]["pointer_click"]["effect"]["default_class"],
            "non_idempotent",
        )
        self.assertEqual(
            manifest["actions"]["pointer_click"]["risk"],
            {"category": "modify", "level": "high"},
        )
        for name in ("expand", "collapse"):
            self.assertEqual(
                manifest["actions"][name]["effect"]["default_class"],
                "idempotent",
            )
        for name, contract in manifest["actions"].items():
            self.assertEqual(contract["contract_major"], 1, name)
            self.assertIn(f"desktop.linux_atspi.{name}@1", atspi.ACTION_NAMES)

    def test_expired_deadline_unknown_action_and_unavailable_are_structured(self) -> None:
        with self.assertRaises(atspi.DriverError) as expired:
            atspi._wire_deadline(int((time.time() - 1) * 1000))
        self.assertEqual(expired.exception.code, "DRIVER.TIMEOUT")

        request = {
            "type": "invoke",
            "id": "x",
            "action": "snapshot",
            "args": {},
        }
        completed = __import__("subprocess").run(
            [sys.executable, str(DRIVER_PATH)],
            input=json.dumps(request) + "\n",
            text=True,
            capture_output=True,
            check=True,
            timeout=5,
        )
        response = json.loads(completed.stdout)
        self.assertEqual(response["error"]["code"], "PROTOCOL.ACTION_NOT_FOUND")

        unavailable = atspi.UnavailableBackend("dependency_missing", detail="Atspi")
        self.assertIn("session_type", unavailable.session_info())
        with self.assertRaises(atspi.DriverError) as raised:
            unavailable.list_applications(deadline=deadline())
        self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
        self.assertEqual(raised.exception.data["reason"], "dependency_missing")

    def test_request_reader_enforces_utf8_and_size_then_continues(self) -> None:
        oversized = b"{" + b"x" * atspi.MAX_REQUEST_BYTES + b"}\n"
        invalid_utf8 = b"\xff\n"
        invalid_json = b"{not-json}\n"
        valid = json.dumps({"type": "manifest", "id": "after"}).encode() + b"\n"
        output = io.BytesIO()
        original_stdout = sys.stdout

        class BinaryStdout:
            buffer = output

        try:
            sys.stdout = BinaryStdout()
            atspi.serve(
                atspi.LinuxAtspiDriver(FakeBackend()),
                io.BytesIO(oversized + invalid_utf8 + invalid_json + valid),
            )
        finally:
            sys.stdout = original_stdout
        messages = [json.loads(line) for line in output.getvalue().decode().splitlines()]
        self.assertEqual(messages[0]["error"]["code"], "PROTOCOL.REQUEST_TOO_LARGE")
        self.assertEqual(messages[1]["error"]["code"], "PROTOCOL.INVALID_ENCODING")
        self.assertEqual(messages[2]["error"]["code"], "PROTOCOL.PARSE_ERROR")
        self.assertEqual(messages[3]["id"], "after")
        self.assertEqual(
            messages[3]["result"]["metadata"]["name"],
            "desktop.linux_atspi",
        )

    def test_response_limit_and_process_deadline_fail_with_structured_errors(self) -> None:
        output = io.BytesIO()
        original_stdout = sys.stdout

        class BinaryStdout:
            buffer = output

        try:
            sys.stdout = BinaryStdout()
            with mock.patch.object(atspi, "MAX_RESPONSE_BYTES", 512):
                atspi.emit({"id": "large", "result": {"text": "x" * 2048}})
        finally:
            sys.stdout = original_stdout
        response = json.loads(output.getvalue())
        self.assertEqual(response["error"]["code"], "DRIVER.OUTPUT_TOO_LARGE")
        self.assertLessEqual(len(output.getvalue()), 512)

        request = {
            "type": "invoke",
            "id": "expired",
            "action": "desktop.linux_atspi.list_applications@1",
            "args": {},
            "deadline_ms": int((time.time() - 1) * 1000),
        }
        completed = __import__("subprocess").run(
            [sys.executable, str(DRIVER_PATH)],
            input=json.dumps(request) + "\n",
            text=True,
            capture_output=True,
            check=True,
            timeout=5,
        )
        response = json.loads(completed.stdout)
        self.assertEqual(response["error"]["code"], "DRIVER.TIMEOUT")
        self.assertTrue(response["error"]["retryable"])

    @unittest.skipUnless(sys.platform.startswith("linux"), "仅在 Linux 检查真实后端")
    def test_linux_backend_smoke_is_conservative(self) -> None:
        backend = atspi.create_default_backend()
        if isinstance(backend, atspi.UnavailableBackend):
            with self.assertRaises(atspi.DriverError) as raised:
                backend.list_applications(deadline=deadline())
            self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
            self.assertIn("session_type", raised.exception.data["session"])
            return
        if (
            os.environ.get("XDG_SESSION_TYPE", "").lower() != "x11"
            or not os.environ.get("DISPLAY")
        ):
            self.skipTest("当前进程不在可资格验证的 X11 图形会话中")
        applications = backend.list_applications(deadline=deadline())
        self.assertIsInstance(applications, list)

    def test_gio_fallback_write_methods_are_explicitly_unsupported(self) -> None:
        backend = object.__new__(atspi.GioAtspiBackend)
        native = atspi.GioAccessibleRef(":1.42", "/org/a11y/atspi/accessible/1")
        calls = (
            ("focus", (native,)),
            ("invoke", (native,)),
            ("set_text", (native, "value")),
            ("toggle", (native,)),
            ("expand", (native,)),
            ("collapse", (native,)),
        )
        for action, args in calls:
            with self.subTest(action=action), self.assertRaises(
                atspi.DriverError
            ) as raised:
                getattr(backend, action)(*args, deadline=deadline())
            self.assertEqual(raised.exception.code, "DRIVER.ACTION_UNSUPPORTED")
            self.assertEqual(raised.exception.data["backend"], "gio_atspi")
            self.assertEqual(raised.exception.data["action"], action)

    def test_gio_fallback_observes_new_state_bits_without_writes(self) -> None:
        backend = object.__new__(atspi.GioAtspiBackend)
        state_indexes = (4, 9, 10, 22, 23)
        word = sum(1 << index for index in state_indexes)
        backend._try_call = mock.Mock(return_value=([word, 0],))
        states = backend._state_values(
            atspi.GioAccessibleRef(
                ":1.42", "/org/a11y/atspi/accessible/1"
            ),
            deadline=deadline(),
        )
        self.assertTrue(states["checked"])
        self.assertTrue(states["expandable"])
        self.assertTrue(states["expanded"])
        self.assertTrue(states["selectable"])
        self.assertTrue(states["selected"])
        self.assertIsNone(states["protected"])

    def test_default_backend_requires_explicit_kde_x11_session(self) -> None:
        cases = (
            ({}, "missing session metadata"),
            (
                {
                    "XDG_SESSION_TYPE": "wayland",
                    "XDG_CURRENT_DESKTOP": "KDE",
                    "DISPLAY": ":1",
                },
                "Wayland",
            ),
            (
                {
                    "XDG_SESSION_TYPE": "x11",
                    "XDG_CURRENT_DESKTOP": "GNOME",
                    "DISPLAY": ":1",
                },
                "GNOME",
            ),
            (
                {
                    "XDG_SESSION_TYPE": "x11",
                    "XDG_CURRENT_DESKTOP": "NOTKDE",
                    "DISPLAY": ":1",
                },
                "desktop token containing KDE",
            ),
            (
                {
                    "XDG_SESSION_TYPE": "x11",
                    "XDG_CURRENT_DESKTOP": "KDE",
                },
                "missing DISPLAY",
            ),
        )
        for environment, label in cases:
            with self.subTest(label=label), mock.patch.dict(
                os.environ, environment, clear=True
            ), mock.patch.object(atspi, "PyGObjectAtspiBackend") as pygobject, mock.patch.object(
                atspi, "GioAtspiBackend"
            ) as gio:
                backend = atspi.create_default_backend()
            self.assertIsInstance(backend, atspi.UnavailableBackend)
            self.assertEqual(backend.reason, "unsupported_session")
            pygobject.assert_not_called()
            gio.assert_not_called()

    def test_default_backend_uses_gio_after_atspi_typelib_failure(self) -> None:
        fallback = object()
        environment = {
            "XDG_SESSION_TYPE": "x11",
            "XDG_CURRENT_DESKTOP": "KDE",
            "DISPLAY": ":10.0",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1001/bus",
        }

        class MissingAtspi:
            def __init__(self) -> None:
                raise atspi.DriverError(
                    "DRIVER.UNAVAILABLE",
                    "missing Atspi",
                    data={"reason": "dependency_missing"},
                )

        class GioFallback:
            def __new__(cls) -> object:
                return fallback

        with mock.patch.dict(os.environ, environment, clear=True), mock.patch.object(
            atspi, "PyGObjectAtspiBackend", MissingAtspi
        ), mock.patch.object(
            atspi, "GioAtspiBackend", GioFallback
        ):
            self.assertIs(atspi.create_default_backend(), fallback)

    def test_gio_children_response_has_post_decode_hard_cap(self) -> None:
        backend = object.__new__(atspi.GioAtspiBackend)
        too_many = [
            (":1.42", f"/org/a11y/atspi/accessible/{index}")
            for index in range(atspi.MAX_DBUS_CHILDREN_PER_CALL + 1)
        ]
        backend._call = mock.Mock(return_value=(too_many,))
        with self.assertRaises(atspi.DriverError) as raised:
            backend._children(
                atspi.GioAccessibleRef(
                    "org.a11y.atspi.Registry",
                    "/org/a11y/atspi/accessible/root",
                ),
                deadline=deadline(),
            )
        self.assertEqual(raised.exception.code, "DRIVER.OUTPUT_TOO_LARGE")
        self.assertEqual(
            raised.exception.data["limit"], atspi.MAX_DBUS_CHILDREN_PER_CALL
        )


if __name__ == "__main__":
    unittest.main()
