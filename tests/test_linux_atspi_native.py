"""KDE/X11 会话中的真实 AT-SPI 原生集成测试。

测试辅助可以从当前用户的 ``kwin_x11`` 进程恢复图形会话环境；生产驱动不得
扫描 ``/proc`` 或猜测其他会话。缺少 KDE/X11、Gio、AT-SPI bus、System Settings
或对应 toolkit bridge 时，本测试保守跳过。自有 GTK3 fixture 还会验证
PyGObject 后端的 snapshot/find/focus/set_text/invoke/toggle/expand/collapse 完整链路。
"""

from __future__ import annotations

from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.plugin import ProcessPlugin
from ai_auto_desktop.runtime import run_descriptor


PROJECT_ROOT = Path(__file__).resolve().parents[1]
DRIVER_PATH = PROJECT_ROOT / "plugins" / "linux_atspi" / "linux_atspi_driver.py"
FIXTURE_PATH = PROJECT_ROOT / "tests" / "linux" / "atspi_fixture_app.py"
QT_FIXTURE_SOURCE = PROJECT_ROOT / "tests" / "linux" / "qt_atspi_fixture.cpp"
QT_FIXTURE_RUNNER = PROJECT_ROOT / "tests" / "linux" / "qt_atspi_native_runner.py"
QML_FIXTURE_SOURCE = PROJECT_ROOT / "tests" / "linux" / "qml_atspi_fixture.qml"
QML_FIXTURE_RUNNER = PROJECT_ROOT / "tests" / "linux" / "qml_atspi_native_runner.py"
GTK_FIXTURE_RUNNER = PROJECT_ROOT / "tests" / "linux" / "gtk_atspi_native_runner.py"
XTEST_BUILD_SCRIPT = PROJECT_ROOT / "plugins" / "linux_atspi" / "build_x11_xtest_helper.sh"
TEST_TYPELIB_ENV = "AI_AUTO_DESKTOP_TEST_ATSPI_TYPELIB_PATH"
FIXTURE_ENTRY_NAME = "Fixture text entry"
FIXTURE_BUTTON_NAME = "Invoke fixture button"
FIXTURE_STATUS_INVOKED = "Fixture status invoked"
FIXTURE_CHECK_NAME = "Toggle fixture check button"
FIXTURE_EXPANDER_NAME = "Expand fixture details"
QT_FIXTURE_ENTRY_NAME = "Qt fixture text entry"
QT_FIXTURE_BUTTON_NAME = "Invoke Qt fixture button"
QT_FIXTURE_STATUS_INVOKED = "Qt fixture status invoked"

SPEC = importlib.util.spec_from_file_location(
    "testable_linux_atspi_native_driver", DRIVER_PATH
)
assert SPEC is not None and SPEC.loader is not None
atspi = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = atspi
SPEC.loader.exec_module(atspi)


SESSION_KEYS = (
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
)


def _read_process_environment(pid: int) -> dict[str, str]:
    """读取一个测试候选进程的有界环境白名单。"""

    raw = (Path("/proc") / str(pid) / "environ").read_bytes()
    if len(raw) > 1024 * 1024:
        raise ValueError("进程环境超过测试辅助上限")
    allowed = set(SESSION_KEYS)
    result: dict[str, str] = {}
    for field in raw.split(b"\0"):
        if b"=" not in field:
            continue
        key_raw, value_raw = field.split(b"=", 1)
        key = key_raw.decode("utf-8", errors="strict")
        if key in allowed:
            result[key] = value_raw.decode("utf-8", errors="strict")
    return result


def _kde_x11_test_environment() -> dict[str, str] | None:
    """仅供测试：从当前用户的 ``kwin_x11`` 恢复会话变量。"""

    proc = Path("/proc")
    if not proc.is_dir():
        return None
    for entry in sorted(proc.iterdir(), key=lambda item: item.name):
        if not entry.name.isdigit():
            continue
        try:
            if (entry / "comm").read_text(encoding="utf-8").strip() != "kwin_x11":
                continue
            if entry.stat().st_uid != os.getuid():
                continue
            values = _read_process_environment(int(entry.name))
        except (FileNotFoundError, PermissionError, ProcessLookupError, UnicodeError, ValueError):
            continue
        if (
            values.get("XDG_SESSION_TYPE", "").lower() == "x11"
            and "KDE"
            in {
                item.upper()
                for item in re.split(
                    r"[:;]", values.get("XDG_CURRENT_DESKTOP", "")
                )
                if item
            }
            and values.get("DISPLAY")
            and values.get("DBUS_SESSION_BUS_ADDRESS")
        ):
            return values
    return None


