#!/usr/bin/env python3
"""在隔离会话中验证真实 KCalc 的 AT-SPI 语义计算闭环。"""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import time
from typing import Any

from ai_auto_desktop.plugin import ProcessPlugin


BUTTONS = ("1", "+", "2", "=")
INITIAL_RESULT = "0"
FINAL_RESULT = "3"
PRIVATE_DISPLAY_PATTERN = re.compile(r"^:[1-9][0-9]{2,3}$")
CHILD_ENVIRONMENT_KEYS = frozenset({
    "DBUS_SESSION_BUS_ADDRESS", "DISPLAY", "GI_TYPELIB_PATH",
    "HOME", "KDE_FULL_SESSION", "KDE_SESSION_VERSION",
    "QT_ACCESSIBILITY", "QT_LINUX_ACCESSIBILITY_ALWAYS_ON",
    "QT_QPA_PLATFORM", "XAUTHORITY", "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME", "XDG_CURRENT_DESKTOP", "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR", "XDG_SESSION_ID", "XDG_SESSION_TYPE",
    "XDG_STATE_HOME",
})


def _json_failure(reason: str, *, status: int = 64) -> int:
    print(json.dumps({"status": "failed", "reason": reason}))
    return status


def _stop_owned(process: subprocess.Popen[Any]) -> None:
    if process.poll() is not None:
        return
    try:
        if os.getpgid(process.pid) != process.pid:
            return
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        try:
            if os.getpgid(process.pid) == process.pid:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
        except (OSError, ProcessLookupError, subprocess.TimeoutExpired):
            pass
    except (OSError, ProcessLookupError):
        pass


def _assert_owned_application(
    process: subprocess.Popen[Any], expected_start_time: int
) -> None:
    if process.poll() is not None:
        raise RuntimeError("KCalc exited before the requested action")
    if os.getpgid(process.pid) != process.pid:
        raise RuntimeError("KCalc is not the owned process-group leader")
    if (Path("/proc") / str(process.pid)).stat().st_uid != os.getuid():
        raise RuntimeError("KCalc process is not owned by the current user")
    if _process_start_time(process.pid) != expected_start_time:
        raise RuntimeError("KCalc process identity changed before the requested action")


def _process_start_time(pid: int) -> int:
    raw = (Path("/proc") / str(pid) / "stat").read_text(encoding="ascii")
    fields = raw[raw.rindex(")") + 2:].split()
    return int(fields[19])


