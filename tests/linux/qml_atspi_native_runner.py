#!/usr/bin/env python3
"""Exercise one semantic invoke against the owned Qt Quick fixture."""

from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
from typing import Any

from ai_auto_desktop.plugin import ProcessPlugin


BUTTON_NAME = "Invoke QML fixture button"
STATUS_IDLE = "QML fixture status idle"
STATUS_INVOKED = "QML fixture status invoked"
CHILD_ENVIRONMENT_KEYS = frozenset({
    "DBUS_SESSION_BUS_ADDRESS", "DISPLAY", "GI_TYPELIB_PATH",
    "HOME", "KDE_FULL_SESSION", "KDE_SESSION_VERSION",
    "QT_ACCESSIBILITY", "QT_LINUX_ACCESSIBILITY_ALWAYS_ON",
    "QT_QPA_PLATFORM", "XAUTHORITY", "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME", "XDG_CURRENT_DESKTOP", "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR", "XDG_SESSION_ID", "XDG_SESSION_TYPE",
    "XDG_STATE_HOME",
})


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


def main() -> int:
    if len(sys.argv) != 4:
        print(json.dumps({"status": "failed", "reason": "invalid_arguments"}))
        return 64
    # Keep the qmlscene symlink as argv[0]: Debian's qtchooser selects the
    # actual tool from that basename and fails if resolved to "qtchooser".
    qmlscene = Path(sys.argv[1])
    fixture_source = Path(sys.argv[2]).resolve()
    driver = Path(sys.argv[3]).resolve()
    if not qmlscene.is_absolute() or not qmlscene.is_file():
        print(json.dumps({"status": "failed", "reason": "invalid_qmlscene"}))
        return 64
    environment = {
        key: value for key, value in os.environ.items()
        if key in CHILD_ENVIRONMENT_KEYS and value
    }
    environment["PATH"] = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin"
    environment["PYTHONPATH"] = str(driver.parents[2] / "src")
    environment["LANG"] = "C.UTF-8"
    environment["LC_ALL"] = "C.UTF-8"
    fixture_source = fixture_source.resolve()
    environment["QT_QPA_PLATFORM"] = "xcb"
    environment["QT_LINUX_ACCESSIBILITY_ALWAYS_ON"] = "1"
    environment["QT_ACCESSIBILITY"] = "1"
    environment.pop("AT_SPI_BUS_ADDRESS", None)
    fixture = subprocess.Popen(
        [str(qmlscene), str(fixture_source)],
        env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL, start_new_session=True,
    )
    plugin: ProcessPlugin | None = None
    stage = "registration"
    try:
        plugin = ProcessPlugin(
            [sys.executable, str(driver)], env=environment, timeout=20,
            name="desktop.linux_atspi owned QML fixture",
        )
        plugin.start(timeout=10)
        application: dict[str, Any] | None = None
        stop = time.monotonic() + 10
        while time.monotonic() < stop:
            if fixture.poll() is not None:
                raise RuntimeError("QML fixture exited before registration")
            listing = plugin.invoke(
                "desktop.linux_atspi.list_applications@1", {}, timeout=3
            )
            matches = [
                item for item in listing.get("applications", [])
                if item.get("process_id") == fixture.pid
            ]
            if len(matches) == 1:
                application = matches[0]
                break
            if len(matches) > 1:
                raise RuntimeError("QML fixture PID is ambiguous")
            time.sleep(0.2)
        if application is None:
            raise RuntimeError("QML fixture did not register with AT-SPI")
        if application.get("toolkit_name") != "Qt":
            raise RuntimeError("QML fixture did not report the Qt toolkit")
        selector: dict[str, Any] = {"process_id": fixture.pid}
        for key in ("bus_name", "toolkit_name"):
            value = application.get(key)
            if isinstance(value, str) and value:
                selector[key] = value

        def snapshot() -> dict[str, Any]:
            captured = plugin.invoke(
                "desktop.linux_atspi.snapshot@1",
                {"application": selector, "max_depth": 8, "max_nodes": 64},
                timeout=5,
            )
            if captured.get("truncated") is not False:
                raise RuntimeError("QML fixture snapshot was truncated")
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

        button_locator = {
            "role": "push_button", "name": BUTTON_NAME,
            "toolkit_name": "Qt", "actions": ["invoke"],
        }
        idle_locator = {
            "role": "label", "name": STATUS_IDLE,
            "toolkit_name": "Qt",
        }
        invoked_locator = {
            "role": "label", "name": STATUS_INVOKED,
            "toolkit_name": "Qt",
        }
        stage = "initial_snapshot"
        initial = snapshot()
        find(initial, idle_locator)
        button = find(initial, button_locator)
        stage = "invoke"
        invoked = plugin.invoke(
            "desktop.linux_atspi.invoke@1",
            {"target": button["target"], "locator": button_locator},
            timeout=5,
        )
        backend = invoked.get("backend_result", {})
        if backend.get("native_interface") != "Action.do_action":
            raise RuntimeError("QML invoke did not use the AT-SPI Action interface")
        if backend.get("native_action_name") != "Press":
            raise RuntimeError("QML invoke did not use the exact Press action")
        if backend.get("accepted") is not True:
            raise RuntimeError("QML invoke was not accepted")
        stage = "postcondition"
        stop = time.monotonic() + 3
        while True:
            try:
                find(snapshot(), invoked_locator)
                break
            except Exception:
                if time.monotonic() >= stop:
                    raise
                time.sleep(0.1)
        print(json.dumps({
            "status": "passed",
            "toolkit": "Qt Quick",
            "actions": ["invoke"],
            "native_interface": backend["native_interface"],
            "native_action_name": backend["native_action_name"],
            "postcondition_observed": True,
            "input_injection": False,
            "ocr": False,
        }, sort_keys=True))
        return 0
    except Exception as exc:
        print(json.dumps({
            "status": "failed", "stage": stage,
            "error_type": type(exc).__name__, "message": str(exc)[:1000],
        }, sort_keys=True))
        return 1
    finally:
        if plugin is not None:
            plugin.close()
        _stop_owned(fixture)


if __name__ == "__main__":
    raise SystemExit(main())
