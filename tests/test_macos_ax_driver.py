"""Cross-platform contracts for the macOS AX process driver."""

from __future__ import annotations

import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from ai_auto_desktop.plugin import PluginError, ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
PLUGIN_ROOT = PROJECT_ROOT / "plugins" / "macos_ax"
DRIVER_PATH = PLUGIN_ROOT / "macos_ax_driver.py"
RUN_SCRIPT = PLUGIN_ROOT / "run.sh"
BUILD_SCRIPT = PLUGIN_ROOT / "build.sh"
HELPER_SOURCE = PLUGIN_ROOT / "swift" / "MacOSAXHelper.swift"

SPEC = importlib.util.spec_from_file_location("testable_macos_ax_driver", DRIVER_PATH)
assert SPEC is not None and SPEC.loader is not None
ax = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ax
SPEC.loader.exec_module(ax)


def deadline() -> float:
    return time.monotonic() + 5.0


def node(
    key: str,
    parent_index: int | None,
    role: str,
    name: str | None,
    *,
    identifier: str | None = None,
    value: str | None = None,
    actions: tuple[str, ...] = (),
    protected: bool = False,
) -> object:
    return ax.BackendNode(
        native=key,
        parent_index=parent_index,
        role=role,
        subrole="AXSecureTextField" if protected else None,
        name=name,
        description=f"description:{name}" if name else None,
        value=value,
        states={
            "enabled": True,
            "focused": False,
            "focusable": "focus" in actions,
            "editable": "set_value" in actions,
            "protected": protected,
        },
        bounds={"x": 10, "y": 20, "width": 120, "height": 30},
        actions=actions,
        provenance={
            "identifier": identifier,
            "process_id": 7,
            "bundle_id": "dev.example.Editor",
            "coordinate_space": "screen_points",
        },
    )


def default_tree() -> list[object]:
    return [
        node("app", None, "AXApplication", "Editor", identifier="app"),
        node("window", 0, "AXWindow", "Editor", identifier="main", actions=("focus",)),
        node(
            "save",
            1,
            "AXButton",
            "Save",
            identifier="save",
            actions=("focus", "invoke"),
        ),
        node(
            "title",
            1,
            "AXTextField",
            "Title",
            identifier="title",
            value="Draft",
            actions=("focus", "set_value"),
        ),
    ]


class FakeBackend:
    name = "fake_macos_ax"

    def __init__(self, snapshots: list[list[object]] | None = None) -> None:
        self.snapshots = snapshots or [default_tree()]
        self.capture_count = 0
        self.calls: list[tuple[object, ...]] = []
        self.fail: str | None = None
        self.truncated = False

    def list_apps(self, *, deadline: float) -> dict[str, object]:
        self.calls.append(("list_apps",))
        return {
            "accessibility_trusted": True,
            "apps": [
                {
                    "process_id": 7,
                    "bundle_id": "dev.example.Editor",
                    "name": "Editor",
                    "active": True,
                }
            ],
        }

    def capture(
        self, app: object, *, max_depth: int, max_nodes: int, deadline: float
    ) -> object:
        self.calls.append(("capture", copy.deepcopy(app), max_depth, max_nodes))
        index = min(self.capture_count, len(self.snapshots) - 1)
        self.capture_count += 1
        return ax.BackendSnapshot(
            app={
                "process_id": 7,
                "bundle_id": "dev.example.Editor",
                "name": "Editor",
            },
            nodes=copy.deepcopy(self.snapshots[index]),
            truncated=self.truncated,
        )

    def _action(
        self, action: str, native: object, value: str | None = None
    ) -> dict[str, object]:
        self.calls.append((action, native, value))
        if self.fail == action:
            raise RuntimeError("synthetic native failure")
        return {"native_operation": action, "accepted": True}

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


