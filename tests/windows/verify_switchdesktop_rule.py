#!/usr/bin/env python3
"""Confirm the rule: does SendInput follow the INPUT desktop, not the thread's?

diagnose_hidden_desktop_input.py showed SendInput failing with ACCESS_DENIED on
a hidden desktop from three different angles, including a process born there.
The one thing all three had in common: the hidden desktop was never the *input*
desktop (OpenInputDesktop reported "Default" throughout).

This test flips exactly that one variable. SwitchDesktop makes the hidden
desktop the input desktop for a couple of seconds; if SendInput then succeeds
and the probe records the keystroke, the rule is established and the earlier
failures are explained. Everything else is held constant.

The screen goes blank briefly while the hidden desktop is shown -- that is what
SwitchDesktop does, and it is also why this cannot be the basis of a
"don't disturb the user" feature.
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
import json
import os
import sys
import tempfile
import time


HERE = os.path.dirname(os.path.abspath(__file__))
PROBE = os.path.join(HERE, "input_channel_probe.py")

DESKTOP_ALL = 0x01FF
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
ULONG_PTR = ctypes.c_size_t


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                ("dwExtraInfo", ULONG_PTR)]


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG),
                ("mouseData", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


class INPUTUNION(ctypes.Union):
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("union", INPUTUNION)]


class STARTUPINFOW(ctypes.Structure):
    _fields_ = [("cb", wintypes.DWORD), ("lpReserved", wintypes.LPWSTR),
                ("lpDesktop", wintypes.LPWSTR), ("lpTitle", wintypes.LPWSTR),
                ("dwX", wintypes.DWORD), ("dwY", wintypes.DWORD),
                ("dwXSize", wintypes.DWORD), ("dwYSize", wintypes.DWORD),
                ("dwXCountChars", wintypes.DWORD), ("dwYCountChars", wintypes.DWORD),
                ("dwFillAttribute", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
                ("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
                ("lpReserved2", ctypes.POINTER(ctypes.c_byte)),
                ("hStdInput", wintypes.HANDLE), ("hStdOutput", wintypes.HANDLE),
                ("hStdError", wintypes.HANDLE)]


class PROCESS_INFORMATION(ctypes.Structure):
    _fields_ = [("hProcess", wintypes.HANDLE), ("hThread", wintypes.HANDLE),
                ("dwProcessId", wintypes.DWORD), ("dwThreadId", wintypes.DWORD)]


user32.CreateDesktopW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.LPVOID,
                                  wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID]
user32.CreateDesktopW.restype = wintypes.HANDLE
user32.OpenDesktopW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
user32.OpenDesktopW.restype = wintypes.HANDLE
user32.OpenInputDesktop.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
user32.OpenInputDesktop.restype = wintypes.HANDLE
user32.SwitchDesktop.argtypes = [wintypes.HANDLE]
user32.SwitchDesktop.restype = wintypes.BOOL
user32.SetThreadDesktop.argtypes = [wintypes.HANDLE]
user32.SetThreadDesktop.restype = wintypes.BOOL
user32.GetThreadDesktop.argtypes = [wintypes.DWORD]
user32.GetThreadDesktop.restype = wintypes.HANDLE
user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.MapVirtualKeyW.argtypes = [wintypes.UINT, wintypes.UINT]
user32.MapVirtualKeyW.restype = wintypes.UINT
user32.GetUserObjectInformationW.argtypes = [wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID,
                                             wintypes.DWORD, ctypes.POINTER(wintypes.DWORD)]
user32.EnumDesktopWindows.argtypes = [
    wintypes.HANDLE, ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM),
    wintypes.LPARAM]
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
kernel32.CreateProcessW.argtypes = [
    wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.LPVOID, wintypes.LPVOID,
    wintypes.BOOL, wintypes.DWORD, wintypes.LPVOID, wintypes.LPCWSTR,
    ctypes.POINTER(STARTUPINFOW), ctypes.POINTER(PROCESS_INFORMATION)]
kernel32.CreateProcessW.restype = wintypes.BOOL
kernel32.GetCurrentThreadId.restype = wintypes.DWORD

WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
UOI_NAME = 2


def desktop_name(handle) -> str:
    buffer = ctypes.create_unicode_buffer(256)
    needed = wintypes.DWORD()
    if user32.GetUserObjectInformationW(handle, UOI_NAME, buffer, 512, ctypes.byref(needed)):
        return buffer.value
    return f"<error {ctypes.get_last_error()}>"


def key(vk: int, up: bool = False) -> INPUT:
    return INPUT(type=INPUT_KEYBOARD, union=INPUTUNION(
        ki=KEYBDINPUT(wVk=vk, wScan=user32.MapVirtualKeyW(vk, 0),
                      dwFlags=KEYEVENTF_KEYUP if up else 0, time=0, dwExtraInfo=0)))


def read_log(path: str) -> list[dict]:
    if not os.path.exists(path):
        return []
    with open(path, "r", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def main() -> int:
    name = "AadSwitchTest"
    log_path = os.path.join(tempfile.gettempdir(), f"aad-switch-{os.getpid()}.jsonl")
    if os.path.exists(log_path):
        os.remove(log_path)
    results: dict = {}

    hidden = user32.CreateDesktopW(name, None, None, 0, DESKTOP_ALL, None)
    if not hidden:
        print(json.dumps({"error": "CreateDesktopW", "code": ctypes.get_last_error()}))
        return 1

    title = f"AAD Switch Probe {os.getpid()}"
    startup = STARTUPINFOW()
    startup.cb = ctypes.sizeof(STARTUPINFOW)
    startup.lpDesktop = name
    info = PROCESS_INFORMATION()
    command = f'"{sys.executable}" "{PROBE}" --title "{title}" --log "{log_path}"'
    if not kernel32.CreateProcessW(None, ctypes.create_unicode_buffer(command),
                                   None, None, False, 0, None, None,
                                   ctypes.byref(startup), ctypes.byref(info)):
        print(json.dumps({"error": "CreateProcessW", "code": ctypes.get_last_error()}))
        user32.CloseDesktop(hidden)
        return 1

    # Wait for the probe's window to exist on the hidden desktop.
    deadline = time.time() + 20
    found_hwnd = None
    while time.time() < deadline and found_hwnd is None:
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
            found_hwnd = found[0]
        else:
            time.sleep(0.3)
    results["probe_window"] = found_hwnd
    if found_hwnd is None:
        print(json.dumps({"error": "probe window never appeared"}))
        user32.CloseDesktop(hidden)
        return 1

    original_input = user32.OpenInputDesktop(0, False, DESKTOP_ALL)
    results["input_desktop_before"] = desktop_name(original_input)

    original_thread = user32.GetThreadDesktop(kernel32.GetCurrentThreadId())
    user32.SetThreadDesktop(hidden)

    # -- A: hidden desktop is NOT the input desktop (reproduce the failure) --
    start = len(read_log(log_path))
    array = (INPUT * 2)(key(0x42), key(0x42, up=True))
    ctypes.set_last_error(0)
    sent = user32.SendInput(2, array, ctypes.sizeof(INPUT))
    error = ctypes.get_last_error()
    time.sleep(0.8)
    results["not_input_desktop"] = {
        "sent": sent, "requested": 2, "last_error": error,
        "probe_entries": len(read_log(log_path)) - start}

    # -- B: make it the input desktop, change nothing else -------------------
    switched = bool(user32.SwitchDesktop(hidden))
    switch_error = 0 if switched else ctypes.get_last_error()
    time.sleep(0.6)
    now_input = user32.OpenInputDesktop(0, False, DESKTOP_ALL)
    results["switch"] = {"ok": switched, "error": switch_error,
                         "input_desktop_now": desktop_name(now_input) if now_input else "?"}

    start = len(read_log(log_path))
    array = (INPUT * 2)(key(0x42), key(0x42, up=True))
    ctypes.set_last_error(0)
    sent = user32.SendInput(2, array, ctypes.sizeof(INPUT))
    error = ctypes.get_last_error()
    time.sleep(1.0)
    entries = read_log(log_path)[start:]
    channels: dict[str, int] = {}
    for entry in entries:
        channels[entry["channel"]] = channels.get(entry["channel"], 0) + 1
    results["as_input_desktop"] = {
        "sent": sent, "requested": 2, "last_error": error,
        "channels": channels, "entries": entries}

    # Always switch back, even if something above went wrong.
    if original_input:
        user32.SwitchDesktop(original_input)
    else:
        back = user32.OpenDesktopW("Default", 0, False, DESKTOP_ALL)
        if back:
            user32.SwitchDesktop(back)
            user32.CloseDesktop(back)
    time.sleep(0.5)
    restored = user32.OpenInputDesktop(0, False, DESKTOP_ALL)
    results["input_desktop_restored"] = desktop_name(restored) if restored else "?"

    user32.SetThreadDesktop(original_thread)
    kernel32.TerminateProcess(info.hProcess, 0)
    kernel32.CloseHandle(info.hProcess)
    kernel32.CloseHandle(info.hThread)
    user32.CloseDesktop(hidden)

    print(json.dumps(results, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
