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
TYPE_ENTRY_NAME = "Qt fixture XTest text entry"
BUTTON_NAME = "Invoke Qt fixture button"
STATUS_INVOKED = "Qt fixture status invoked"


def main() -> int:
    if len(sys.argv) not in {3, 4}:
        print(json.dumps({"status": "failed", "reason": "invalid_arguments"}))
        return 64
    mode = "semantic"
    if len(sys.argv) == 4:
        flag = sys.argv[3]
        if flag == "--type-text":
            mode = "type_text"
        elif flag == "--pointer-click":
            mode = "pointer_click"
        else:
            print(json.dumps({"status": "failed", "reason": "invalid_arguments"}))
            return 64
    executable = Path(sys.argv[1]).resolve()
    driver = Path(sys.argv[2]).resolve()
    environment = os.environ.copy()
    environment["QT_LINUX_ACCESSIBILITY_ALWAYS_ON"] = "1"
    environment["QT_ACCESSIBILITY"] = "1"
    environment.pop("AT_SPI_BUS_ADDRESS", None)
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
        type_entry_locator = {
            "role": "text",
            "name": TYPE_ENTRY_NAME,
            "toolkit_name": "Qt",
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
        if mode == "type_text":
            stage = "invoke_type_text"
            initial_type_entry = find(snapshot(), type_entry_locator)["node"]
            if initial_type_entry["value"] != "":
                raise RuntimeError("XTest entry did not start empty")
            if "type_text" not in initial_type_entry["actions"]:
                raise RuntimeError(
                    f"XTest entry did not qualify for type_text: {initial_type_entry!r}"
                )
            focus_result = write("focus", type_entry_locator)
            if not focus_result["backend_result"].get("accepted"):
                raise RuntimeError("XTest entry focus was not accepted")
            time.sleep(0.1)
            typed = "Qt XTest UTF-8 你好"
            typed_result = write("type_text", type_entry_locator, text=typed)
            input_result = typed_result["backend_result"].get("input", {})
            if input_result.get("native_interface") != "XTEST":
                raise RuntimeError("type_text did not use the XTEST helper")
            stage = "observe_type_text"
            stop = time.monotonic() + 3
            last_value = None
            while True:
                try:
                    typed_node = find(
                        snapshot(), {**type_entry_locator, "value": typed}
                    )["node"]
                    if typed_node["value"] == typed:
                        break
                except Exception as exc:
                    try:
                        last_value = find(snapshot(), type_entry_locator)["node"].get("value")
                    except Exception:
                        pass
                    if time.monotonic() >= stop:
                        text_nodes = [
                            {
                                "name": node.get("name"),
                                "value": node.get("value"),
                                "focused": node.get("states", {}).get("focused"),
                            }
                            for node in snapshot()["nodes"]
                            if node.get("role") in {"text", "entry"}
                        ]
                        raise RuntimeError(
                            f"type_text postcondition not observed; value={last_value!r}; "
                            f"text_nodes={text_nodes!r}; input_result={input_result!r}"
                        ) from exc
                    time.sleep(0.1)
        if mode == "pointer_click":
            stage = "invoke_pointer_click"
            initial_button = find(snapshot(), button_locator)["node"]
            if "pointer_click" not in initial_button["actions"]:
                raise RuntimeError(
                    f"Qt button did not qualify for pointer_click: {initial_button!r}"
                )
            clicked = write(
                "pointer_click",
                button_locator,
                button="left",
                position="center",
            )
            input_result = clicked["backend_result"].get("input", {})
            if input_result.get("native_interface") != "XTEST":
                raise RuntimeError("pointer_click did not use the XTEST helper")
        stage = "invoke"
        if mode == "pointer_click":
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
        else:
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
                    "actions": (
                        ["focus", "pointer_click"]
                        if mode == "pointer_click"
                        else (
                            ["focus", "set_text", "type_text", "invoke"]
                            if mode == "type_text"
                            else ["focus", "set_text", "invoke"]
                        )
                    ),
                    "input_injection": "XTEST" if mode in {"type_text", "pointer_click"} else False,
                    "type_text_observed": mode == "type_text",
                    "pointer_click_observed": mode == "pointer_click",
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
