"""Linux AT-SPI 进程驱动的跨平台契约测试。"""

from __future__ import annotations

import copy
import importlib.util
import io
import json
import os
from pathlib import Path
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
            "editable": "set_text" in actions,
            "sensitive": True,
            "protected": protected,
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
            actions=("focus", "invoke"),
            attributes={"class": "QPushButton", "id": "save"},
        ),
        node(
            "title",
            1,
            "text",
            "Title",
            value="Draft",
            actions=("focus", "set_text"),
            attributes={"class": "QLineEdit", "id": "title"},
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

    def same_element(
        self, previous: object, current: object, *, deadline: float
    ) -> bool:
        self.calls.append(("same_element", previous, current))
        return previous == current


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
            ["n0", "n1", "n2", "n3"],
        )
        save = snapshot["nodes"][2]
        self.assertEqual(save["parent_id"], "n1")
        self.assertEqual(save["description"], "Save document")
        self.assertEqual(save["actions"], ["focus", "invoke"])
        self.assertEqual(save["attributes"]["class"], "QPushButton")
        self.assertEqual(save["provenance"]["backend"], "fake_linux_atspi")

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
        self.assertEqual(found["target"]["node_id"], "n4")
        found = self.find(snapshot, {"description": "Save document"})
        self.assertEqual(found["target"]["node_id"], "n2")
        found = self.find(
            snapshot,
            {"object_path": "/org/a11y/atspi/accessible/save"},
        )
        self.assertEqual(found["target"]["node_id"], "n2")

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
            ("set_text", {"attributes": {"id": "title"}}, {"text": "Final"}),
        )
        for action, locator, extra in cases:
            with self.subTest(action=action):
                backend = FakeBackend()
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
                if action == "set_text":
                    self.assertEqual(action_calls[0], ("set_text", "title", "Final"))

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
            app = type("Application", (), {"bus_name": ":1.9"})()
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

    def test_manifest_is_linux_only_and_uses_action_specific_permissions(self) -> None:
        manifest = self.make_plugin().start()
        self.assertEqual(manifest["metadata"]["name"], "desktop.linux_atspi")
        self.assertEqual(manifest["runtime"]["platforms"], ["linux"])
        self.assertEqual(manifest["runtime"]["entrypoint"], "./run.sh")
        self.assertEqual(
            set(manifest["actions"]),
            {
                "list_applications",
                "snapshot",
                "find",
                "focus",
                "invoke",
                "set_text",
            },
        )
        for name in ("list_applications", "snapshot", "find"):
            self.assertEqual(
                manifest["actions"][name]["permissions"],
                ["desktop.observe"],
            )
        for name in ("focus", "invoke", "set_text"):
            self.assertEqual(
                manifest["actions"][name]["permissions"],
                ["desktop.observe", "desktop.input"],
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
        )
        for action, args in calls:
            with self.subTest(action=action), self.assertRaises(
                atspi.DriverError
            ) as raised:
                getattr(backend, action)(*args, deadline=deadline())
            self.assertEqual(raised.exception.code, "DRIVER.ACTION_UNSUPPORTED")
            self.assertEqual(raised.exception.data["backend"], "gio_atspi")
            self.assertEqual(raised.exception.data["action"], action)

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
