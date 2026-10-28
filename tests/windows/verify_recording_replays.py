#!/usr/bin/env python3
"""Record with `aad record --save`, then replay the saved workflow with `aad run`.

An isolated Win32 probe window is created and destroyed here. While `aad record`
watches it, `aad do` changes the edit box and presses the button, standing in
for a person. The saved recording is in the platform-neutral step format; the
edit box is then cleared and the workflow replayed, and the box is read back
independently with WM_GETTEXT.
"""
import ctypes
import json
import os
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
AAD = os.path.join(REPO, "target", "debug", "aad.exe")
PROBE = os.path.join(HERE, "input_channel_probe.py")
WM_GETTEXT = 0x000D


def aad(*args: str, env: dict | None = None) -> tuple[dict, int]:
    run = subprocess.run([AAD, "--compact", *args], capture_output=True, text=True,
                         encoding="utf-8", timeout=120, env=env)
    return json.loads(run.stdout), run.returncode


def edit_text(handle: int) -> str:
    buffer = ctypes.create_unicode_buffer(256)
    ctypes.windll.user32.SendMessageW(handle, WM_GETTEXT, 256, buffer)
    return buffer.value


def main() -> int:
    store = tempfile.mkdtemp(prefix="aad-record-store-")
    env = dict(os.environ, AAD_RECORDINGS_DIR=store)
    title = f"AAD Record Probe {os.getpid()}"
    log = os.path.join(tempfile.gettempdir(), f"aad-record-{os.getpid()}.jsonl")
    probe = subprocess.Popen([sys.executable, PROBE, "--title", title, "--log", log],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        ready = json.loads(probe.stdout.readline())
        window = f"hwnd:{ready['window_handle']}"
        recorder = subprocess.Popen(
            [AAD, "--compact", "record", window, "--seconds", "8", "--save", "probe-recording"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", env=env,
        )
        time.sleep(2.5)
        aad("do", "fill", window, "--role", "edit", "--value", "recorded text")
        time.sleep(0.5)
        aad("do", "invoke", window, "--role", "button", "--name", "Probe target")
        out, _ = recorder.communicate(timeout=60)
        recorded = json.loads(out)
        steps = recorded.get("steps", [])
        workflow = recorded.get("saved", {}).get("workflow_path")

        aad("do", "fill", window, "--role", "edit", "--value", "")
        cleared = edit_text(ready["edit_handle"])
        result, code = (aad("run", workflow, "--permission", "desktop.input",
                            "--permission", "desktop.observe", env=env) if workflow else ({}, 1))
        replayed = edit_text(ready["edit_handle"])
        findings = {
            "recorded_actions": [s.get("action", {}).get("type") for s in steps],
            "recorded_app": recorded.get("app"),
            "skipped": recorded.get("skipped"),
            "saved": bool(workflow),
            "cleared_before_replay": cleared == "",
            "replay_status": result.get("status"),
            "replayed_text": replayed,
        }
        findings["passed"] = (
            "fill" in findings["recorded_actions"]
            and findings["saved"]
            and findings["cleared_before_replay"]
            and code == 0
            and replayed == "recorded text"
        )
        print(json.dumps(findings, ensure_ascii=False, indent=1))
        return 0 if findings["passed"] else 1
    finally:
        probe.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
