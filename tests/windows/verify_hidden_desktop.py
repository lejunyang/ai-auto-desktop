#!/usr/bin/env python3
"""Does SendInput follow the calling thread's desktop, or the visible one?

Verification 2 of docs/research/input-synthesis-and-background-execution.md.
This is the foundation of the whole Windows background story: if SendInput on a
SetThreadDesktop'd thread reaches a hidden desktop, agents can drive apps
without touching the user's session. If it does not, that plan is dead.

Two published projects appear to disagree on this, so the test is built to
distinguish *why*:

  - the probe runs on a hidden desktop created with CreateDesktopW
  - one worker thread calls SetThreadDesktop before SendInput
  - a control thread sends the same input WITHOUT SetThreadDesktop

and, just as important, it records whether the user's own desktop was
disturbed: foreground window and cursor position are sampled before and after.
Evidence is the probe's log file, not the screen -- nothing on a hidden desktop
can be seen.
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time


HERE = os.path.dirname(os.path.abspath(__file__))
PROBE = os.path.join(HERE, "input_channel_probe.py")

GENERIC_ALL = 0x10000000
DESKTOP_NAME = "AadVerifyHidden"

INPUT_MOUSE = 0
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_ABSOLUTE = 0x8000
MOUSEEVENTF_VIRTUALDESK = 0x4000

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
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


class STARTUPINFOW(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD), ("lpReserved", wintypes.LPWSTR),
        ("lpDesktop", wintypes.LPWSTR), ("lpTitle", wintypes.LPWSTR),
        ("dwX", wintypes.DWORD), ("dwY", wintypes.DWORD),
        ("dwXSize", wintypes.DWORD), ("dwYSize", wintypes.DWORD),
        ("dwXCountChars", wintypes.DWORD), ("dwYCountChars", wintypes.DWORD),
        ("dwFillAttribute", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
        ("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
        ("lpReserved2", ctypes.POINTER(ctypes.c_byte)),
        ("hStdInput", wintypes.HANDLE), ("hStdOutput", wintypes.HANDLE),
        ("hStdError", wintypes.HANDLE),
    ]


class PROCESS_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("hProcess", wintypes.HANDLE), ("hThread", wintypes.HANDLE),
        ("dwProcessId", wintypes.DWORD), ("dwThreadId", wintypes.DWORD),
    ]


user32.CreateDesktopW.argtypes = [
    wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.LPVOID,
    wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
]
user32.CreateDesktopW.restype = wintypes.HANDLE
user32.SetThreadDesktop.argtypes = [wintypes.HANDLE]
user32.SetThreadDesktop.restype = wintypes.BOOL
user32.CloseDesktop.argtypes = [wintypes.HANDLE]
user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.MapVirtualKeyW.argtypes = [wintypes.UINT, wintypes.UINT]
user32.MapVirtualKeyW.restype = wintypes.UINT
user32.GetForegroundWindow.restype = wintypes.HWND
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.GetCursorPos.argtypes = [ctypes.POINTER(wintypes.POINT)]
user32.EnumDesktopWindows.argtypes = [
    wintypes.HANDLE,
    ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM),
    wintypes.LPARAM,
]
user32.EnumDesktopWindows.restype = wintypes.BOOL
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.PrintWindow.argtypes = [wintypes.HWND, wintypes.HDC, wintypes.UINT]
user32.PrintWindow.restype = wintypes.BOOL
user32.GetWindowDC.argtypes = [wintypes.HWND]
user32.GetWindowDC.restype = wintypes.HDC
user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
kernel32.CreateProcessW.argtypes = [
    wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.LPVOID, wintypes.LPVOID,
    wintypes.BOOL, wintypes.DWORD, wintypes.LPVOID, wintypes.LPCWSTR,
    ctypes.POINTER(STARTUPINFOW), ctypes.POINTER(PROCESS_INFORMATION),
]
kernel32.CreateProcessW.restype = wintypes.BOOL

WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)


def key_vk(vk: int, up: bool = False) -> INPUT:
    scan = user32.MapVirtualKeyW(vk, 0)
    return INPUT(type=INPUT_KEYBOARD, union=INPUTUNION(
        ki=KEYBDINPUT(wVk=vk, wScan=scan, dwFlags=KEYEVENTF_KEYUP if up else 0,
                      time=0, dwExtraInfo=0)))


def mouse_at(x: int, y: int, flags: int) -> INPUT:
    ox = user32.GetSystemMetrics(76)
    oy = user32.GetSystemMetrics(77)
    w = user32.GetSystemMetrics(78)
    h = user32.GetSystemMetrics(79)
    return INPUT(type=INPUT_MOUSE, union=INPUTUNION(
        mi=MOUSEINPUT(dx=int(round((x - ox) * 65535 / w)),
                      dy=int(round((y - oy) * 65535 / h)), mouseData=0,
                      dwFlags=flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                      time=0, dwExtraInfo=0)))


def read_log(path: str) -> list[dict]:
    if not os.path.exists(path):
        return []
    with open(path, "r", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def snapshot_user_desktop() -> dict:
    hwnd = user32.GetForegroundWindow()
    title = ctypes.create_unicode_buffer(256)
    user32.GetWindowTextW(hwnd, title, 256)
    point = wintypes.POINT()
    user32.GetCursorPos(ctypes.byref(point))
    return {"foreground_title": title.value, "cursor": [point.x, point.y]}


def main() -> int:
    log_path = os.path.join(tempfile.gettempdir(), f"aad-hidden-{os.getpid()}.jsonl")
    if os.path.exists(log_path):
        os.remove(log_path)

    result: dict = {"desktop_name": DESKTOP_NAME, "log": log_path}

    hidden = user32.CreateDesktopW(DESKTOP_NAME, None, None, 0, GENERIC_ALL, None)
    if not hidden:
        result["error"] = {"what": "CreateDesktopW", "code": ctypes.get_last_error()}
        print(json.dumps(result, indent=2))
        return 1
    result["created_desktop"] = True

    title = f"AAD Hidden Probe {os.getpid()}"
    startup = STARTUPINFOW()
    startup.cb = ctypes.sizeof(STARTUPINFOW)
    startup.lpDesktop = DESKTOP_NAME
    info = PROCESS_INFORMATION()
    command = f'"{sys.executable}" "{PROBE}" --title "{title}" --log "{log_path}"'
    if not kernel32.CreateProcessW(None, ctypes.create_unicode_buffer(command),
                                   None, None, False, 0, None, None,
                                   ctypes.byref(startup), ctypes.byref(info)):
        result["error"] = {"what": "CreateProcessW", "code": ctypes.get_last_error()}
        user32.CloseDesktop(hidden)
        print(json.dumps(result, indent=2))
        return 1
    result["probe_pid"] = info.dwProcessId

    # The probe writes its readiness record to stdout, which is not piped here
    # (CreateProcessW without redirection), so wait for the log file's window
    # to appear via EnumDesktopWindows instead.
    target = {"hwnd": None}
    deadline = time.time() + 20
    while time.time() < deadline and target["hwnd"] is None:
        found: list[int] = []

        @WNDENUMPROC
        def collect(hwnd, _lparam):
            buffer = ctypes.create_unicode_buffer(256)
            user32.GetWindowTextW(hwnd, buffer, 256)
            if buffer.value == title:
                found.append(int(hwnd))
            return True

        user32.EnumDesktopWindows(hidden, collect, 0)
        if found:
            target["hwnd"] = found[0]
            break
        time.sleep(0.3)

    if target["hwnd"] is None:
        result["error"] = {"what": "probe window never appeared on hidden desktop"}
        user32.CloseDesktop(hidden)
        print(json.dumps(result, indent=2))
        return 1
    result["hidden_window"] = target["hwnd"]

    before = snapshot_user_desktop()
    outcomes: queue.Queue = queue.Queue()

    def worker(attach_desktop: bool, label: str) -> None:
        """Send the same input with and without SetThreadDesktop."""
        attached = None
        if attach_desktop:
            attached = bool(user32.SetThreadDesktop(hidden))
            if not attached:
                outcomes.put({"label": label, "set_thread_desktop": False,
                              "error": ctypes.get_last_error()})
                return
        start = len(read_log(log_path))
        events = [key_vk(0x42), key_vk(0x42, up=True)]      # 'B'
        array = (INPUT * len(events))(*events)
        sent = user32.SendInput(len(events), array, ctypes.sizeof(INPUT))
        error = ctypes.get_last_error()
        time.sleep(1.2)
        entries = read_log(log_path)[start:]
        channels: dict[str, int] = {}
        for entry in entries:
            channels[entry["channel"]] = channels.get(entry["channel"], 0) + 1
        outcomes.put({
            "label": label, "set_thread_desktop": attached,
            "sent": sent, "requested": len(events), "last_error": error,
            "channels": channels, "entries": entries,
        })

    # Control first: no SetThreadDesktop. Runs on the user's desktop, so any
    # keystroke it produces lands in whatever the user has focused -- which is
    # exactly the behaviour being measured.
    control = threading.Thread(target=worker, args=(False, "no_setthreaddesktop"))
    control.start()
    control.join(timeout=15)
    result["control"] = outcomes.get()

    # Then the real question.
    attached_thread = threading.Thread(target=worker, args=(True, "with_setthreaddesktop"))
    attached_thread.start()
    attached_thread.join(timeout=15)
    result["attached"] = outcomes.get()

    # Mouse on the hidden desktop, too: does a click reach a window nobody can see?
    def mouse_worker() -> None:
        if not user32.SetThreadDesktop(hidden):
            outcomes.put({"label": "mouse", "set_thread_desktop": False,
                          "error": ctypes.get_last_error()})
            return
        rect = wintypes.RECT()
        user32.GetWindowRect(wintypes.HWND(target["hwnd"]), ctypes.byref(rect))
        cx = (rect.left + rect.right) // 2
        cy = rect.top + 40
        start = len(read_log(log_path))
        events = [mouse_at(cx, cy, MOUSEEVENTF_MOVE),
                  mouse_at(cx, cy, MOUSEEVENTF_LEFTDOWN),
                  mouse_at(cx, cy, MOUSEEVENTF_LEFTUP)]
        array = (INPUT * len(events))(*events)
        sent = user32.SendInput(len(events), array, ctypes.sizeof(INPUT))
        time.sleep(1.2)
        entries = read_log(log_path)[start:]
        channels: dict[str, int] = {}
        for entry in entries:
            channels[entry["channel"]] = channels.get(entry["channel"], 0) + 1
        outcomes.put({"label": "mouse", "set_thread_desktop": True, "sent": sent,
                      "requested": len(events), "window_rect": [rect.left, rect.top,
                                                                rect.right, rect.bottom],
                      "channels": channels, "entries": entries})

    mouse_thread = threading.Thread(target=mouse_worker)
    mouse_thread.start()
    mouse_thread.join(timeout=15)
    result["mouse"] = outcomes.get()

    after = snapshot_user_desktop()
    result["user_desktop"] = {
        "before": before, "after": after,
        "foreground_unchanged": before["foreground_title"] == after["foreground_title"],
        "cursor_unchanged": before["cursor"] == after["cursor"],
    }

    # Verification 3, cheaply: can the hidden window be captured at all?
    def capture_worker() -> None:
        if not user32.SetThreadDesktop(hidden):
            outcomes.put({"label": "capture", "ok": False})
            return
        hwnd = wintypes.HWND(target["hwnd"])
        rect = wintypes.RECT()
        user32.GetWindowRect(hwnd, ctypes.byref(rect))
        width = rect.right - rect.left
        height = rect.bottom - rect.top
        window_dc = user32.GetWindowDC(hwnd)
        memory_dc = gdi32.CreateCompatibleDC(window_dc)
        bitmap = gdi32.CreateCompatibleBitmap(window_dc, width, height)
        gdi32.SelectObject(memory_dc, bitmap)
        ok = bool(user32.PrintWindow(hwnd, memory_dc, 2))  # PW_RENDERFULLCONTENT
        # Sample a few pixels: an all-black capture means "returned true but
        # drew nothing", which is the failure mode that matters here.
        samples = []
        for fx, fy in ((0.5, 0.1), (0.5, 0.3), (0.2, 0.25), (0.5, 0.5)):
            samples.append(int(gdi32.GetPixel(memory_dc, int(width * fx), int(height * fy))))
        gdi32.DeleteObject(bitmap)
        gdi32.DeleteDC(memory_dc)
        user32.ReleaseDC(hwnd, window_dc)
        outcomes.put({"label": "capture", "print_window": ok,
                      "size": [width, height], "pixel_samples": samples,
                      "distinct_pixels": len(set(samples)),
                      "all_black": all(value == 0 for value in samples)})

    capture_thread = threading.Thread(target=capture_worker)
    capture_thread.start()
    capture_thread.join(timeout=15)
    result["capture"] = outcomes.get()

    kernel32.TerminateProcess(info.hProcess, 0)
    kernel32.CloseHandle(info.hProcess)
    kernel32.CloseHandle(info.hThread)
    user32.CloseDesktop(hidden)

    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
