"""KDE/X11 会话中的真实 Gio AT-SPI 只读冒烟测试。

测试辅助可以从当前用户的 ``kwin_x11`` 进程恢复图形会话环境；生产驱动不得
扫描 ``/proc`` 或猜测其他会话。缺少 KDE/X11、Gio、AT-SPI bus、System Settings
或 Qt AT-SPI bridge 时，本测试保守跳过。
"""

from __future__ import annotations

from contextlib import contextmanager
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import unittest

from ai_auto_desktop.plugin import ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
DRIVER_PATH = PROJECT_ROOT / "plugins" / "linux_atspi" / "linux_atspi_driver.py"

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
            and "KDE" in values.get("XDG_CURRENT_DESKTOP", "").upper()
            and values.get("DISPLAY")
            and values.get("DBUS_SESSION_BUS_ADDRESS")
        ):
            return values
    return None


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
        applications_result = plugin.invoke(
            "desktop.linux_atspi.list_applications@1", {}
        )
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
            self.fail(f"KDE/X11 AT-SPI bus 可读但默认后端不可用：{backend.details!r}")
        self.assertIsInstance(backend, atspi.GioAtspiBackend)
        self.assertEqual(backend.name, "gio_atspi")

    def test_system_settings_is_observed_when_qt_bridge_is_available(self) -> None:
        executable = shutil.which("systemsettings5") or shutil.which("systemsettings")
        if executable is None:
            self.skipTest("未安装 KDE System Settings")
        environment = os.environ.copy()
        environment.update(self.session)
        environment["QT_LINUX_ACCESSIBILITY_ALWAYS_ON"] = "1"
        environment["QT_ACCESSIBILITY"] = "1"
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
        if process.poll() is not None:
            return
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