def _native_subprocess_environment(session: dict[str, str]) -> dict[str, str]:
    """Build a test-only GTK/typelib environment around the real session."""

    environment = os.environ.copy()
    environment.update(session)
    environment["GDK_BACKEND"] = "x11"
    # The recovered session currently advertises org.a11y.Status.IsEnabled=false.
    # GTK_A11Y forces only this owned fixture to load its standard ATK bridge.
    environment["GTK_A11Y"] = "always"
    environment.pop("NO_AT_BRIDGE", None)

    raw_typelib_path = environment.get(TEST_TYPELIB_ENV)
    typelib_path = Path(raw_typelib_path) if raw_typelib_path else None
    if (
        typelib_path is not None
        and (typelib_path / "Atspi-2.0.typelib").is_file()
    ):
        existing = environment.get("GI_TYPELIB_PATH")
        paths = [str(typelib_path)]
        if existing:
            paths.extend(
                item
                for item in existing.split(os.pathsep)
                if item and item != str(typelib_path)
            )
        environment["GI_TYPELIB_PATH"] = os.pathsep.join(paths)
    return environment


@contextmanager
def _patched_environment(values: dict[str, str]):
    previous = {key: os.environ.get(key) for key in SESSION_KEYS}
    try:
        for key in SESSION_KEYS:
            if key in values:
                os.environ[key] = values[key]
            else:
                os.environ.pop(key, None)
        yield
    finally:
        for key, value in previous.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


