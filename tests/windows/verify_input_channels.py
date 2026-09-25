#!/usr/bin/env python3
"""Drive the input-channel probe with SendInput and report what each channel saw.

Verification 1 of docs/research/input-synthesis-and-background-execution.md:
are SendInput keystrokes visible to Raw Input / GetAsyncKeyState, or only to
the window message queue? And is the mouse different from the keyboard?

Runs on the *current* desktop, so it needs an interactive session. It restores
nothing about the user's focus, so it foregrounds its own probe window first.
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
PROBE = os.path.join(HERE, "input_channel_probe.py")

INPUT_MOUSE = 0
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004
KEYEVENTF_SCANCODE = 0x0008
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_RIGHTDOWN = 0x0008
MOUSEEVENTF_RIGHTUP = 0x0010
MOUSEEVENTF_WHEEL = 0x0800
MOUSEEVENTF_ABSOLUTE = 0x8000
MOUSEEVENTF_VIRTUALDESK = 0x4000

user32 = ctypes.WinDLL("user32", use_last_error=True)
wparam = ctypes.c_size_t
ULONG_PTR = ctypes.c_size_t


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG), ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR),
    ]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD), ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class INPUTUNION(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("union", INPUTUNION)]


user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.MapVirtualKeyW.argtypes = [wintypes.UINT, wintypes.UINT]
user32.MapVirtualKeyW.restype = wintypes.UINT
user32.GetAsyncKeyState.argtypes = [ctypes.c_int]
user32.GetAsyncKeyState.restype = ctypes.c_short
user32.GetForegroundWindow.restype = wintypes.HWND
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.SetForegroundWindow.restype = wintypes.BOOL
user32.SetFocus.argtypes = [wintypes.HWND]
user32.SetFocus.restype = wintypes.HWND
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.GetWindowThreadProcessId.restype = wintypes.DWORD
user32.AttachThreadInput.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.BOOL]
user32.AttachThreadInput.restype = wintypes.BOOL
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.GetWindowRect.restype = wintypes.BOOL
user32.GetSystemMetrics.argtypes = [ctypes.c_int]
user32.GetSystemMetrics.restype = ctypes.c_int


def send(events: list[INPUT], label: str) -> dict:
    array = (INPUT * len(events))(*events)
    sent = user32.SendInput(len(events), array, ctypes.sizeof(INPUT))
    error = ctypes.get_last_error()
    return {"label": label, "requested": len(events), "sent": sent, "last_error": error}


def key_vk(vk: int, up: bool = False) -> INPUT:
    scan = user32.MapVirtualKeyW(vk, 0)
    return INPUT(type=INPUT_KEYBOARD, union=INPUTUNION(
        ki=KEYBDINPUT(wVk=vk, wScan=scan, dwFlags=KEYEVENTF_KEYUP if up else 0,
                      time=0, dwExtraInfo=0)))


def key_scan(scan: int, up: bool = False) -> INPUT:
    flags = KEYEVENTF_SCANCODE | (KEYEVENTF_KEYUP if up else 0)
    return INPUT(type=INPUT_KEYBOARD, union=INPUTUNION(
        ki=KEYBDINPUT(wVk=0, wScan=scan, dwFlags=flags, time=0, dwExtraInfo=0)))


def key_unicode(ch: str, up: bool = False) -> INPUT:
    flags = KEYEVENTF_UNICODE | (KEYEVENTF_KEYUP if up else 0)
    return INPUT(type=INPUT_KEYBOARD, union=INPUTUNION(
        ki=KEYBDINPUT(wVk=0, wScan=ord(ch), dwFlags=flags, time=0, dwExtraInfo=0)))


def mouse_at(x: int, y: int, flags: int, data: int = 0) -> INPUT:
    ox = user32.GetSystemMetrics(76)   # SM_XVIRTUALSCREEN
    oy = user32.GetSystemMetrics(77)
    w = user32.GetSystemMetrics(78)
    h = user32.GetSystemMetrics(79)
    ax = int(round((x - ox) * 65535 / w))
    ay = int(round((y - oy) * 65535 / h))
    return INPUT(type=INPUT_MOUSE, union=INPUTUNION(
        mi=MOUSEINPUT(dx=ax, dy=ay, mouseData=data,
                      dwFlags=flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                      time=0, dwExtraInfo=0)))


def read_log(path: str) -> list[dict]:
    if not os.path.exists(path):
        return []
    with open(path, "r", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def main() -> int:
    log_path = os.path.join(tempfile.gettempdir(), f"aad-input-probe-{os.getpid()}.jsonl")
    if os.path.exists(log_path):
        os.remove(log_path)

    title = f"AAD Input Probe {os.getpid()}"
    process = subprocess.Popen(
        [sys.executable, PROBE, "--title", title, "--log", log_path],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    ready_line = process.stdout.readline()
    if not ready_line.strip():
        print("probe failed to start:", process.stderr.read(), file=sys.stderr)
        return 1
    ready = json.loads(ready_line)

    hwnd = ready["window_handle"]
    edit = ready["edit_handle"]
    # SetForegroundWindow is refused (ERROR_ACCESS_DENIED) unless the calling
    # process owns the foreground or has just received input. AttachThreadInput
    # to the current foreground thread lifts that restriction, which is the
    # standard workaround -- without it the window message channel never sees
    # anything and the result would look like "messages don't work".
    foreground = user32.GetForegroundWindow()
    target_thread = user32.GetWindowThreadProcessId(wintypes.HWND(hwnd), None)
    source_thread = user32.GetWindowThreadProcessId(foreground, None)
    attached = False
    if source_thread and target_thread and source_thread != target_thread:
        attached = bool(user32.AttachThreadInput(source_thread, target_thread, True))
    brought = bool(user32.SetForegroundWindow(wintypes.HWND(hwnd)))
    foreground_error = ctypes.get_last_error()
    time.sleep(0.4)
    # Focus the top-level window, not the EDIT child: keyboard messages are
    # delivered to the focused HWND's wndproc, and only the top-level one is
    # instrumented by this probe.
    user32.SetFocus(wintypes.HWND(hwnd))
    time.sleep(0.2)
    actual_foreground = user32.GetForegroundWindow()

    results = {
        "ready": ready,
        "foreground": {
            "attached_thread_input": attached,
            "set_foreground_returned": brought,
            "last_error": foreground_error,
            "probe_is_foreground": int(actual_foreground) == int(hwnd),
        },
        "steps": [],
    }

    def step(name: str, events: list[INPUT], settle: float = 0.35) -> None:
        before = len(read_log(log_path))
        dispatch = send(events, name)
        time.sleep(settle)
        entries = read_log(log_path)[before:]
        channels: dict[str, list] = {}
        for entry in entries:
            channels.setdefault(entry["channel"], []).append(entry)
        results["steps"].append({
            "step": name, "dispatch": dispatch,
            "channels": {k: len(v) for k, v in channels.items()},
            "entries": entries,
        })

    # -- keyboard, three encodings -----------------------------------------
    step("key_vk_A", [key_vk(0x41), key_vk(0x41, up=True)])
    step("key_scancode_A", [key_scan(0x1E), key_scan(0x1E, up=True)])
    step("key_unicode_A", [key_unicode("A"), key_unicode("A", up=True)])

    # -- a real combination: Ctrl+A ----------------------------------------
    step("ctrl_a", [key_vk(0x11), key_vk(0x41), key_vk(0x41, up=True), key_vk(0x11, up=True)])

    # -- a function key, which UNICODE cannot express ----------------------
    step("f1", [key_vk(0x70), key_vk(0x70, up=True)])

    # -- mouse: is it different from the keyboard? -------------------------
    rect = wintypes.RECT()
    user32.GetWindowRect(wintypes.HWND(ready["button_handle"]), ctypes.byref(rect))
    cx = (rect.left + rect.right) // 2
    cy = (rect.top + rect.bottom) // 2
    step("mouse_left_click", [
        mouse_at(cx, cy, MOUSEEVENTF_MOVE),
        mouse_at(cx, cy, MOUSEEVENTF_LEFTDOWN),
        mouse_at(cx, cy, MOUSEEVENTF_LEFTUP),
    ])
    step("mouse_right_click", [
        mouse_at(cx, cy, MOUSEEVENTF_RIGHTDOWN),
        mouse_at(cx, cy, MOUSEEVENTF_RIGHTUP),
    ])
    step("mouse_wheel", [mouse_at(cx, cy, MOUSEEVENTF_WHEEL, data=120)])

    if attached:
        user32.AttachThreadInput(source_thread, target_thread, False)

    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()

    print(json.dumps(results, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
