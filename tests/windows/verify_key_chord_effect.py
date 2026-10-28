#!/usr/bin/env python3
"""Prove a chord is understood as a shortcut, by its effect in a real app.

The per-channel probe shows the keys leave the machine; this shows an
application *acts* on them. Notepad's edit control is used: type text, send
Ctrl+A, and read the selection back through the EDIT's own EM_GETSEL. If Ctrl
and A arrived as a combination (right order, Ctrl still down when A is pressed),
the whole text is selected; if the ordering were wrong, nothing would be.

Uses the real aad CLI path (find -> do key-chord). Briefly takes focus.
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
AAD = os.path.join(REPO, "target", "debug", "aad.exe")

user32 = ctypes.WinDLL("user32", use_last_error=True)
WM_GETTEXTLENGTH = 0x000E
EM_GETSEL = 0x00B0
EM_SETSEL = 0x00B1


def aad(*args: str) -> dict:
    prefix = [AAD, "--compact"] if args[0] in {"find", "do"} else [AAD]
    result = subprocess.run([*prefix, *args], capture_output=True, text=True, encoding="utf-8", timeout=30)
    try:
        return json.loads(result.stdout.strip())
    except json.JSONDecodeError:
        return {"_raw": result.stdout.strip(), "_err": result.stderr.strip()}


def main() -> int:
    findings: dict = {}
    # Launch Notepad and let it settle.
    notepad = subprocess.Popen(["notepad.exe"])
    time.sleep(1.5)
    try:
        apps = aad("apps")
        windows = [app.get("metadata", {}) for app in apps.get("apps", [])]
        window_id = None
        for w in windows:
            proc = str(w.get("process_name", "")).lower()
            if "notepad" in proc:
                window_id = w.get("window_id")
                break
        findings["window_id"] = window_id
        if not window_id:
            findings["error"] = "notepad window not found"
            findings["windows"] = [w.get("process_name") for w in windows]
            print(json.dumps(findings, indent=2, ensure_ascii=False))
            return 1

        # The editable document area.
        role = "Edit"
        found = aad("find", window_id, "--role", role)
        if not found.get("found"):
            role = "Document"
            found = aad("find", window_id, "--role", role)
        findings["target_found"] = bool(found.get("found"))
        if not found.get("found"):
            findings["error"] = "no edit/document element in notepad"
            findings["find"] = found
            print(json.dumps(findings, indent=2, ensure_ascii=False))
            return 1
        # Put known text in, via the driver's own set_value (no synthetic input),
        # so the starting state does not depend on typing working.
        sample = "select all of this text"
        set_result = aad("do", "fill", window_id, "--role", role, "--value", sample)
        findings["set_value_applied"] = set_result["result"]["backend_result"].get("applied", set_result)
        time.sleep(0.4)

        # Locate the EDIT hwnd to read its selection directly.
        edit_hwnd = find_edit_hwnd("Notepad")
        findings["edit_hwnd_found"] = edit_hwnd is not None

        # Collapse any selection first, so the Ctrl+A result is unambiguous.
        if edit_hwnd:
            user32.SendMessageW(edit_hwnd, EM_SETSEL, 0, 0)
        time.sleep(0.2)

        # The real test: Ctrl+A through the whole aad stack.
        chord = aad("do", "key", window_id, "--role", role, "--keys", "ctrl,a")
        findings["ctrl_a_applied"] = chord["result"]["backend_result"].get("applied", chord)
        time.sleep(0.5)

        if edit_hwnd:
            start = wintypes.DWORD()
            end = wintypes.DWORD()
            user32.SendMessageW(edit_hwnd, EM_GETSEL,
                                ctypes.byref(start), ctypes.byref(end))
            length = user32.SendMessageW(edit_hwnd, WM_GETTEXTLENGTH, 0, 0)
            findings["text_length"] = int(length)
            findings["selection"] = [int(start.value), int(end.value)]
            findings["whole_text_selected"] = (
                start.value == 0 and end.value >= length and length > 0
            )
        print(json.dumps(findings, indent=2, ensure_ascii=False))
        return 0
    finally:
        notepad.terminate()
        try:
            notepad.wait(timeout=5)
        except Exception:
            notepad.kill()


def find_edit_hwnd(process_hint: str):
    """Find Notepad's edit control hwnd. Classic Notepad uses class 'Edit';
    the newer one nests a RichEditD2DPT inside. Try both."""
    found = {"hwnd": None}
    EnumWindowsProc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    EnumChildProc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    user32.GetClassNameW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
    user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]

    def class_of(hwnd) -> str:
        buf = ctypes.create_unicode_buffer(256)
        user32.GetClassNameW(hwnd, buf, 256)
        return buf.value

    candidates = []

    @EnumChildProc
    def on_child(hwnd, _lp):
        cls = class_of(hwnd)
        if cls in ("Edit", "RICHEDIT50W", "RichEditD2DPT"):
            candidates.append((hwnd, cls))
        return True

    @EnumWindowsProc
    def on_top(hwnd, _lp):
        if class_of(hwnd) == "Notepad":
            user32.EnumChildWindows(hwnd, on_child, 0)
        return True

    user32.EnumWindows(on_top, 0)
    # Prefer a classic Edit; fall back to the first candidate.
    for hwnd, cls in candidates:
        if cls == "Edit":
            return hwnd
    return candidates[0][0] if candidates else None


if __name__ == "__main__":
    raise SystemExit(main())
