#!/usr/bin/env python3
"""Run the owned Qt 5 fixture inside one isolated accessibility bus."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import time

from ai_auto_desktop.plugin import PluginError, ProcessPlugin


ENTRY_NAME = "Qt fixture text entry"
BUTTON_NAME = "Invoke Qt fixture button"
STATUS_INVOKED = "Qt fixture status invoked"


def main() -> int:
    if len(sys.argv) != 3:
        print(json.dumps({"status": "failed", "reason": "invalid_arguments"}))
        return 64
    executable = Path(sys.argv[1]).resolve()
    driver = Path(sys.argv[2]).resolve()
    environment = os.environ.copy()
    environment["QT_LINUX_ACCESSIBILITY_ALWAYS_ON"] = "1"
    environment["QT_ACCESSIBILITY"] = "1"
    stage = "launch_fixture"
    fixture = subprocess.Popen(
        [str(executable)],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    plugin: ProcessPlugin | None = None
    try:
        stage = "wait_ready"
        assert fixture.stdout is not None
        ready = fixture.stdout.readline().decode("utf-8", errors="replace").strip()
        if ready != "READY":
            raise RuntimeError(f"fixture did not report READY: {ready!r}")
        # Qt emits READY after its top-level window exists. The fresh registry
        # receives registration asynchronously, so allow one bounded interval.
        time.sleep(0.5)
        stage = "start_driver"
        plugin = ProcessPlugin(
            [sys.executable, str(driver)],
            env=environment,
            timeout=30,
            name="desktop.linux_atspi isolated Qt5 fixture",
        )
        plugin.start()
        application = None
        stop = time.monotonic() + 10
        while time.monotonic() < stop:
            applications = plugin.invoke(
                "desktop.linux_atspi.list_applications@1", {}
            )
            application = next(
                (
                    item
                    for item in applications["applications"]
                    if item.get("process_id") == fixture.pid
                ),
                None,
            )
            if application is not None:
                break
            time.sleep(0.2)
        if application is None:
            raise RuntimeError("fixture was not registered in the isolated AT-SPI registry")
        if application.get("toolkit_name") != "Qt":
            raise RuntimeError(f"unexpected toolkit: {application!r}")
        selector = {"process_id": fixture.pid}

        def snapshot() -> dict[str, object]:
            result = plugin.invoke(
                "desktop.linux_atspi.snapshot@1",
                {"application": selector, "max_depth": 8, "max_nodes": 64},
            )
            if result.get("truncated") is not False:
                raise RuntimeError("fixture snapshot was truncated")
            return result

        def find(captured: dict[str, object], locator: dict[str, object]) -> dict[str, object]:
            return plugin.invoke(
                "desktop.linux_atspi.find@1",
                {
                    "snapshot_id": captured["snapshot_id"],
                    "revision": captured["revision"],
                    "locator": locator,
                },
            )

        def write(action: str, locator: dict[str, object], **extra: object) -> dict[str, object]:
            captured = snapshot()
            target = find(captured, locator)["target"]
            return plugin.invoke(
                f"desktop.linux_atspi.{action}@1",
                {"target": target, "locator": locator, **extra},
            )

        entry_locator = {
            "role": "text",
            "name": ENTRY_NAME,
            "toolkit_name": "Qt",
            "actions": ["focus", "set_text"],
        }
        button_locator = {
            "role": "push_button",
            "name": BUTTON_NAME,
            "toolkit_name": "Qt",
            "actions": ["invoke"],
        }
        stage = "initial_snapshot"
        initial_entry = find(snapshot(), entry_locator)["node"]
        if initial_entry["value"] != "Qt fixture initial text":
            raise RuntimeError("initial entry value was not observable")
        if initial_entry["provenance"].get("accessible_id") is not None:
            raise RuntimeError("Qt 5 fixture unexpectedly exposed AccessibleId")
        # Qt 5 does not export AccessibleId. The driver deliberately refuses
        # a write unless bus/path/process/fingerprint all remain stable; a
        # focus request may legitimately expose this platform limitation.
        stage = "focus"
        try:
            focus_result = write("focus", entry_locator)
        except PluginError as exc:
            if exc.code != "DRIVER.STALE_SNAPSHOT":
                raise
        else:
            if not focus_result["backend_result"].get("accepted"):
                raise RuntimeError("focus was not accepted")
        stage = "set_text"
        changed = "Qt fixture changed through AT-SPI"
        if not write("set_text", entry_locator, text=changed)["backend_result"].get(
            "accepted"
        ):
            raise RuntimeError("set_text was not accepted")
        changed_node = find(
            snapshot(), {**entry_locator, "value": changed}
        )["node"]
        if changed_node["value"] != changed:
            raise RuntimeError("set_text was not observed")
        stage = "invoke"
        invoked = write("invoke", button_locator)
        if invoked["backend_result"].get("native_action_name") != "Press":
            raise RuntimeError("Qt button did not use exact Press action")
        stop = time.monotonic() + 3
        while True:
            try:
                status = find(
                    snapshot(),
                    {
                        "role": "label",
                        "name": STATUS_INVOKED,
                        "toolkit_name": "Qt",
                    },
                )
                if status["node"]["name"] == STATUS_INVOKED:
                    break
            except Exception:
                if time.monotonic() >= stop:
                    raise
                time.sleep(0.1)
        print(
            json.dumps(
                {
                    "status": "passed",
                    "toolkit": application["toolkit_name"],
                    "toolkit_version": application["toolkit_version"],
                    "actions": ["focus", "set_text", "invoke"],
                    "input_injection": False,
                    "ocr": False,
                },
                ensure_ascii=False,
            )
        )
        return 0
    except BaseException as exc:
        print(
            json.dumps(
                {
                    "status": "failed",
                    "reason": type(exc).__name__,
                    "message": str(exc),
                    "stage": stage,
                },
                ensure_ascii=False,
            )
        )
        return 1
    finally:
        if plugin is not None:
            plugin.close()
        if fixture.poll() is None:
            fixture.terminate()
            try:
                fixture.wait(timeout=3)
            except subprocess.TimeoutExpired:
                fixture.kill()
                fixture.wait(timeout=3)
        if fixture.stdout is not None:
            fixture.stdout.close()
        if fixture.stderr is not None:
            fixture.stderr.close()


if __name__ == "__main__":
    raise SystemExit(main())
