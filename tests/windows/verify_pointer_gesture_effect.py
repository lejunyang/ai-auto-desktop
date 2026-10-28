#!/usr/bin/env python3
"""Verify pointer gestures through aad against an isolated Win32 input probe.

The probe is created and destroyed by this script. Each action goes through the
real aad path (one `aad do` observes, finds and acts in a control session), then this script independently
reads Raw Input records from the probe's JSONL log. It never touches an existing
application window.
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
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

RI_MOUSE_LEFT_BUTTON_DOWN = 0x0001
RI_MOUSE_LEFT_BUTTON_UP = 0x0002
RI_MOUSE_RIGHT_BUTTON_DOWN = 0x0004
RI_MOUSE_RIGHT_BUTTON_UP = 0x0008
RI_MOUSE_MIDDLE_BUTTON_DOWN = 0x0010
RI_MOUSE_MIDDLE_BUTTON_UP = 0x0020
RI_MOUSE_WHEEL = 0x0400
RI_MOUSE_HWHEEL = 0x0800

user32 = ctypes.WinDLL("user32", use_last_error=True)
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.GetWindowRect.restype = wintypes.BOOL
user32.GetCursorPos.argtypes = [ctypes.POINTER(wintypes.POINT)]
user32.GetCursorPos.restype = wintypes.BOOL


def aad(*args: str) -> dict:
    prefix = [AAD, "--compact"] if args[0] in {"find", "do"} else [AAD]
    result = subprocess.run([*prefix, *args], capture_output=True, text=True, timeout=30)
    try:
        payload = json.loads(result.stdout.strip())
    except json.JSONDecodeError:
        raise RuntimeError(f"aad returned non-JSON: {result.stdout!r} {result.stderr!r}")
    if result.returncode != 0:
        raise RuntimeError(f"aad {' '.join(args)} failed: {payload}")
    return payload


def read_log(path: str) -> list[dict]:
    if not os.path.exists(path):
        return []
    with open(path, "r", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def raw_mouse(entries: list[dict]) -> list[dict]:
    return [entry for entry in entries if entry.get("channel") == "raw_input_mouse"]


BUTTON = ["--role", "button", "--name", "Probe target"]


def main() -> int:
    log_path = os.path.join(tempfile.gettempdir(), f"aad-pointer-probe-{os.getpid()}.jsonl")
    if os.path.exists(log_path):
        os.remove(log_path)

    title = f"AAD Pointer Probe {os.getpid()}"
    probe = subprocess.Popen(
        [sys.executable, PROBE, "--title", title, "--log", log_path],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        ready_line = probe.stdout.readline()
        if not ready_line.strip():
            raise RuntimeError(f"probe failed to start: {probe.stderr.read()}")
        ready = json.loads(ready_line)
        window_id = f"hwnd:{ready['window_handle']}"
        aad("find", window_id, *BUTTON)
        aad("find", window_id, "--role", "edit")
        time.sleep(0.3)

        findings: dict[str, object] = {
            "isolated_probe": True,
            "raw_input_registered": ready["raw_input_registered"],
        }

        def perform(name: str, *args: str) -> list[dict]:
            before = len(read_log(log_path))
            response = aad("do", args[0], window_id, *BUTTON, *args[1:])
            time.sleep(0.45)
            findings[f"{name}_applied"] = (
                response.get("result", {}).get("backend_result", {}).get("applied") is True
            )
            return raw_mouse(read_log(log_path)[before:])

        right = perform(
            "right_click", "click", "--button", "right"
        )
        right_flags = [entry.get("button_flags", 0) for entry in right]
        findings["right_click_seen"] = (
            any(flag & RI_MOUSE_RIGHT_BUTTON_DOWN for flag in right_flags)
            and any(flag & RI_MOUSE_RIGHT_BUTTON_UP for flag in right_flags)
        )

        middle = perform(
            "middle_click", "click", "--button", "middle"
        )
        middle_flags = [entry.get("button_flags", 0) for entry in middle]
        findings["middle_click_seen"] = (
            any(flag & RI_MOUSE_MIDDLE_BUTTON_DOWN for flag in middle_flags)
            and any(flag & RI_MOUSE_MIDDLE_BUTTON_UP for flag in middle_flags)
        )

        double = perform(
            "double_click", "click", "--count", "2"
        )
        double_flags = [entry.get("button_flags", 0) for entry in double]
        findings["double_click_seen"] = (
            sum(bool(flag & RI_MOUSE_LEFT_BUTTON_DOWN) for flag in double_flags) >= 2
            and sum(bool(flag & RI_MOUSE_LEFT_BUTTON_UP) for flag in double_flags) >= 2
        )

        wheel = perform(
            "vertical_scroll", "scroll", "--direction", "down", "--notches", "3",
        )
        wheel_entries = [
            entry for entry in wheel if entry.get("button_flags", 0) & RI_MOUSE_WHEEL
        ]
        findings["vertical_scroll_seen"] = (
            len(wheel_entries) >= 3
            and all(int(entry.get("button_data", 0)) != 0 for entry in wheel_entries)
        )

        horizontal = perform(
            "horizontal_scroll", "scroll", "--direction", "right", "--notches", "2",
        )
        horizontal_entries = [
            entry for entry in horizontal if entry.get("button_flags", 0) & RI_MOUSE_HWHEEL
        ]
        findings["horizontal_scroll_seen"] = len(horizontal_entries) >= 2

        started = time.perf_counter()
        hover = perform(
            "hover", "hover", "--ms", "350"
        )
        elapsed_ms = (time.perf_counter() - started) * 1000
        rect = wintypes.RECT()
        point = wintypes.POINT()
        user32.GetWindowRect(wintypes.HWND(ready["button_handle"]), ctypes.byref(rect))
        user32.GetCursorPos(ctypes.byref(point))
        findings["hover_movement_seen"] = any(
            entry.get("last_x") or entry.get("last_y") for entry in hover
        )
        findings["hover_cursor_inside"] = (
            rect.left <= point.x < rect.right and rect.top <= point.y < rect.bottom
        )
        findings["hover_dwelled"] = elapsed_ms >= 300

        drag = perform(
            "drag", "drag", "--to-role", "edit",
            "--fx", "0.5", "--fy", "0.5", "--to-fx", "0.5", "--to-fy", "0.5",
        )
        drag_flags = [entry.get("button_flags", 0) for entry in drag]
        moved = any(entry.get("last_x") or entry.get("last_y") for entry in drag)
        findings["drag_seen"] = (
            any(flag & RI_MOUSE_LEFT_BUTTON_DOWN for flag in drag_flags)
            and moved
            and any(flag & RI_MOUSE_LEFT_BUTTON_UP for flag in drag_flags)
        )

        required = [
            "raw_input_registered",
            "right_click_applied", "right_click_seen",
            "middle_click_applied", "middle_click_seen",
            "double_click_applied", "double_click_seen",
            "vertical_scroll_applied", "vertical_scroll_seen",
            "horizontal_scroll_applied", "horizontal_scroll_seen",
            "hover_applied", "hover_movement_seen", "hover_cursor_inside", "hover_dwelled",
            "drag_applied", "drag_seen",
        ]
        findings["passed"] = all(findings.get(key) is True for key in required)
        print(json.dumps(findings, indent=2, ensure_ascii=False, sort_keys=True))
        return 0 if findings["passed"] else 1
    finally:
        probe.terminate()
        try:
            probe.wait(timeout=5)
        except subprocess.TimeoutExpired:
            probe.kill()
        try:
            os.remove(log_path)
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