@unittest.skipUnless(sys.platform.startswith("linux"), "仅在 Linux 运行")
class NativeKdeX11AtspiInfrastructureTests(unittest.TestCase):
    """区分 AT-SPI 基础设施可读性与 KDE/Qt 应用资格验证。"""

    @classmethod
    def setUpClass(cls) -> None:
        cls.session = _kde_x11_test_environment()
        if cls.session is None:
            raise unittest.SkipTest("未发现当前用户可访问的 KDE/X11 kwin_x11 会话")

    def test_registry_infrastructure_reads_one_bounded_snapshot(self) -> None:
        with _patched_environment(self.session):
            try:
                backend = atspi.GioAtspiBackend()
            except atspi.DriverError as exc:
                self.skipTest(f"Gio/AT-SPI 不可用：{exc.code}")
            applications = list(backend.list_applications(deadline=time.monotonic() + 10))
            if not applications:
                self.skipTest("AT-SPI registry 当前没有应用")
            self.assertEqual(backend.name, "gio_atspi")
            self.assertEqual(backend.session_info()["session_type"], "x11")
            self.assertEqual(backend.session_info()["desktop"], "KDE")
            application = next(
                (item for item in applications if item.get("name") == "Microsoft Edge"),
                applications[0],
            )
            snapshot = backend.capture(
                {"bus_name": application["bus_name"]},
                max_depth=2,
                max_nodes=64,
                deadline=time.monotonic() + 20,
            )
            self.assertEqual(snapshot.application["bus_name"], application["bus_name"])
            self.assertGreaterEqual(len(snapshot.nodes), 1)
            self.assertLessEqual(len(snapshot.nodes), 64)
            self.assertEqual(snapshot.nodes[0].role, "application")
            self.assertTrue(snapshot.nodes[0].provenance["object_path"].startswith("/"))
            # 这里只证明当前 KDE/X11 会话的 AT-SPI 基础设施可读；被选中的
            # 应用可能是 GTK/Chromium，不能据此宣称 Qt bridge 已通过资格验证。

    def test_process_boundary_reports_real_session_and_backend(self) -> None:
        environment = os.environ.copy()
        environment.update(self.session)
        plugin = ProcessPlugin(
            [sys.executable, str(DRIVER_PATH)],
            env=environment,
            timeout=30,
            name="desktop.linux_atspi KDE/X11 smoke",
        )
        self.addCleanup(plugin.close)
        manifest = plugin.start()
        self.assertEqual(manifest["metadata"]["name"], "desktop.linux_atspi")
        try:
            applications_result = plugin.invoke(
                "desktop.linux_atspi.list_applications@1", {}
            )
        except Exception as exc:
            self.skipTest(f"当前桌面 AT-SPI bus 不可用：{exc}")
        self.assertEqual(applications_result["session"]["session_type"], "x11")
        self.assertEqual(applications_result["session"]["desktop"], "KDE")
        self.assertIn(
            applications_result["backend"], {"pygobject_atspi", "gio_atspi"}
        )
        if not applications_result["applications"]:
            self.skipTest("AT-SPI registry 当前没有应用")
        application = next(
            (
                item
                for item in applications_result["applications"]
                if item.get("name") == "Microsoft Edge"
            ),
            applications_result["applications"][0],
        )
        snapshot = plugin.invoke(
            "desktop.linux_atspi.snapshot@1",
            {
                "application": {"bus_name": application["bus_name"]},
                "max_depth": 2,
                "max_nodes": 64,
            },
        )
        self.assertEqual(snapshot["backend"], applications_result["backend"])
        self.assertGreaterEqual(len(snapshot["nodes"]), 1)
        self.assertLessEqual(len(snapshot["nodes"]), 64)

    def test_default_backend_is_gio_when_atspi_typelib_is_missing(self) -> None:
        with _patched_environment(self.session):
            backend = atspi.create_default_backend()
        if isinstance(backend, atspi.PyGObjectAtspiBackend):
            self.skipTest("当前主机已安装 Atspi typelib，无需 Gio fallback")
        if isinstance(backend, atspi.UnavailableBackend):
            self.skipTest(
                f"当前桌面 AT-SPI bus 不可用：{backend.details!r}"
            )
        self.assertIsInstance(backend, atspi.GioAtspiBackend)
        self.assertEqual(backend.name, "gio_atspi")

    def test_owned_gtk_fixture_supports_real_semantic_actions(self) -> None:
        if not FIXTURE_PATH.is_file():
            self.fail(f"GTK fixture 不存在：{FIXTURE_PATH}")
        environment = _native_subprocess_environment(self.session)
        dependency_probe = subprocess.run(
            [
                sys.executable,
                "-c",
                (
                    "import gi; "
                    "gi.require_version('Gtk', '3.0'); "
                    "gi.require_version('Atspi', '2.0'); "
                    "from gi.repository import Atspi, Gtk"
                ),
            ],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=5,
        )
        if dependency_probe.returncode != 0:
            diagnostic = dependency_probe.stderr.decode(
                "utf-8", errors="replace"
            )[-1000:]
            self.skipTest(
                "完整原生动作需要 Gtk 3 与 Atspi 2.0 typelib："
                + diagnostic
            )
        fixture = subprocess.Popen(
            [sys.executable, str(FIXTURE_PATH)],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.addCleanup(self._stop_process, fixture)

        plugin = ProcessPlugin(
            [sys.executable, str(DRIVER_PATH)],
            env=environment,
            timeout=30,
            name="desktop.linux_atspi owned GTK fixture",
        )
        self.addCleanup(plugin.close)
        plugin.start()

        applications_result: dict[str, object] | None = None
        application: dict[str, object] | None = None
        stop = time.monotonic() + 10
        while time.monotonic() < stop:
            if fixture.poll() is not None:
                stderr = b"" if fixture.stderr is None else fixture.stderr.read()
                self.fail(
                    "GTK fixture 在注册 AT-SPI 前退出："
                    + stderr.decode("utf-8", errors="replace")[-2000:]
                )
            try:
                applications_result = plugin.invoke(
                    "desktop.linux_atspi.list_applications@1", {}
                )
            except Exception as exc:
                self.skipTest(f"当前桌面 AT-SPI bus 不可用：{exc}")
            if applications_result["backend"] != "pygobject_atspi":
                self.skipTest(
                    "完整原生动作需要 PyGObject Atspi backend，实际为 "
                    f"{applications_result['backend']}"
                )
            application = next(
                (
                    item
                    for item in applications_result["applications"]
                    if item.get("process_id") == fixture.pid
                ),
                None,
            )
            if application is not None:
                break
            time.sleep(0.2)
        if application is None:
            self.fail("自有 GTK fixture 在 10 秒内未注册到 AT-SPI registry")

        application_selector = {"process_id": fixture.pid}

        def snapshot() -> dict[str, object]:
            result = plugin.invoke(
                "desktop.linux_atspi.snapshot@1",
                {
                    "application": application_selector,
                    "max_depth": 8,
                    "max_nodes": 64,
                },
            )
            self.assertEqual(result["backend"], "pygobject_atspi")
            self.assertFalse(result["truncated"])
            return result

        def find(
            captured: dict[str, object], locator: dict[str, object]
        ) -> dict[str, object]:
            return plugin.invoke(
                "desktop.linux_atspi.find@1",
                {
                    "snapshot_id": captured["snapshot_id"],
                    "revision": captured["revision"],
                    "locator": locator,
                },
            )

        def write(
            action: str, locator: dict[str, object], **extra: object
        ) -> dict[str, object]:
            captured = snapshot()
            located = find(captured, locator)
            return plugin.invoke(
                f"desktop.linux_atspi.{action}@1",
                {"target": located["target"], "locator": locator, **extra},
            )

        initial = snapshot()
        self.assertEqual(initial["application"]["process_id"], fixture.pid)
        entry_locator = {
            "role": "text",
            "name": FIXTURE_ENTRY_NAME,
            "actions": ["focus", "set_text"],
        }
        button_locator = {
            "role": "push_button",
            "name": FIXTURE_BUTTON_NAME,
            "actions": ["invoke"],
        }
        check_locator = {
            "role": "check_box",
            "name": FIXTURE_CHECK_NAME,
            "actions": ["toggle"],
        }
        expander_locator = {
            "role": "toggle_button",
            "name": FIXTURE_EXPANDER_NAME,
            "actions": ["expand", "collapse"],
        }
        entry = find(initial, entry_locator)
        self.assertEqual(entry["node"]["value"], "Fixture initial text")
        self.assertEqual(
            entry["node"]["provenance"]["accessible_id"], "fixture-entry"
        )
        focused = write("focus", entry_locator)
        self.assertEqual(focused["action"], "focus")
        self.assertTrue(focused["backend_result"]["accepted"])
        self.assertEqual(
            focused["backend_result"]["native_interface"],
            "Component.grab_focus",
        )
        changed_text = "Fixture changed through AT-SPI"
        before_change = snapshot()
        entry = find(before_change, entry_locator)
        entry_indexes = [
            index
            for index, node in enumerate(before_change["nodes"])
            if node.get("node_id") == entry["node"]["node_id"]
        ]
        self.assertEqual(len(entry_indexes), 1)
        entry_index = entry_indexes[0]
        descriptor = compile_descriptor(
            {
                "apiVersion": "ai-auto-desktop.dev/v1alpha1",
                "kind": "Workflow",
                "metadata": {"name": "native-linux-atspi-observation"},
                "requires": {
                    "platforms": ["linux"],
                    "permissions": ["desktop.observe", "desktop.input"],
                },
                "budgets": {
                    "max_duration": "20s",
                    "max_executed_steps": 1,
                    "cleanup_timeout": "1s",
                },
                "steps": [
                    {
                        "id": "set_fixture_text",
                        "type": "action",
                        "uses": "desktop.linux_atspi.set_text@1",
                        "with": {
                            "target": entry["target"],
                            "locator": entry_locator,
                            "text": changed_text,
                        },
                        "effect": {"class": "contextual"},
                        "risk": {"category": "input", "level": "high"},
                        "timeout": "15s",
                        "postcondition": {
                            "observe": {
                                "uses": "desktop.linux_atspi.snapshot@1",
                                "with": {
                                    "application": application_selector,
                                    "max_depth": 8,
                                    "max_nodes": 64,
                                },
                            },
                            "condition": (
                                "${{ observation.truncated == False and "
                                f"observation.nodes[{entry_index}].node_id == "
                                f"'{entry['node']['node_id']}' and "
                                f"observation.nodes[{entry_index}].value == "
                                f"'{changed_text}'"
                                " }}"
                            ),
                            "timeout": "5s",
                            "poll_interval": "100ms",
                        },
                    }
                ],
            }
        )
        changed_run = run_descriptor(
            descriptor,
            plugins={"desktop.linux_atspi": plugin},
            granted_permissions=["desktop.observe", "desktop.input"],
        )
        self.assertTrue(changed_run.ok, changed_run.to_dict())
        self.assertEqual(changed_run.steps["set_fixture_text"]["attempts"], 1)
        changed = changed_run.steps["set_fixture_text"]["output"]
        self.assertEqual(changed["action"], "set_text")
        self.assertEqual(
            changed["backend_result"]["native_interface"],
            "EditableText.set_text_contents",
        )
        changed_snapshot = snapshot()
        self.assertEqual(
            find(
                changed_snapshot,
                {
                    "role": "text",
                    "name": FIXTURE_ENTRY_NAME,
                    "value": changed_text,
                },
            )["node"]["value"],
            changed_text,
        )

        invoked = write("invoke", button_locator)
        self.assertEqual(invoked["action"], "invoke")
        self.assertEqual(
            invoked["backend_result"]["native_interface"], "Action.do_action"
        )
        self.assertTrue(invoked["backend_result"]["accepted"])
        stop = time.monotonic() + 3
        while True:
            invoked_snapshot = snapshot()
            try:
                status = find(
                    invoked_snapshot,
                    {
                        "role": "label",
                        "name": FIXTURE_STATUS_INVOKED,
                        "value": FIXTURE_STATUS_INVOKED,
                    },
                )
            except Exception:
                if time.monotonic() >= stop:
                    raise
                time.sleep(0.1)
                continue
            self.assertEqual(status["node"]["value"], FIXTURE_STATUS_INVOKED)
            break

        before_toggle = snapshot()
        check = find(before_toggle, check_locator)
        self.assertFalse(check["node"]["states"]["checked"])
        self.assertEqual(
            check["node"]["provenance"]["native_action_name"], "click"
        )
        toggled = write("toggle", check_locator)
        self.assertEqual(toggled["backend_result"]["native_action_name"], "click")
        self.assertTrue(toggled["backend_result"]["dispatched"])
        toggled_snapshot = snapshot()
        self.assertTrue(
            find(toggled_snapshot, check_locator)["node"]["states"]["checked"]
        )

        before_expand = snapshot()
        expander = find(before_expand, expander_locator)
        self.assertTrue(expander["node"]["states"]["expandable"])
        self.assertFalse(expander["node"]["states"]["expanded"])
        self.assertEqual(
            expander["node"]["provenance"]["native_action_name"], "activate"
        )
        expanded = write("expand", expander_locator)
        self.assertEqual(expanded["backend_result"]["native_action_name"], "activate")
        self.assertTrue(expanded["backend_result"]["dispatched"])
        expanded_snapshot = snapshot()
        self.assertTrue(
            find(expanded_snapshot, expander_locator)["node"]["states"]["expanded"]
        )

        expand_no_op = write("expand", expander_locator)
        self.assertTrue(expand_no_op["backend_result"]["no_op"])
        self.assertFalse(expand_no_op["backend_result"]["dispatched"])
        collapsed = write("collapse", expander_locator)
        self.assertTrue(collapsed["backend_result"]["dispatched"])
        collapsed_snapshot = snapshot()
        self.assertFalse(
            find(collapsed_snapshot, expander_locator)["node"]["states"]["expanded"]
        )

    def _build_xtest_helper(self) -> None:
        if not XTEST_BUILD_SCRIPT.is_file():
            self.fail(f"XTest helper 构建脚本不存在：{XTEST_BUILD_SCRIPT}")
        completed = subprocess.run(
            ["sh", str(XTEST_BUILD_SCRIPT)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        if completed.returncode != 0:
            self.fail(
                "XTest helper 编译失败："
                + completed.stderr.decode("utf-8", errors="replace")[-2000:]
            )

    def test_owned_gtk_fixture_supports_real_xtest_type_text(self) -> None:
        session_id = self.session.get("XDG_SESSION_ID")
        if session_id and shutil.which("loginctl"):
            lock_state = subprocess.run(
                ["loginctl", "show-session", session_id, "-p", "LockedHint", "--value"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                encoding="utf-8",
                check=False,
                timeout=5,
            )
            if lock_state.stdout.strip().lower() == "yes":
                self.skipTest("当前 KDE 会话已锁屏；XTEST 事件会被锁屏器隔离")
        if not FIXTURE_PATH.is_file() or not GTK_FIXTURE_RUNNER.is_file():
            self.fail("GTK fixture 或隔离 runner 不存在")
        dbus_run_session = shutil.which("dbus-run-session")
        if dbus_run_session is None:
            self.skipTest("GTK XTest fixture 需要 dbus-run-session")
        environment = _native_subprocess_environment(self.session)
        environment["PYTHONPATH"] = str(PROJECT_ROOT / "src")
        environment.pop("AT_SPI_BUS_ADDRESS", None)
        dependency_probe = subprocess.run(
            [
                sys.executable,
                "-c",
                (
                    "import gi; "
                    "gi.require_version('Gtk', '3.0'); "
                    "gi.require_version('Atspi', '2.0'); "
                    "from gi.repository import Atspi, Gtk"
                ),
            ],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=5,
        )
        if dependency_probe.returncode != 0:
            self.skipTest("GTK XTest fixture 需要 Gtk 3 与 Atspi 2.0 typelib")
        self._build_xtest_helper()
        completed = subprocess.run(
            [
                dbus_run_session,
                "--",
                sys.executable,
                str(GTK_FIXTURE_RUNNER),
                str(FIXTURE_PATH),
                str(DRIVER_PATH),
                "--type-text",
            ],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=45,
        )
        try:
            report = json.loads(completed.stdout.strip().splitlines()[-1])
        except (IndexError, json.JSONDecodeError) as exc:
            self.fail(f"GTK fixture runner 未输出 JSON：{completed.stdout!r}; {exc}")
        self.assertEqual(completed.returncode, 0, msg=f"{report!r}\n{completed.stderr}")
        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["toolkit"], "gtk")
        self.assertEqual(report["actions"], ["focus", "type_text"])
        self.assertEqual(report["input_injection"], "XTEST")
        self.assertTrue(report["type_text_observed"])

    def test_owned_qt5_fixture_supports_real_semantic_actions(self) -> None:
        compiler = shutil.which("g++")
        pkg_config = shutil.which("pkg-config")
        if compiler is None or pkg_config is None or not QT_FIXTURE_SOURCE.is_file():
            self.skipTest("完整 Qt5 fixture 测试需要 g++、pkg-config 与 fixture 源码")
        flags = subprocess.run(
            [pkg_config, "--cflags", "--libs", "Qt5Widgets"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=10,
        )
        if flags.returncode != 0:
            self.skipTest("完整 Qt5 fixture 测试需要 Qt5Widgets 开发包")
        self._build_xtest_helper()
        temporary = tempfile.TemporaryDirectory(prefix="aad-qt-atspi-")
        self.addCleanup(temporary.cleanup)
        executable = Path(temporary.name) / "qt_atspi_fixture"
        compiled = subprocess.run(
            [
                compiler, "-std=c++17", "-O2", "-fPIC",
                str(QT_FIXTURE_SOURCE), "-o", str(executable),
                *shlex.split(flags.stdout),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        if compiled.returncode != 0:
            self.fail(
                "Qt5 fixture 编译失败："
                + compiled.stderr.decode("utf-8", errors="replace")[-2000:]
            )

        dbus_run_session = shutil.which("dbus-run-session")
        if dbus_run_session is None or not QT_FIXTURE_RUNNER.is_file():
            self.skipTest("Qt5 fixture 测试需要 dbus-run-session 与测试 runner")
        environment = os.environ.copy()
        environment.update(self.session)
        environment["PYTHONPATH"] = str(PROJECT_ROOT / "src")
        session_locked = False
        session_id = self.session.get("XDG_SESSION_ID")
        if session_id and shutil.which("loginctl"):
            lock_state = subprocess.run(
                ["loginctl", "show-session", session_id, "-p", "LockedHint", "--value"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                encoding="utf-8",
                check=False,
                timeout=5,
            )
            session_locked = lock_state.stdout.strip().lower() == "yes"
        runner_command = [
            dbus_run_session,
            "--",
            sys.executable,
            str(QT_FIXTURE_RUNNER),
            str(executable),
            str(DRIVER_PATH),
        ]
        if not session_locked:
            runner_command.append("--type-text")
        completed = subprocess.run(
            runner_command,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=45,
        )
        try:
            report = json.loads(completed.stdout.strip().splitlines()[-1])
        except json.JSONDecodeError as exc:
            self.fail(f"Qt5 fixture runner 未输出 JSON：{completed.stdout!r}; {exc}")
        self.assertEqual(completed.returncode, 0, msg=f"{report!r}\n{completed.stderr}")
        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["toolkit"], "Qt")
        self.assertEqual(
            report["actions"],
            (
                ["focus", "set_text", "type_text", "invoke"]
                if not session_locked
                else ["focus", "set_text", "invoke"]
            ),
        )
        self.assertEqual(
            report["input_injection"], "XTEST" if not session_locked else False
        )
        self.assertEqual(report["type_text_observed"], not session_locked)
        self.assertFalse(report["ocr"])

    def test_owned_qml_fixture_supports_real_semantic_invoke(self) -> None:
        qmlscene = shutil.which("qmlscene") or shutil.which("qmlscene-qt5")
        dbus_run_session = shutil.which("dbus-run-session")
        if not QML_FIXTURE_SOURCE.is_file():
            self.fail(f"仓库 QML fixture 不存在：{QML_FIXTURE_SOURCE}")
        if not QML_FIXTURE_RUNNER.is_file():
            self.fail(f"仓库 QML runner 不存在：{QML_FIXTURE_RUNNER}")
        if qmlscene is None or dbus_run_session is None:
            self.skipTest("Qt Quick fixture 需要 qmlscene 与 dbus-run-session")
        with tempfile.TemporaryDirectory(prefix="aad-qml-atspi-") as temporary:
            root = Path(temporary)
            environment = _native_subprocess_environment(self.session)
            environment["PYTHONPATH"] = str(PROJECT_ROOT / "src")
            environment.pop("AT_SPI_BUS_ADDRESS", None)
            for key, leaf in (
                ("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                ("XDG_CACHE_HOME", "cache"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_STATE_HOME", "state"),
                ("XDG_RUNTIME_DIR", "runtime"),
            ):
                directory = root / leaf
                directory.mkdir(mode=0o700)
                environment[key] = str(directory)
            command = [
                dbus_run_session, "--", sys.executable,
                str(QML_FIXTURE_RUNNER), qmlscene,
                str(QML_FIXTURE_SOURCE), str(DRIVER_PATH),
            ]
            process = subprocess.Popen(
                command, env=environment, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                encoding="utf-8", start_new_session=True,
            )
            try:
                stdout, stderr = process.communicate(timeout=45)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=3)
                self.fail("QML fixture runner 超时且已回收整个进程组")
            completed = subprocess.CompletedProcess(
                command, process.returncode, stdout, stderr
            )
        try:
            report = json.loads(completed.stdout.strip().splitlines()[-1])
        except (IndexError, json.JSONDecodeError) as exc:
            self.fail(
                f"QML fixture runner 未输出 JSON：{completed.stdout!r}; {exc}"
            )
        self.assertEqual(
            completed.returncode, 0, msg=f"{report!r}\n{completed.stderr}"
        )
        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["toolkit"], "Qt Quick")
        self.assertEqual(report["actions"], ["invoke"])
        self.assertEqual(report["native_interface"], "Action.do_action")
        self.assertEqual(report["native_action_name"], "Press")
        self.assertTrue(report["postcondition_observed"])
        self.assertFalse(report["input_injection"])
        self.assertFalse(report["ocr"])

    def test_system_settings_is_observed_when_qt_bridge_is_available(self) -> None:
        executable = shutil.which("systemsettings5") or shutil.which("systemsettings")
        if executable is None:
            self.skipTest("未安装 KDE System Settings")
        environment = os.environ.copy()
        environment.update(self.session)
        environment["QT_LINUX_ACCESSIBILITY_ALWAYS_ON"] = "1"
        environment["QT_ACCESSIBILITY"] = "1"
        # The long-lived desktop bus can retain stale registrations after
        # abrupt fixture exits. A dedicated test bus gives each qualification
        # run deterministic ownership while keeping the real X11 display.
        environment.pop("AT_SPI_BUS_ADDRESS", None)
        before_bus_names: set[str] = set()
        with _patched_environment(self.session):
            try:
                initial_backend = atspi.GioAtspiBackend()
                before_bus_names = {
                    str(item.get("bus_name"))
                    for item in initial_backend.list_applications(
                        deadline=time.monotonic() + 5
                    )
                }
            except atspi.DriverError as exc:
                self.skipTest(f"Gio/AT-SPI 不可用：{exc.code}")
        process = subprocess.Popen(
            [executable],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.addCleanup(self._stop_process, process)
        with _patched_environment(self.session):
            try:
                backend = atspi.GioAtspiBackend()
            except atspi.DriverError as exc:
                self.skipTest(f"Gio/AT-SPI 不可用：{exc.code}")
            found: dict[str, object] | None = None
            stop = time.monotonic() + 10
            # System Settings 可能移交给已有的单实例进程并让启动器退出，
            # 因此继续观察 registry，而不把启动器退出视为失败。
            while time.monotonic() < stop:
                applications = backend.list_applications(deadline=time.monotonic() + 3)
                found = next(
                    (
                        item
                        for item in applications
                        if item.get("process_id") == process.pid
                        or (
                            str(item.get("bus_name")) not in before_bus_names
                            and "system settings"
                            in str(item.get("name", "")).lower()
                        )
                    ),
                    None,
                )
                if found is not None:
                    break
                time.sleep(0.2)
            if found is None:
                self.skipTest(
                    "System Settings 未注册到 AT-SPI；当前 Qt AT-SPI bridge 不可用"
                )
            snapshot = backend.capture(
                {"bus_name": found["bus_name"]},
                max_depth=3,
                max_nodes=128,
                deadline=time.monotonic() + 20,
            )
            self.assertGreaterEqual(len(snapshot.nodes), 1)
            self.assertLessEqual(len(snapshot.nodes), 128)

    @staticmethod
    def _stop_process(process: subprocess.Popen[bytes]) -> None:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        if process.stderr is not None:
            process.stderr.close()
        if process.stdout is not None:
            process.stdout.close()


@unittest.skipUnless(sys.platform.startswith("linux"), "仅在 Linux 运行")
class NativeIsolatedX11TypeTextTests(unittest.TestCase):
    """Use a private Xvfb and private AT-SPI bus for deterministic XTEST."""

    def setUp(self) -> None:
        if shutil.which("Xvfb") is None or shutil.which("dbus-run-session") is None:
            self.skipTest("XTest 真机测试需要 Xvfb 与 dbus-run-session")
        session = _kde_x11_test_environment()
        if session is None:
            self.skipTest("需要当前用户的 KDE/X11 会话作为资格门")
        self.base_environment = os.environ.copy()
        self.base_environment.update(session)
        self.base_environment["PYTHONPATH"] = str(PROJECT_ROOT / "src")
        self.base_environment["XDG_SESSION_TYPE"] = "x11"
        self.base_environment["XDG_CURRENT_DESKTOP"] = "KDE"
        self.base_environment.pop("AT_SPI_BUS_ADDRESS", None)
        self.temporary = tempfile.TemporaryDirectory(prefix="aad-x11-type-text-")
        self.addCleanup(self.temporary.cleanup)
        display_number = 120 + (os.getpid() % 300)
        self.display = f":{display_number}"
        self.xvfb = subprocess.Popen(
            [
                shutil.which("Xvfb"),
                self.display,
                "-screen",
                "0",
                "1280x800x24",
                "-nolisten",
                "tcp",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.addCleanup(NativeKdeX11AtspiInfrastructureTests._stop_process, self.xvfb)
        socket = Path("/tmp/.X11-unix") / f"X{display_number}"
        stop = time.monotonic() + 5
        while not socket.exists() and time.monotonic() < stop:
            if self.xvfb.poll() is not None:
                self.fail("Xvfb 在 display 准备前退出")
            time.sleep(0.05)
        if not socket.exists():
            self.fail("Xvfb display 未在 5 秒内准备就绪")
        self.base_environment["DISPLAY"] = self.display
        build = subprocess.run(
            ["sh", str(XTEST_BUILD_SCRIPT)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        self.assertEqual(
            build.returncode,
            0,
            build.stderr.decode("utf-8", errors="replace")[-2000:],
        )

    def _run_isolated(
        self, runner: Path, fixture: Path, mode: str = "--type-text"
    ) -> dict[str, object]:
        completed = subprocess.run(
            [
                shutil.which("dbus-run-session"),
                "--",
                sys.executable,
                str(runner),
                str(fixture),
                str(DRIVER_PATH),
                mode,
            ],
            env=self.base_environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=45,
        )
        try:
            report = json.loads(completed.stdout.strip().splitlines()[-1])
        except (IndexError, json.JSONDecodeError) as exc:
            self.fail(f"fixture runner 未输出 JSON：{completed.stdout!r}; {exc}")
        self.assertEqual(completed.returncode, 0, msg=f"{report!r}\n{completed.stderr}")
        return report

    def test_helper_rejects_wayland_and_unowned_focus_before_dispatch(self) -> None:
        helper = PROJECT_ROOT / "plugins" / "linux_atspi" / ".build" / "x11_xtest_helper"
        command = [
            str(helper),
            "type-text",
            "--expected-pid",
            str(os.getpid()),
            "--deadline-monotonic-ns",
            str(time.monotonic_ns() + 500_000_000),
        ]
        wayland_environment = dict(self.base_environment)
        wayland_environment["XDG_SESSION_TYPE"] = "wayland"
        wayland = subprocess.run(
            command,
            input=b"never",
            env=wayland_environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=3,
        )
        self.assertEqual(wayland.returncode, 69)
        wayland_result = json.loads(wayland.stdout.splitlines()[-1])
        self.assertFalse(wayland_result["dispatch_started"])
        self.assertEqual(wayland_result["code"], "unsupported_session")

        wrong_focus = subprocess.run(
            command,
            input=b"never",
            env=self.base_environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=3,
        )
        self.assertEqual(wrong_focus.returncode, 73)
        focus_result = json.loads(wrong_focus.stdout.splitlines()[-1])
        self.assertFalse(focus_result["dispatch_started"])
        self.assertIn(
            focus_result["code"],
            {"focus_owner_mismatch", "pid_property_unavailable"},
        )

    def test_gtk3_type_text_is_observed_after_xtest_dispatch(self) -> None:
        environment = _native_subprocess_environment(self.base_environment)
        probe = subprocess.run(
            [
                sys.executable,
                "-c",
                (
                    "import gi; gi.require_version('Gtk', '3.0'); "
                    "gi.require_version('Atspi', '2.0'); "
                    "from gi.repository import Atspi, Gtk"
                ),
            ],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=5,
        )
        if probe.returncode != 0:
            self.skipTest("GTK3/Atspi 2.0 依赖不可用")
        self.base_environment.update(environment)
        report = self._run_isolated(GTK_FIXTURE_RUNNER, FIXTURE_PATH)
        self.assertEqual(report["toolkit"], "gtk")
        self.assertEqual(report["input_injection"], "XTEST")
        self.assertTrue(report["type_text_observed"])
        self.assertFalse(report["pointer_click_observed"])

    def test_gtk3_pointer_click_is_observed_after_xtest_dispatch(self) -> None:
        environment = _native_subprocess_environment(self.base_environment)
        probe = subprocess.run(
            [
                sys.executable,
                "-c",
                (
                    "import gi; gi.require_version('Gtk', '3.0'); "
                    "gi.require_version('Atspi', '2.0'); "
                    "from gi.repository import Atspi, Gtk"
                ),
            ],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=5,
        )
        if probe.returncode != 0:
            self.skipTest("GTK3/Atspi 2.0 依赖不可用")
        self.base_environment.update(environment)
        report = self._run_isolated(GTK_FIXTURE_RUNNER, FIXTURE_PATH, "--pointer-click")
        self.assertEqual(report["toolkit"], "gtk")
        self.assertEqual(report["input_injection"], "XTEST")
        self.assertTrue(report["pointer_click_observed"])
        self.assertFalse(report["type_text_observed"])

    def test_qt5_type_text_is_observed_after_xtest_dispatch(self) -> None:
        compiler = shutil.which("g++")
        pkg_config = shutil.which("pkg-config")
        if compiler is None or pkg_config is None:
            self.skipTest("Qt5 fixture 需要 g++ 与 pkg-config")
        flags = subprocess.run(
            [pkg_config, "--cflags", "--libs", "Qt5Widgets"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=10,
        )
        if flags.returncode != 0:
            self.skipTest("Qt5Widgets 开发包不可用")
        executable = Path(self.temporary.name) / "qt_atspi_fixture"
        compiled = subprocess.run(
            [
                compiler,
                "-std=c++17",
                "-O2",
                "-fPIC",
                str(QT_FIXTURE_SOURCE),
                "-o",
                str(executable),
                *shlex.split(flags.stdout),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        self.assertEqual(
            compiled.returncode,
            0,
            compiled.stderr.decode("utf-8", errors="replace")[-2000:],
        )
        report = self._run_isolated(QT_FIXTURE_RUNNER, executable)
        self.assertEqual(report["toolkit"], "Qt")
        self.assertEqual(report["input_injection"], "XTEST")
        self.assertTrue(report["type_text_observed"])
        self.assertFalse(report["pointer_click_observed"])

    def test_qt5_pointer_click_is_observed_after_xtest_dispatch(self) -> None:
        compiler = shutil.which("g++")
        pkg_config = shutil.which("pkg-config")
        if compiler is None or pkg_config is None:
            self.skipTest("Qt5 fixture 需要 g++ 与 pkg-config")
        flags = subprocess.run(
            [pkg_config, "--cflags", "--libs", "Qt5Widgets"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=10,
        )
        if flags.returncode != 0:
            self.skipTest("Qt5Widgets 开发包不可用")
        executable = Path(self.temporary.name) / "qt_atspi_fixture"
        compiled = subprocess.run(
            [
                compiler,
                "-std=c++17",
                "-O2",
                "-fPIC",
                str(QT_FIXTURE_SOURCE),
                "-o",
                str(executable),
                *shlex.split(flags.stdout),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        self.assertEqual(
            compiled.returncode,
            0,
            compiled.stderr.decode("utf-8", errors="replace")[-2000:],
        )
        report = self._run_isolated(QT_FIXTURE_RUNNER, executable, "--pointer-click")
        self.assertEqual(report["toolkit"], "Qt")
        self.assertEqual(report["input_injection"], "XTEST")
        self.assertTrue(report["pointer_click_observed"])
        self.assertFalse(report["type_text_observed"])


if __name__ == "__main__":
    unittest.main()