def main() -> int:
    if len(sys.argv) not in {4, 5}:
        return _json_failure("invalid_arguments")
    mode = "semantic"
    if len(sys.argv) == 5:
        if sys.argv[4] != "--pointer-click":
            return _json_failure("invalid_arguments")
        mode = "pointer_click"
    executable = Path(sys.argv[1])
    window_manager = Path(sys.argv[2])
    driver = Path(sys.argv[3]).resolve()
    if (
        not executable.is_absolute()
        or executable != Path("/usr/bin/kcalc")
        or executable.is_symlink()
        or not executable.is_file()
        or executable.stat().st_uid != 0
        or stat.S_IMODE(executable.stat().st_mode) & 0o022
    ):
        return _json_failure("invalid_kcalc")
    if (
        not window_manager.is_absolute()
        or window_manager != Path("/usr/bin/kwin_x11")
        or window_manager.is_symlink()
        or not window_manager.is_file()
        or window_manager.stat().st_uid != 0
        or stat.S_IMODE(window_manager.stat().st_mode) & 0o022
    ):
        return _json_failure("invalid_window_manager")
    display = os.environ.get("DISPLAY", "")
    session_bus = os.environ.get("DBUS_SESSION_BUS_ADDRESS", "")
    expected_display = os.environ.get("AI_AUTO_DESKTOP_TEST_XVFB_DISPLAY", "")
    expected_xvfb_pid = os.environ.get("AI_AUTO_DESKTOP_TEST_XVFB_PID", "")
    expected_xvfb_start = os.environ.get(
        "AI_AUTO_DESKTOP_TEST_XVFB_START_TIME", ""
    )
    private_root = os.environ.get("AI_AUTO_DESKTOP_TEST_PRIVATE_ROOT", "")
    private_token = os.environ.get("AI_AUTO_DESKTOP_TEST_PRIVATE_TOKEN", "")
    if (
        not PRIVATE_DISPLAY_PATTERN.fullmatch(display)
        or not session_bus.startswith("unix:path=/tmp/dbus-")
        or display != expected_display
        or not expected_xvfb_pid.isdecimal()
        or not expected_xvfb_start.isdecimal()
        or not private_root
        or not private_token
    ):
        return _json_failure("private_xvfb_not_proven")
    xvfb_pid = int(expected_xvfb_pid)
    supplied_root = Path(private_root)
    root = supplied_root.resolve()
    try:
        supplied_root_metadata = supplied_root.lstat()
        xvfb_executable = (
            Path("/proc") / str(xvfb_pid) / "exe"
        ).resolve(strict=True)
        xvfb_uid = (Path("/proc") / str(xvfb_pid)).stat().st_uid
        xvfb_start = _process_start_time(xvfb_pid)
        xvfb_argv = (
            Path("/proc") / str(xvfb_pid) / "cmdline"
        ).read_bytes().split(b"\0")
        own_path_names = (
            "HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
            "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR",
        )
        own_path_items = []
        for name in own_path_names:
            supplied_path = Path(os.environ[name])
            metadata = supplied_path.lstat()
            own_path_items.append((supplied_path.resolve(), metadata))
        root_metadata = root.stat()
        token_path = root / ".xvfb-owner-token"
        token_metadata = token_path.lstat()
        token_value = token_path.read_text(encoding="ascii")
        xauthority_path = Path(os.environ["XAUTHORITY"])
        xauthority = xauthority_path.resolve()
        xauthority_metadata = xauthority_path.lstat()
    except (KeyError, OSError):
        return _json_failure("private_xvfb_not_proven")
    if (
        xvfb_executable.name != "Xvfb"
        or xvfb_uid != os.getuid()
        or xvfb_start != int(expected_xvfb_start)
        or display.encode("ascii") not in xvfb_argv
        or b"-nolisten" not in xvfb_argv
        or b"tcp" not in xvfb_argv
        or b"-auth" not in xvfb_argv
        or str(xauthority).encode("utf-8") not in xvfb_argv
        or stat.S_ISLNK(supplied_root_metadata.st_mode)
        or not stat.S_ISDIR(supplied_root_metadata.st_mode)
        or root_metadata.st_uid != os.getuid()
        or stat.S_IMODE(root_metadata.st_mode) != 0o700
        or stat.S_ISLNK(token_metadata.st_mode)
        or not stat.S_ISREG(token_metadata.st_mode)
        or token_metadata.st_uid != os.getuid()
        or stat.S_IMODE(token_metadata.st_mode) != 0o600
        or token_value != private_token
        or any(
            (
                (root != resolved and root not in resolved.parents)
                or stat.S_ISLNK(metadata.st_mode)
                or not stat.S_ISDIR(metadata.st_mode)
                or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) != 0o700
            )
            for resolved, metadata in own_path_items
        )
        or root not in xauthority.parents
        or stat.S_ISLNK(xauthority_metadata.st_mode)
        or not stat.S_ISREG(xauthority_metadata.st_mode)
        or xauthority_metadata.st_uid != os.getuid()
        or stat.S_IMODE(xauthority_metadata.st_mode) != 0o600
    ):
        return _json_failure("private_xvfb_not_proven")

    environment = {
        key: value for key, value in os.environ.items()
        if key in CHILD_ENVIRONMENT_KEYS and value
    }
    environment["PATH"] = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin"
    environment["PYTHONPATH"] = str(driver.parents[2] / "src")
    environment["LANG"] = "C.UTF-8"
    environment["LC_ALL"] = "C.UTF-8"
    environment["QT_QPA_PLATFORM"] = "xcb"
    environment["QT_LINUX_ACCESSIBILITY_ALWAYS_ON"] = "1"
    environment["QT_ACCESSIBILITY"] = "1"
    environment.pop("AT_SPI_BUS_ADDRESS", None)
    dpkg_query = Path("/usr/bin/dpkg-query")
    if not dpkg_query.is_file():
        return _json_failure("kcalc_version_unavailable", status=1)
    version = subprocess.run(
        [str(dpkg_query), "-W", "-f=${Version}", "kcalc"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        encoding="utf-8",
        check=False,
        timeout=5,
    )
    version_text = version.stdout.strip()[:200]
    if version.returncode != 0 or not version_text:
        return _json_failure("kcalc_version_unavailable", status=1)
    package_owner = subprocess.run(
        [str(dpkg_query), "-S", str(executable)],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        encoding="utf-8",
        check=False,
        timeout=5,
    )
    if (
        package_owner.returncode != 0
        or package_owner.stdout.strip() != "kcalc: /usr/bin/kcalc"
    ):
        return _json_failure("kcalc_package_untrusted", status=1)
    window_manager_owner = subprocess.run(
        [str(dpkg_query), "-S", str(window_manager)],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        encoding="utf-8",
        check=False,
        timeout=5,
    )
    if (
        window_manager_owner.returncode != 0
        or window_manager_owner.stdout.strip() != "kwin-x11: /usr/bin/kwin_x11"
    ):
        return _json_failure("window_manager_package_untrusted", status=1)

    window_manager_process = subprocess.Popen(
        [str(window_manager), "--replace"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        window_manager_start_time = _process_start_time(window_manager_process.pid)
    except (OSError, UnicodeError, ValueError, IndexError):
        _stop_owned(window_manager_process)
        return _json_failure("window_manager_identity_unavailable", status=1)
    time.sleep(0.3)
    try:
        _assert_owned_application(window_manager_process, window_manager_start_time)
    except Exception:
        _stop_owned(window_manager_process)
        return _json_failure("window_manager_start_failed", status=1)

    application_process = subprocess.Popen(
        [str(executable)],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        application_start_time = _process_start_time(application_process.pid)
    except (OSError, UnicodeError, ValueError, IndexError):
        _stop_owned(application_process)
        _stop_owned(window_manager_process)
        return _json_failure("kcalc_identity_unavailable", status=1)
    plugin: ProcessPlugin | None = None
    stage = "registration"
    try:
        plugin = ProcessPlugin(
            [sys.executable, str(driver)],
            env=environment,
            timeout=20,
            name="desktop.linux_atspi owned KCalc",
        )
        plugin.start(timeout=10)
        application: dict[str, Any] | None = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if application_process.poll() is not None:
                raise RuntimeError("KCalc exited before AT-SPI registration")
            listing = plugin.invoke(
                "desktop.linux_atspi.list_applications@1", {}, timeout=3
            )
            matches = [
                item for item in listing.get("applications", [])
                if item.get("process_id") == application_process.pid
            ]
            if len(matches) == 1:
                application = matches[0]
                break
            if len(matches) > 1:
                raise RuntimeError("KCalc PID is ambiguous in the AT-SPI registry")
            time.sleep(0.2)
        if application is None:
            raise RuntimeError("KCalc did not register with AT-SPI")
        if application.get("name") != "kcalc":
            raise RuntimeError("unexpected AT-SPI application name")
        if application.get("toolkit_name") != "Qt":
            raise RuntimeError("KCalc did not report the Qt toolkit")
        _assert_owned_application(application_process, application_start_time)

        selector: dict[str, Any] = {"process_id": application_process.pid}
        for key in ("bus_name", "toolkit_name"):
            value = application.get(key)
            if isinstance(value, str) and value:
                selector[key] = value

        def snapshot() -> dict[str, Any]:
            captured = plugin.invoke(
                "desktop.linux_atspi.snapshot@1",
                {"application": selector, "max_depth": 32, "max_nodes": 1000},
                timeout=10,
            )
            if captured.get("truncated") is not False:
                raise RuntimeError("KCalc snapshot was truncated")
            return captured

        def find(captured: dict[str, Any], locator: dict[str, Any]) -> dict[str, Any]:
            return plugin.invoke(
                "desktop.linux_atspi.find@1",
                {
                    "snapshot_id": captured["snapshot_id"],
                    "revision": captured["revision"],
                    "locator": locator,
                },
                timeout=5,
            )

        def activate_button(name: str) -> dict[str, Any]:
            _assert_owned_application(application_process, application_start_time)
            locator = {
                "role": "push_button",
                "name": name,
                "toolkit_name": "Qt",
                "actions": ["invoke"],
                "states": {
                    "enabled": True,
                    "visible": True,
                    "showing": True,
                    "sensitive": True,
                },
            }
            captured = snapshot()
            located = find(captured, locator)
            target = located["target"]
            node = located["node"]
            if mode == "semantic":
                result = plugin.invoke(
                    "desktop.linux_atspi.invoke@1",
                    {"target": target, "locator": locator},
                    timeout=5,
                )
            else:
                result = plugin.invoke(
                    "desktop.linux_atspi.pointer_click@1",
                    {
                        "target": target,
                        "locator": locator,
                        "button": "left",
                        "position": "center",
                    },
                    # KCalc exposes about 217 AT-SPI nodes on this image.  A
                    # pointer action performs another full fresh capture plus
                    # component hit-testing before XTEST, so keep a larger but
                    # still bounded per-action deadline than semantic Press.
                    timeout=12,
                )
            backend = result.get("backend_result", {})
            if mode == "semantic":
                if backend.get("native_interface") != "Action.do_action":
                    raise RuntimeError(
                        f"button {name!r} did not use Action.do_action"
                    )
                if backend.get("native_action_name") != "Press":
                    raise RuntimeError(f"button {name!r} did not use exact Press")
                if backend.get("accepted") is not True:
                    raise RuntimeError(f"button {name!r} was not accepted")
                action_evidence = {
                    "native_interface": backend["native_interface"],
                    "native_action_name": backend["native_action_name"],
                    "accepted": True,
                    "synthetic_input": False,
                }
            else:
                input_result = backend.get("input", {})
                if backend.get("native_interface") != "Component.grab_focus -> XTEST":
                    raise RuntimeError(f"button {name!r} did not use XTEST pointer click")
                if input_result.get("native_interface") != "XTEST":
                    raise RuntimeError(f"button {name!r} did not report XTEST")
                if input_result.get("submitted") is not True:
                    raise RuntimeError(f"button {name!r} pointer click was not submitted")
                if backend.get("button") != "left":
                    raise RuntimeError(f"button {name!r} did not use the left button")
                if backend.get("click_point") != input_result.get("click_point"):
                    raise RuntimeError(f"button {name!r} click point evidence disagrees")
                action_evidence = {
                    "native_interface": backend["native_interface"],
                    "synthetic_input": backend.get("synthetic_input"),
                    "submitted": input_result["submitted"],
                    "button_kind": backend["button"],
                    "click_point": input_result.get("click_point"),
                    "focus": backend.get("focus"),
                }
            return {
                "button": name,
                "role": node.get("role"),
                "states": node.get("states"),
                "provenance": {
                    key: node.get("provenance", {}).get(key)
                    for key in (
                        "bus_name", "object_path", "process_id",
                        "toolkit_name", "toolkit_version",
                    )
                },
                **action_evidence,
            }

        result_locator = {
            "role": "frame",
            "name": INITIAL_RESULT,
            "toolkit_name": "Qt",
            "states": {"visible": True, "showing": True},
        }
        stage = "initial_snapshot"
        initial = snapshot()
        initial_result = find(initial, result_locator)["node"]
        if initial_result.get("name") != INITIAL_RESULT:
            raise RuntimeError("KCalc initial display was not zero")
        initial_provenance = initial_result.get("provenance", {})
        display_identity = {
            key: initial_provenance.get(key)
            for key in ("bus_name", "object_path", "process_id")
        }
        if (
            not all(display_identity.values())
            or display_identity["process_id"] != application_process.pid
        ):
            raise RuntimeError("KCalc display identity was incomplete")

        stage = "invoke_buttons"
        actions = []
        for name in BUTTONS:
            stage = f"activate_button_{name}"
            actions.append(activate_button(name))

        stage = "postcondition"
        final_locator = {
            **result_locator,
            "name": FINAL_RESULT,
            "bus_name": display_identity["bus_name"],
            "object_path": display_identity["object_path"],
        }
        deadline = time.monotonic() + 3
        final_result: dict[str, Any] | None = None
        while time.monotonic() < deadline:
            try:
                final_result = find(snapshot(), final_locator)["node"]
                break
            except Exception:
                time.sleep(0.1)
        if final_result is None or final_result.get("name") != FINAL_RESULT:
            raise RuntimeError("KCalc did not expose the expected result")
        final_provenance = final_result.get("provenance", {})
        final_display_identity = {
            key: final_provenance.get(key)
            for key in ("bus_name", "object_path", "process_id")
        }
        if final_display_identity != display_identity:
            raise RuntimeError("KCalc result came from a different display element")

        print(json.dumps({
            "status": "passed",
            "application": "kcalc",
            "application_process_id": application_process.pid,
            "application_process_start_time": application_start_time,
            "toolkit": application["toolkit_name"],
            "toolkit_version": application.get("toolkit_version"),
            "application_version": version_text,
            "executable": str(executable.resolve()),
            "display": display,
            "display_kind": "private_xvfb_with_kwin_x11",
            "node_count": len(initial.get("nodes", [])),
            "snapshot_truncated": False,
            "operation": "1+2=3",
            "action_mode": mode,
            "actions": actions,
            "fresh_snapshot_before_each_action": True,
            "fresh_snapshot_postcondition": True,
            "postcondition_observed": True,
            "display_identity": display_identity,
            "final_display_identity": final_display_identity,
            "isolation": {
                "private_xvfb": True,
                "x11_tcp_disabled": True,
                "private_xauthority": True,
                "private_session_bus": bool(
                    session_bus
                ),
                "private_home_xdg": True,
                "window_manager_started": True,
                "window_manager": "kwin_x11",
                "window_manager_process_id": window_manager_process.pid,
                "window_manager_process_start_time": window_manager_start_time,
            },
            "input_injection": "XTEST" if mode == "pointer_click" else False,
            "ocr": False,
            "screenshots": False,
        }, ensure_ascii=False, sort_keys=True))
        return 0
    except Exception as exc:
        print(json.dumps({
            "status": "failed",
            "stage": stage,
            "error_type": type(exc).__name__,
            "message": str(exc)[:1000],
            "plugin_stderr": (plugin.stderr[-4000:] if plugin is not None else ""),
        }, ensure_ascii=False, sort_keys=True))
        return 1
    finally:
        if plugin is not None:
            plugin.close()
        _stop_owned(application_process)
        _stop_owned(window_manager_process)


if __name__ == "__main__":
    raise SystemExit(main())
