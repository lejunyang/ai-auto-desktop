#!/usr/bin/env python3
"""Run the owned GTK3 fixture and XTest input in one isolated a11y bus."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import time

from ai_auto_desktop.plugin import ProcessPlugin


TYPE_ENTRY_NAME = "Fixture XTest text entry"


def main() -> int:
    if len(sys.argv) not in {3, 4} or (len(sys.argv) == 4 and sys.argv[3] != "--type-text"):
        print(json.dumps({"status": "failed", "reason": "invalid_arguments"}))
        return 64
    fixture_path = Path(sys.argv[1]).resolve()
    driver_path = Path(sys.argv[2]).resolve()
    environment = os.environ.copy()
    environment["GDK_BACKEND"] = "x11"
    environment["GTK_A11Y"] = "always"
    environment.pop("NO_AT_BRIDGE", None)
    environment.pop("AT_SPI_BUS_ADDRESS", None)
    stage = "launch_fixture"
    fixture = subprocess.Popen(
        [sys.executable, str(fixture_path)],
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
        time.sleep(0.5)
        stage = "start_driver"
        plugin = ProcessPlugin(
            [sys.executable, str(driver_path)],
            env=environment,
            timeout=30,
            name="desktop.linux_atspi isolated GTK3 XTest fixture",
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
        if application.get("toolkit_name") != "gtk":
            raise RuntimeError(f"unexpected toolkit: {application!r}")
        selector = {"process_id": fixture.pid}

        def snapshot() -> dict[str, object]:
            result = plugin.invoke(
                "desktop.linux_atspi.snapshot@1",
                {"application": selector, "max_depth": 8, "max_nodes": 96},
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

        locator = {
            "role": "text",
            "name": TYPE_ENTRY_NAME,
            "toolkit_name": "gtk",
        }
        stage = "initial_snapshot"
        initial = find(snapshot(), locator)["node"]
        if initial["value"] != "":
            raise RuntimeError("XTest entry did not start empty")
        if "type_text" not in initial["actions"]:
            raise RuntimeError(
                f"XTest entry did not qualify for type_text: {initial!r}"
            )
        stage = "invoke_type_text"
        typed = "GTK XTest UTF-8 你好"
        result = write("type_text", locator, text=typed)
        input_result = result["backend_result"].get("input", {})
        if input_result.get("native_interface") != "XTEST":
            raise RuntimeError("type_text did not use the XTEST helper")
        stage = "observe_type_text"
        stop = time.monotonic() + 3
        last_value = None
        while True:
            try:
                observed = find(snapshot(), {**locator, "value": typed})["node"]
                if observed["value"] == typed:
                    break
            except Exception as exc:
                try:
                    last_value = find(snapshot(), locator)["node"].get("value")
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
        print(
            json.dumps(
                {
                    "status": "passed",
                    "toolkit": application["toolkit_name"],
                    "toolkit_version": application["toolkit_version"],
                    "actions": ["focus", "type_text"],
                    "input_injection": "XTEST",
                    "type_text_observed": True,
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