class HelperFaultBackend(FakeBackend):
    def __init__(self, code: str, *, request_dispatched: bool) -> None:
        super().__init__()
        self.code = code
        self.request_dispatched = request_dispatched

    def invoke(self, native: object, *, deadline: float) -> object:
        self.calls.append(("invoke_attempt", native))
        raise ax.DriverError(
            self.code,
            "synthetic helper channel failure",
            retryable=self.code == "DRIVER.TIMEOUT",
            data={
                "helper_channel_failure": True,
                "helper_request_dispatched": self.request_dispatched,
                "effect": "unknown" if self.request_dispatched else "not_applied",
            },
        )


class IdentityChannelFaultBackend(FakeBackend):
    def same_element(
        self, previous: object, current: object, *, deadline: float
    ) -> bool:
        raise ax.DriverError(
            "DRIVER.UNAVAILABLE",
            "synthetic helper identity channel failure",
            data={
                "helper_channel_failure": True,
                "helper_request_dispatched": True,
                "effect": "unknown",
            },
        )


class DummyStream:
    def __init__(self, descriptor: int) -> None:
        self.descriptor = descriptor
        self.closed = False

    def fileno(self) -> int:
        return self.descriptor

    def close(self) -> None:
        self.closed = True


class DummyProcess:
    def __init__(self) -> None:
        self.stdin = DummyStream(101)
        self.stdout = DummyStream(102)
        self.returncode: int | None = None
        self.killed = 0
        self.terminated = 0

    def poll(self) -> int | None:
        return self.returncode

    def kill(self) -> None:
        self.killed += 1
        self.returncode = -9

    def terminate(self) -> None:
        self.terminated += 1
        self.returncode = -15

    def wait(self, timeout: float | None = None) -> int:
        return self.returncode if self.returncode is not None else 0


def helper_backend() -> tuple[object, DummyProcess]:
    backend = object.__new__(ax.SwiftHelperBackend)
    process = DummyProcess()
    backend._process = process
    backend._buffer = bytearray()
    backend._request_number = 0
    backend._closed = False
    backend.helper_source = "custom_untrusted"
    backend.source_authenticated = False
    backend.integrity_verified = True
    return backend, process


class MacOSAXCoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.backend = FakeBackend()
        self.driver = ax.MacOSAXDriver(self.backend)

    def snapshot(
        self, *, max_depth: int = 32, max_nodes: int = 1000
    ) -> dict[str, object]:
        return self.driver.execute(
            "snapshot",
            {
                "app": {"bundle_id": "dev.example.Editor"},
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

    def test_list_apps_and_normalized_snapshot(self) -> None:
        listed = self.driver.execute("list_apps", {}, deadline=deadline())
        self.assertEqual(listed["backend"], "fake_macos_ax")
        self.assertTrue(listed["accessibility_trusted"])
        self.assertEqual(listed["apps"][0]["bundle_id"], "dev.example.Editor")

        snapshot = self.snapshot()
        self.assertEqual(snapshot["revision"], 1)
        self.assertTrue(str(snapshot["snapshot_id"]).endswith(":1"))
        self.assertEqual(snapshot["backend"], "fake_macos_ax")
        self.assertEqual(snapshot["app"]["process_id"], 7)
        self.assertFalse(snapshot["truncated"])
        self.assertEqual(
            [item["node_id"] for item in snapshot["nodes"]],
            ["n0", "n1", "n2", "n3"],
        )
        save = snapshot["nodes"][2]
        self.assertEqual(save["parent_id"], "n1")
        self.assertEqual(save["role"], "AXButton")
        self.assertEqual(save["bounds"]["x"], 10)
        self.assertEqual(save["actions"], ["focus", "invoke"])
        self.assertEqual(save["provenance"]["backend"], "fake_macos_ax")

    def test_locator_is_exact_and_never_selects_first_ambiguous_node(self) -> None:
        duplicate = default_tree()
        duplicate.append(
            node(
                "save2",
                1,
                "AXButton",
                "Save",
                identifier="save2",
                actions=("invoke",),
            )
        )
        self.driver = ax.MacOSAXDriver(FakeBackend([duplicate]))
        snapshot = self.snapshot()

        with self.assertRaises(ax.DriverError) as missing:
            self.find(snapshot, {"name": "save"})
        self.assertEqual(missing.exception.code, "DRIVER.NOT_FOUND")

        with self.assertRaises(ax.DriverError) as ambiguous:
            self.find(snapshot, {"role": "AXButton", "name": "Save"})
        self.assertEqual(ambiguous.exception.code, "DRIVER.AMBIGUOUS")
        self.assertEqual(ambiguous.exception.data["candidate_count"], 2)
        self.assertNotIn("candidates", ambiguous.exception.data)

        found = self.find(snapshot, {"identifier": "save2"})
        self.assertEqual(found["target"]["node_id"], "n4")

    def test_invalid_locator_shapes_fail_closed(self) -> None:
        snapshot = self.snapshot()
        invalid = (
            {},
            {"match": "exact"},
            {"match": "contains", "name": "Save"},
            {"states": {}},
            {"actions": []},
            {"states": {"imaginary": True}},
            {"actions": ["click"]},
        )
        for locator in invalid:
            with self.subTest(locator=locator), self.assertRaises(ax.DriverError) as raised:
                self.find(snapshot, locator)
            self.assertEqual(raised.exception.code, "DRIVER.INVALID_REQUEST")

    def test_write_actions_reresolve_with_original_budget_and_native_identity(self) -> None:
        cases = (
            ("focus", {"identifier": "save"}, {}),
            ("invoke", {"identifier": "save"}, {}),
            ("set_value", {"identifier": "title"}, {"value": "Final"}),
        )
        for action, locator, extra in cases:
            with self.subTest(action=action):
                backend = FakeBackend()
                driver = ax.MacOSAXDriver(backend)
                snapshot = driver.execute(
                    "snapshot",
                    {
                        "app": {"process_id": 7},
                        "max_depth": 64,
                        "max_nodes": 2000,
                    },
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
                captures = [call for call in backend.calls if call[0] == "capture"]
                self.assertEqual(captures[-1][2:], (64, 2000))
                self.assertTrue(any(call[0] == "same_element" for call in backend.calls))
                with self.assertRaises(ax.DriverError) as stale_after_write:
                    driver.execute(
                        "find",
                        {
                            "snapshot_id": result["resolved"]["snapshot_id"],
                            "revision": result["resolved"]["revision"],
                            "locator": locator,
                        },
                        deadline=deadline(),
                    )
                self.assertEqual(stale_after_write.exception.code, "DRIVER.STALE_SNAPSHOT")

    def test_replaced_native_target_is_stale_without_dispatch(self) -> None:
        replacement = default_tree()
        replacement[2] = node(
            "replacement",
            1,
            "AXButton",
            "Save",
            identifier="save",
            actions=("focus", "invoke"),
        )
        backend = FakeBackend([default_tree(), replacement])
        driver = ax.MacOSAXDriver(backend)
        snapshot = driver.execute(
            "snapshot", {"app": {"process_id": 7}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"identifier": "save"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(ax.DriverError) as stale:
            driver.execute(
                "invoke",
                {"target": found["target"], "locator": {"identifier": "save"}},
                deadline=deadline(),
            )
        self.assertEqual(stale.exception.code, "DRIVER.STALE_SNAPSHOT")
        self.assertFalse(any(call[0] == "invoke" for call in backend.calls))

    def test_truncated_and_protected_nodes_fail_closed(self) -> None:
        self.backend.truncated = True
        snapshot = self.snapshot()
        with self.assertRaises(ax.DriverError) as truncated:
            self.find(snapshot, {"identifier": "save"})
        self.assertEqual(truncated.exception.code, "DRIVER.SNAPSHOT_TRUNCATED")

        tree = default_tree()
        tree.append(
            node(
                "password",
                1,
                "AXTextField",
                "Password",
                identifier="password",
                value="secret",
                actions=("focus", "set_value"),
                protected=True,
            )
        )
        backend = FakeBackend([tree])
        driver = ax.MacOSAXDriver(backend)
        snapshot = driver.execute(
            "snapshot", {"app": {"process_id": 7}}, deadline=deadline()
        )
        protected_node = snapshot["nodes"][-1]
        self.assertIsNone(protected_node["value"])
        self.assertTrue(protected_node["provenance"]["value_redacted"])
        self.assertNotIn("set_value", protected_node["actions"])
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"identifier": "password"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(ax.DriverError) as protected:
            driver.execute(
                "set_value",
                {
                    "target": found["target"],
                    "locator": {"identifier": "password"},
                    "value": "new",
                },
                deadline=deadline(),
            )
        self.assertEqual(protected.exception.code, "DRIVER.PROTECTED_ELEMENT")
        self.assertFalse(any(call[0] == "set_value" for call in backend.calls))

    def test_post_dispatch_failure_is_unknown_effect_and_deadline_is_retryable(self) -> None:
        backend = FakeBackend()
        backend.fail = "invoke"
        driver = ax.MacOSAXDriver(backend)
        snapshot = driver.execute(
            "snapshot", {"app": {"process_id": 7}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"identifier": "save"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(ax.DriverError) as failed:
            driver.execute(
                "invoke",
                {"target": found["target"], "locator": {"identifier": "save"}},
                deadline=deadline(),
            )
        self.assertEqual(failed.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertEqual(failed.exception.data["effect"], "unknown")
        self.assertFalse(failed.exception.retryable)

        with self.assertRaises(ax.DriverError) as expired:
            self.driver.execute("list_apps", {}, deadline=time.monotonic() - 1)
        self.assertEqual(expired.exception.code, "DRIVER.TIMEOUT")
        self.assertTrue(expired.exception.retryable)

    def test_all_post_write_helper_channel_faults_are_unknown_and_invalidate_session(self) -> None:
        for code in (
            "DRIVER.UNAVAILABLE",
            "DRIVER.ACTION_FAILED",
            "DRIVER.TIMEOUT",
            "DRIVER.OUTPUT_TOO_LARGE",
            "PROTOCOL.PARSE_ERROR",
        ):
            with self.subTest(code=code):
                backend = HelperFaultBackend(code, request_dispatched=True)
                driver = ax.MacOSAXDriver(backend)
                snapshot = driver.execute(
                    "snapshot", {"app": {"process_id": 7}}, deadline=deadline()
                )
                found = driver.execute(
                    "find",
                    {
                        "snapshot_id": snapshot["snapshot_id"],
                        "revision": snapshot["revision"],
                        "locator": {"identifier": "save"},
                    },
                    deadline=deadline(),
                )
                with self.assertRaises(ax.DriverError) as failed:
                    driver.execute(
                        "invoke",
                        {
                            "target": found["target"],
                            "locator": {"identifier": "save"},
                        },
                        deadline=deadline(),
                    )
                self.assertEqual(failed.exception.code, "DRIVER.UNKNOWN_EFFECT")
                self.assertEqual(failed.exception.data["effect"], "unknown")
                self.assertIsNone(driver._current)

    def test_pre_write_helper_pipe_failure_is_not_applied(self) -> None:
        backend = HelperFaultBackend("DRIVER.UNAVAILABLE", request_dispatched=False)
        driver = ax.MacOSAXDriver(backend)
        snapshot = driver.execute(
            "snapshot", {"app": {"process_id": 7}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"identifier": "save"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(ax.DriverError) as failed:
            driver.execute(
                "invoke",
                {"target": found["target"], "locator": {"identifier": "save"}},
                deadline=deadline(),
            )
        self.assertEqual(failed.exception.code, "DRIVER.UNAVAILABLE")
        self.assertEqual(failed.exception.data["effect"], "not_applied")
        self.assertIsNone(driver._current)

    def test_identity_channel_failure_invalidates_snapshot_without_write(self) -> None:
        backend = IdentityChannelFaultBackend()
        driver = ax.MacOSAXDriver(backend)
        snapshot = driver.execute(
            "snapshot", {"app": {"process_id": 7}}, deadline=deadline()
        )
        found = driver.execute(
            "find",
            {
                "snapshot_id": snapshot["snapshot_id"],
                "revision": snapshot["revision"],
                "locator": {"identifier": "save"},
            },
            deadline=deadline(),
        )
        with self.assertRaises(ax.DriverError) as failed:
            driver.execute(
                "invoke",
                {"target": found["target"], "locator": {"identifier": "save"}},
                deadline=deadline(),
            )
        self.assertEqual(failed.exception.code, "DRIVER.UNAVAILABLE")
        self.assertIsNone(driver._current)
        self.assertFalse(any(call[0] == "invoke" for call in backend.calls))


class MacOSAXProcessTests(unittest.TestCase):
    def make_plugin(self) -> ProcessPlugin:
        plugin = ProcessPlugin(
            [sys.executable, str(DRIVER_PATH)], timeout=3, name="desktop.macos_ax"
        )
        self.addCleanup(plugin.close)
        return plugin

    def test_manifest_passes_host_schema_and_has_canonical_actions(self) -> None:
        manifest = self.make_plugin().start()
        self.assertEqual(manifest["metadata"]["name"], "desktop.macos_ax")
        self.assertEqual(manifest["runtime"]["platforms"], ["macos"])
        self.assertEqual(manifest["runtime"]["entrypoint"], "./run.sh")
        self.assertEqual(
            set(manifest["actions"]),
            {"list_apps", "snapshot", "find", "focus", "invoke", "set_value"},
        )
        for name in ("list_apps", "snapshot", "find"):
            self.assertEqual(manifest["actions"][name]["permissions"], ["desktop.observe"])
        for name in ("focus", "invoke", "set_value"):
            self.assertEqual(
                manifest["actions"][name]["permissions"],
                ["desktop.observe", "desktop.input"],
            )
        self.assertEqual(manifest["actions"]["invoke"]["effect"]["default_class"], "non_idempotent")
        for name, contract in manifest["actions"].items():
            self.assertEqual(contract["contract_major"], 1, name)
            self.assertIn(f"desktop.macos_ax.{name}@1", ax.ACTION_NAMES)

    @unittest.skipIf(sys.platform == "darwin", "non-macOS unavailable smoke")
    def test_non_macos_runtime_is_structured_unavailable(self) -> None:
        plugin = self.make_plugin()
        with self.assertRaises(PluginError) as raised:
            plugin.invoke("desktop.macos_ax.list_apps@1", {})
        self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
        self.assertEqual(raised.exception.details["reason"], "platform")
        self.assertEqual(raised.exception.details["required_platform"], "darwin")

    def test_missing_helper_is_explicit_and_never_falls_back(self) -> None:
        with tempfile.TemporaryDirectory() as temporary, mock.patch.object(
            ax.sys, "platform", "darwin"
        ):
            missing = Path(temporary) / "MacOSAXHelper.app" / "Contents" / "MacOS" / "MacOSAXHelper"
            with self.assertRaises(ax.DriverError) as raised:
                ax.SwiftHelperBackend(missing)
        self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
        self.assertEqual(raised.exception.data["reason"], "helper_missing")
        self.assertEqual(raised.exception.data["helper_source"], "custom_untrusted")
        self.assertFalse(raised.exception.data["source_authenticated"])

    def test_helper_bundle_identity_is_checked_separately_from_signature(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            app = Path(temporary) / ax.HELPER_BUNDLE_NAME
            contents = app / "Contents"
            contents.mkdir(parents=True)
            info = {
                "CFBundleIdentifier": ax.HELPER_BUNDLE_ID,
                "CFBundleExecutable": ax.HELPER_EXECUTABLE_NAME,
                "CFBundlePackageType": "APPL",
            }
            with (contents / "Info.plist").open("wb") as stream:
                plistlib.dump(info, stream)
            ax._validate_helper_bundle(app)
            info["CFBundleIdentifier"] = "dev.example.untrusted"
            with (contents / "Info.plist").open("wb") as stream:
                plistlib.dump(info, stream)
            with self.assertRaises(ax.DriverError) as mismatch:
                ax._validate_helper_bundle(app)
        self.assertEqual(mismatch.exception.code, "DRIVER.UNAVAILABLE")
        self.assertEqual(
            mismatch.exception.data["reason"], "helper_bundle_identity_mismatch"
        )

    def test_custom_helper_is_explicitly_not_source_authenticated(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            app = Path(temporary) / ax.HELPER_BUNDLE_NAME
            contents = app / "Contents"
            executable = contents / "MacOS" / ax.HELPER_EXECUTABLE_NAME
            executable.parent.mkdir(parents=True)
            executable.touch(mode=0o755)
            with (contents / "Info.plist").open("wb") as stream:
                plistlib.dump(
                    {
                        "CFBundleIdentifier": ax.HELPER_BUNDLE_ID,
                        "CFBundleExecutable": ax.HELPER_EXECUTABLE_NAME,
                        "CFBundlePackageType": "APPL",
                    },
                    stream,
                )
            process = DummyProcess()
            status = {
                "protocol_version": ax.HELPER_PROTOCOL_VERSION,
                "implementation": "native_accessibility_api",
            }
            with mock.patch.object(ax.sys, "platform", "darwin"), mock.patch.object(
                ax.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0, b"", b""),
            ), mock.patch.object(
                ax.subprocess, "Popen", return_value=process
            ), mock.patch.object(
                ax.SwiftHelperBackend, "_rpc", return_value=status
            ):
                backend = ax.SwiftHelperBackend(executable)
            self.addCleanup(backend.close)
            self.assertEqual(
                backend.security_info(),
                {
                    "source": "custom_untrusted",
                    "integrity_verified": True,
                    "source_authenticated": False,
                },
            )

    def test_helper_pipe_failure_before_complete_frame_is_not_applied_and_closes(self) -> None:
        backend, process = helper_backend()
        writes = 0

        def partial_then_fail(_descriptor: int, data: bytes) -> int:
            nonlocal writes
            writes += 1
            if writes == 1:
                return min(3, len(data))
            raise BrokenPipeError()

        with mock.patch.object(ax.os, "write", side_effect=partial_then_fail):
            with self.assertRaises(ax.DriverError) as raised:
                backend._rpc("invoke", {"native_token": "n"}, deadline=deadline())
        self.assertEqual(raised.exception.code, "DRIVER.UNAVAILABLE")
        self.assertFalse(raised.exception.data["helper_request_dispatched"])
        self.assertEqual(raised.exception.data["effect"], "not_applied")
        self.assertEqual(raised.exception.data["bytes_written"], 3)
        self.assertTrue(backend._closed)
        self.assertEqual(process.killed, 1)
        with self.assertRaises(ax.DriverError) as reused:
            backend._rpc("status", {}, deadline=deadline())
        self.assertEqual(reused.exception.data["reason"], "helper_closed")

    def test_complete_write_then_protocol_error_is_unknown_and_kills_helper(self) -> None:
        backend, process = helper_backend()
        with mock.patch.object(ax.os, "write", side_effect=lambda _fd, data: len(data)), mock.patch.object(
            backend, "_readline", return_value=b"not-json"
        ):
            with self.assertRaises(ax.DriverError) as raised:
                backend._write_rpc("invoke", {"native_token": "n"}, deadline=deadline())
        self.assertEqual(raised.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertTrue(raised.exception.data["helper_request_dispatched"])
        self.assertEqual(raised.exception.data["effect"], "unknown")
        self.assertTrue(backend._closed)
        self.assertEqual(process.killed, 1)

    def test_helper_timeout_eof_and_output_limit_are_fatal(self) -> None:
        scenarios = (
            ("timeout", "DRIVER.TIMEOUT"),
            ("eof", "DRIVER.UNAVAILABLE"),
            ("oversized", "DRIVER.OUTPUT_TOO_LARGE"),
        )
        for scenario, expected_code in scenarios:
            with self.subTest(scenario=scenario):
                backend, process = helper_backend()
                if scenario == "timeout":
                    with self.assertRaises(ax.DriverError) as raised:
                        backend._readline(time.monotonic() - 1)
                elif scenario == "eof":
                    with mock.patch.object(
                        ax.select, "select", return_value=([102], [], [])
                    ), mock.patch.object(ax.os, "read", return_value=b""):
                        with self.assertRaises(ax.DriverError) as raised:
                            backend._readline(deadline())
                else:
                    backend._buffer.extend(b"x" * 33 + b"\n")
                    with mock.patch.object(ax, "MAX_HELPER_RESPONSE_BYTES", 32):
                        with self.assertRaises(ax.DriverError) as raised:
                            backend._readline(deadline())
                self.assertEqual(raised.exception.code, expected_code)
                self.assertTrue(raised.exception.data["helper_request_dispatched"])
                self.assertTrue(backend._closed)
                self.assertEqual(process.killed, 1)
                with self.assertRaises(ax.DriverError) as reused:
                    backend._rpc("status", {}, deadline=deadline())
                self.assertEqual(reused.exception.data["reason"], "helper_closed")

    def test_readline_checks_limit_after_read_before_returning_newline(self) -> None:
        backend, process = helper_backend()
        with mock.patch.object(ax, "MAX_HELPER_RESPONSE_BYTES", 32), mock.patch.object(
            ax.select, "select", return_value=([102], [], [])
        ), mock.patch.object(ax.os, "read", return_value=b"x" * 33 + b"\n"):
            with self.assertRaises(ax.DriverError) as raised:
                backend._readline(deadline())
        self.assertEqual(raised.exception.code, "DRIVER.OUTPUT_TOO_LARGE")
        self.assertEqual(process.killed, 1)

    def test_invalid_helper_write_result_is_unknown_and_kills_helper(self) -> None:
        backend, process = helper_backend()
        with mock.patch.object(backend, "_rpc", return_value={"accepted": False}):
            with self.assertRaises(ax.DriverError) as raised:
                backend._write_rpc("invoke", {"native_token": "n"}, deadline=deadline())
        self.assertEqual(raised.exception.code, "DRIVER.UNKNOWN_EFFECT")
        self.assertTrue(raised.exception.data["helper_request_dispatched"])
        self.assertEqual(raised.exception.data["effect"], "unknown")
        self.assertTrue(backend._closed)
        self.assertEqual(process.killed, 1)

    def test_python_filters_executable_and_rejects_helper_leak(self) -> None:
        backend = FakeBackend()
        original = backend.list_apps

        def leaked(*, deadline: float) -> dict[str, object]:
            result = original(deadline=deadline)
            result["apps"][0]["executable"] = "/Users/private/Editor.app"
            return result

        backend.list_apps = leaked  # type: ignore[method-assign]
        listed = ax.MacOSAXDriver(backend).execute("list_apps", {}, deadline=deadline())
        self.assertNotIn("executable", listed["apps"][0])

        native, process = helper_backend()
        with mock.patch.object(
            native,
            "_rpc",
            return_value={
                "accessibility_trusted": True,
                "apps": [{"process_id": 7, "executable": "/Users/private/Editor"}],
            },
        ):
            with self.assertRaises(ax.DriverError) as raised:
                native.list_apps(deadline=deadline())
        self.assertEqual(raised.exception.code, "DRIVER.ACTION_FAILED")
        self.assertTrue(native._closed)
        self.assertEqual(process.killed, 1)

    def test_ndjson_reader_rejects_bad_frames_then_continues(self) -> None:
        oversized = b"{" + b"x" * ax.MAX_REQUEST_BYTES + b"}\n"
        invalid_utf8 = b"\xff\n"
        invalid_json = b"{not-json}\n"
        valid = json.dumps({"type": "manifest", "id": "after"}).encode() + b"\n"
        output = io.BytesIO()
        original_stdout = sys.stdout

        class BinaryStdout:
            buffer = output

        try:
            sys.stdout = BinaryStdout()
            ax.serve(
                ax.MacOSAXDriver(FakeBackend()),
                io.BytesIO(oversized + invalid_utf8 + invalid_json + valid),
            )
        finally:
            sys.stdout = original_stdout
        messages = [json.loads(line) for line in output.getvalue().decode().splitlines()]
        self.assertEqual(messages[0]["error"]["code"], "PROTOCOL.REQUEST_TOO_LARGE")
        self.assertEqual(messages[1]["error"]["code"], "PROTOCOL.INVALID_ENCODING")
        self.assertEqual(messages[2]["error"]["code"], "PROTOCOL.PARSE_ERROR")
        self.assertEqual(messages[3]["id"], "after")
        self.assertEqual(messages[3]["result"]["metadata"]["name"], "desktop.macos_ax")


class MacOSAXSourceContracts(unittest.TestCase):
    def test_shell_scripts_are_executable_and_valid_posix_shell(self) -> None:
        shell = shutil.which("sh")
        if shell is None:
            self.skipTest("POSIX shell unavailable")
        for script in (RUN_SCRIPT, BUILD_SCRIPT):
            with self.subTest(script=script.name):
                self.assertTrue(os.access(script, os.X_OK))
                completed = subprocess.run(
                    [shell, "-n", str(script)], capture_output=True, text=True, check=False
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_native_source_is_real_ax_scoped_and_preflights_mutations(self) -> None:
        source = HELPER_SOURCE.read_text(encoding="utf-8")
        for marker in (
            "AXUIElementCreateApplication",
            "AXUIElementGetAttributeValueCount",
            "AXUIElementCopyAttributeValues",
            "AXUIElementIsAttributeSettable",
            "AXUIElementCopyActionNames",
            "AXUIElementSetAttributeValue",
            "AXUIElementPerformAction",
            "AXUIElementSetMessagingTimeout",
            "CFEqual(previous, current)",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, source)
        for forbidden in (
            "AXUIElementCreateSystemWide",
            "CGWindowListCreateImage",
            "CGRequestScreenCaptureAccess",
            "osascript",
            "readLine(",
            "executableURL",
        ):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, source)
        self.assertLess(source.index("isSettable(element, kAXFocusedAttribute"), source.index("AXUIElementSetAttributeValue(\n            element, kAXFocusedAttribute"))
        self.assertLess(source.index("copyActionNames(element).contains(kAXPressAction"), source.index("AXUIElementPerformAction(element"))
        for marker in (
            "input.readData(ofLength: 65_536)",
            "buffer.count > maximumInputBytes",
            "discardingOversizedFrame",
            "data.count + 1 > maximumOutputBytes",
            '"DRIVER.OUTPUT_TOO_LARGE"',
            '"candidate_count": matches.count',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, source)
        self.assertNotIn('"candidates": Array(matches', source)

    def test_build_requires_macos_and_signs_app_bundle(self) -> None:
        source = BUILD_SCRIPT.read_text(encoding="utf-8")
        for marker in (
            "xcrun --sdk macosx --find swiftc",
            '"$swiftc_path"',
            '-sdk "$sdk_path"',
            "MacOSAXHelper.app",
            "dev.ai-auto-desktop.macos-ax-helper",
            "codesign --force --sign",
            "codesign --verify --strict",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, source)

    @unittest.skipIf(platform.system() == "Darwin", "checks unsupported build host")
    def test_build_script_refuses_non_macos_without_creating_default_build(self) -> None:
        before = (PLUGIN_ROOT / ".build").exists()
        completed = subprocess.run(
            [str(BUILD_SCRIPT)], capture_output=True, text=True, check=False, timeout=10
        )
        self.assertEqual(completed.returncode, 3)
        self.assertIn("macOS", completed.stderr)
        self.assertEqual((PLUGIN_ROOT / ".build").exists(), before)


if __name__ == "__main__":
    unittest.main()
