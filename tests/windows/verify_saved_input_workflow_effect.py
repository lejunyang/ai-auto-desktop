#!/usr/bin/env python3
"""Verify saved keyboard and pointer actions survive a target restart.

This is deliberately an end-to-end probe rather than another assembler test:

1. start an isolated Win32 target and observe its edit/button through ``aad find``;
2. save structured actions through the real MCP ``save_workflow`` tool;
3. stop that target, start a new process/window with the same durable identity;
4. replay the saved ``.workflow.json`` with ``aad run``;
5. independently read the edit control and Raw Input log.

The workflow never contains the first process' snapshot target. Its locators are
built only from stable fields returned by ``find`` on observed elements.
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
AAD = REPO / "target" / "debug" / "aad.exe"
PROBE = HERE / "input_channel_probe.py"

WM_GETTEXT = 0x000D
WM_GETTEXTLENGTH = 0x000E
RI_MOUSE_LEFT_BUTTON_DOWN = 0x0001
RI_MOUSE_LEFT_BUTTON_UP = 0x0002
RI_MOUSE_RIGHT_BUTTON_DOWN = 0x0004
RI_MOUSE_RIGHT_BUTTON_UP = 0x0008

user32 = ctypes.WinDLL("user32", use_last_error=True)
user32.SendMessageW.argtypes = [
    wintypes.HWND,
    wintypes.UINT,
    ctypes.c_size_t,
    ctypes.c_ssize_t,
]
user32.SendMessageW.restype = ctypes.c_ssize_t


def aad(*args: str, env: dict[str, str] | None = None) -> dict:
    prefix = [str(AAD), "--compact"] if args[0] in {"apps", "find", "run", "workflows"} else [str(AAD)]
    result = subprocess.run(
        [*prefix, *args], capture_output=True, text=True, encoding="utf-8", timeout=60, env=env
    )
    try:
        payload = json.loads(result.stdout.strip())
    except json.JSONDecodeError as error:
        raise RuntimeError(
            f"aad returned non-JSON for {' '.join(args)}: {result.stdout!r} {result.stderr!r}"
        ) from error
    if result.returncode != 0:
        raise RuntimeError(f"aad {' '.join(args)} failed: {payload}")
    return payload


def start_probe(title: str, log_path: Path) -> tuple[subprocess.Popen[str], dict]:
    probe = subprocess.Popen(
        [
            sys.executable,
            str(PROBE),
            "--title",
            title,
            "--log",
            str(log_path),
            "--keyboard-layout",
            "00000409",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    ready_line = probe.stdout.readline()
    if not ready_line.strip():
        error = probe.stderr.read()
        probe.terminate()
        raise RuntimeError(f"probe failed to start: {error}")
    return probe, json.loads(ready_line)


def stop_probe(probe: subprocess.Popen[str]) -> None:
    probe.terminate()
    try:
        probe.wait(timeout=5)
    except subprocess.TimeoutExpired:
        probe.kill()
        probe.wait(timeout=5)


def find(window_id: str, *, role: str, name: str | None = None) -> dict:
    args = ["find", window_id, "--role", role]
    if name:
        args += ["--name", name]
    found = aad(*args)
    if not found.get("found") or not found.get("node"):
        raise RuntimeError(f"element was not observed: {found}")
    return found


def durable_locator(found: dict) -> dict:
    """Derive a locator only from the node returned by this observation."""
    node = found.get("node") or {}
    automation_id = node.get("automation_id")
    if not isinstance(automation_id, str) or not automation_id:
        raise RuntimeError(f"observed node has no durable automation_id: {found}")
    return {"automation_id": automation_id}


def observed_window(window_id: str) -> dict:
    listing = aad("apps")
    for app in listing.get("apps", []):
        if app.get("metadata", {}).get("window_id") == window_id:
            return app["metadata"]
    raise RuntimeError(f"observed window {window_id} disappeared")


def save_via_mcp(arguments: dict, env: dict[str, str]) -> dict:
    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "save_workflow", "arguments": arguments},
    }
    result = subprocess.run(
        [str(AAD), "mcp", "--allow-input"],
        input=json.dumps(request, separators=(",", ":")) + "\n",
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    if result.returncode != 0:
        raise RuntimeError(f"aad mcp failed: {result.stderr}")
    line = next((line for line in result.stdout.splitlines() if line.strip()), "")
    response = json.loads(line)
    tool_result = response.get("result") or {}
    if tool_result.get("isError"):
        raise RuntimeError(f"save_workflow refused: {tool_result}")
    content = tool_result.get("content") or []
    if not content:
        raise RuntimeError(f"save_workflow returned no content: {response}")
    return json.loads(content[0]["text"])


def edit_text(hwnd: int) -> str:
    length = int(user32.SendMessageW(wintypes.HWND(hwnd), WM_GETTEXTLENGTH, 0, 0))
    buffer = ctypes.create_unicode_buffer(length + 1)
    user32.SendMessageW(
        wintypes.HWND(hwnd), WM_GETTEXT, length + 1, ctypes.addressof(buffer)
    )
    return buffer.value


def read_log(path: Path) -> list[dict]:
    if not path.exists():
        return []
    with path.open("r", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def main() -> int:
    if sys.platform != "win32":
        print("the saved input workflow probe requires Windows", file=sys.stderr)
        return 2
    if not AAD.exists():
        print(f"build aad first: {AAD}", file=sys.stderr)
        return 2

    root = Path(tempfile.mkdtemp(prefix="aad-saved-input-"))
    store = root / "recordings"
    store.mkdir()
    first_log = root / "first.jsonl"
    second_log = root / "second.jsonl"
    title = f"AAD Saved Input Probe {os.getpid()}"
    name = f"saved_input_probe_{os.getpid()}"
    env = os.environ.copy()
    env["AAD_RECORDINGS_DIR"] = str(store)
    first: subprocess.Popen[str] | None = None
    second: subprocess.Popen[str] | None = None

    try:
        first, first_ready = start_probe(title, first_log)
        first_window_id = f"hwnd:{first_ready['window_handle']}"
        edit_found = find(first_window_id, role="edit")
        button_found = find(first_window_id, role="button", name="Probe target")
        edit_locator = durable_locator(edit_found)
        button_locator = durable_locator(button_found)
        window = observed_window(first_window_id)
        process_name = window.get("process_name")
        if not process_name:
            raise RuntimeError(f"observed window has no process name: {window}")
        selector = {"process_name": process_name, "title": title}

        steps = [
            {
                "action": "set_value",
                "locator": edit_locator,
                "window": selector,
                "argument": "replace me",
            },
            {
                "action": "key_chord",
                "locator": edit_locator,
                "window": selector,
                "parameters": {"keys": ["ctrl", "a"]},
            },
            {
                "action": "type_text",
                "locator": edit_locator,
                "window": selector,
                "argument": "aA1!",
                "parameters": {"mode": "keys"},
            },
            {
                "action": "pointer_gesture",
                "locator": button_locator,
                "window": selector,
                "parameters": {"button": "right", "count": 1},
            },
            {
                "action": "drag",
                "locator": button_locator,
                "destination": edit_locator,
                "window": selector,
                "parameters": {
                    "button": "left",
                    "fraction": {"fx": 0.5, "fy": 0.5},
                    "to_fraction": {"fx": 0.5, "fy": 0.5},
                },
            },
        ]
        recording_path = store / f"{name}.recording.json"
        recording_path.write_text(
            json.dumps(
                {
                    "apiVersion": "ai-auto-desktop.dev/v1alpha1",
                    "kind": "Recording",
                    "metadata": {"name": name},
                    "steps": [
                        {
                            "id": f"step_{index}",
                            **step,
                            "summary": step["action"],
                            "window_title": title,
                            "enabled": True,
                        }
                        for index, step in enumerate(steps, 1)
                    ],
                },
                indent=2,
            ),
            encoding="utf-8",
        )
        saved = save_via_mcp(
            {"name": name, "steps": steps, "overwrite": True}, env
        )
        workflow_path = Path(saved["path"])
        if saved.get("executed_steps") != 16 or not workflow_path.exists():
            raise RuntimeError(f"saved workflow has the wrong shape: {saved}")

        explanation = aad("workflows", name, env=env)
        actions = explanation.get("actions", [])
        if len(actions) != 5:
            raise RuntimeError(f"workflow description lost logical actions: {explanation}")

        old_handle = first_ready["window_handle"]
        stop_probe(first)
        first = None
        time.sleep(0.4)

        second, second_ready = start_probe(title, second_log)
        if second_ready["window_handle"] == old_handle:
            raise RuntimeError("target window handle did not change after restart")
        run = aad("run", str(workflow_path), env=env)
        time.sleep(0.7)

        entries = read_log(second_log)
        raw_keys = [entry for entry in entries if entry.get("channel") == "raw_input_keyboard"]
        raw_mouse = [entry for entry in entries if entry.get("channel") == "raw_input_mouse"]
        key_vkeys = [int(entry.get("vkey", 0)) for entry in raw_keys]
        mouse_flags = [int(entry.get("button_flags", 0)) for entry in raw_mouse]
        moved = any(entry.get("last_x") or entry.get("last_y") for entry in raw_mouse)
        actual_text = edit_text(second_ready["edit_handle"])
        workflow_text = workflow_path.read_text(encoding="utf-8")

        findings = {
            "isolated_probe": True,
            "saved_through_mcp": saved.get("kind") == "WorkflowSaved",
            "recording_source_written": recording_path.exists(),
            "workflow_written": workflow_path.exists(),
            "performed_steps": saved.get("performed_steps"),
            "executed_steps": saved.get("executed_steps"),
            "logical_actions_described": len(actions),
            "target_restarted": second_ready["window_handle"] != old_handle,
            "old_target_not_persisted": first_window_id not in workflow_text
            and "observation_id" not in workflow_text,
            "run_status": run.get("status"),
            "run_succeeded": run.get("status") == "succeeded",
            "expected_text": "aA1!",
            "actual_text": actual_text,
            "text_exact_after_ctrl_a_and_keys": actual_text == "aA1!",
            "ctrl_a_seen": 0x11 in key_vkeys and 0x41 in key_vkeys,
            "key_events": len(raw_keys),
            "right_click_seen": any(flag & RI_MOUSE_RIGHT_BUTTON_DOWN for flag in mouse_flags)
            and any(flag & RI_MOUSE_RIGHT_BUTTON_UP for flag in mouse_flags),
            "drag_seen": any(flag & RI_MOUSE_LEFT_BUTTON_DOWN for flag in mouse_flags)
            and moved
            and any(flag & RI_MOUSE_LEFT_BUTTON_UP for flag in mouse_flags),
        }
        required = [
            "saved_through_mcp",
            "recording_source_written",
            "workflow_written",
            "target_restarted",
            "old_target_not_persisted",
            "run_succeeded",
            "text_exact_after_ctrl_a_and_keys",
            "ctrl_a_seen",
            "right_click_seen",
            "drag_seen",
        ]
        findings["passed"] = all(findings.get(key) is True for key in required)
        print(json.dumps(findings, indent=2, ensure_ascii=False, sort_keys=True))
        return 0 if findings["passed"] else 1
    finally:
        if first is not None:
            stop_probe(first)
        if second is not None:
            stop_probe(second)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
