#!/usr/bin/env python3
"""Why did SendInput fail on the hidden desktop? Narrow down the cause.

verify_hidden_desktop.py found SetThreadDesktop succeeding while SendInput
returned 0 with ERROR_ACCESS_DENIED. Several explanations are possible and
they lead to different conclusions, so test them apart:

  1. The desktop handle lacks the rights SendInput needs -> ask for explicit
     DESKTOP_* rights instead of GENERIC_ALL, and re-open by name.
  2. SendInput only ever targets the *input* desktop (the one currently shown),
     regardless of the calling thread -> compare against OpenInputDesktop.
  3. Something about this process (existing windows/hooks, Python host) taints
     the thread -> run the same test from a freshly spawned process whose
     initial desktop is the hidden one, so no SetThreadDesktop is needed at all.

Case 3 is the decisive one: if a process *started on* the hidden desktop can
SendInput into it, the capability exists and the earlier failure was about how
the thread got there.
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

DESKTOP_READOBJECTS = 0x0001
DESKTOP_CREATEWINDOW = 0x0002
DESKTOP_CREATEMENU = 0x0004
DESKTOP_HOOKCONTROL = 0x0008
DESKTOP_JOURNALRECORD = 0x0010
DESKTOP_JOURNALPLAYBACK = 0x0020
DESKTOP_ENUMERATE = 0x0040
DESKTOP_WRITEOBJECTS = 0x0080
DESKTOP_SWITCHDESKTOP = 0x0100
DESKTOP_ALL = (
    DESKTOP_READOBJECTS | DESKTOP_CREATEWINDOW | DESKTOP_CREATEMENU
    | DESKTOP_HOOKCONTROL | DESKTOP_JOURNALRECORD | DESKTOP_JOURNALPLAYBACK
    | DESKTOP_ENUMERATE | DESKTOP_WRITEOBJECTS | DESKTOP_SWITCHDESKTOP
)

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


user32.CreateDesktopW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.LPVOID,
                                  wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID]
user32.CreateDesktopW.restype = wintypes.HANDLE
user32.OpenDesktopW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
user32.OpenDesktopW.restype = wintypes.HANDLE
user32.OpenInputDesktop.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
user32.OpenInputDesktop.restype = wintypes.HANDLE
user32.GetThreadDesktop.argtypes = [wintypes.DWORD]
user32.GetThreadDesktop.restype = wintypes.HANDLE
user32.SetThreadDesktop.argtypes = [wintypes.HANDLE]
user32.SetThreadDesktop.restype = wintypes.BOOL
user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.MapVirtualKeyW.argtypes = [wintypes.UINT, wintypes.UINT]
user32.MapVirtualKeyW.restype = wintypes.UINT
user32.GetUserObjectInformationW.argtypes = [wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID,
                                             wintypes.DWORD, ctypes.POINTER(wintypes.DWORD)]
user32.GetUserObjectInformationW.restype = wintypes.BOOL
kernel32.GetCurrentThreadId.restype = wintypes.DWORD

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


def try_send(label: str) -> dict:
    events = [key(0x42), key(0x42, up=True)]
    array = (INPUT * 2)(*events)
    ctypes.set_last_error(0)
    sent = user32.SendInput(2, array, ctypes.sizeof(INPUT))
    return {"case": label, "sent": sent, "last_error": ctypes.get_last_error(),
            "thread_desktop": desktop_name(user32.GetThreadDesktop(kernel32.GetCurrentThreadId()))}


def child_mode(name: str, log: str) -> int:
    """Run inside a process whose *initial* desktop is the hidden one."""
    findings = {
        "role": "child",
        "initial_thread_desktop": desktop_name(
            user32.GetThreadDesktop(kernel32.GetCurrentThreadId())),
    }
    findings["send_from_native_desktop"] = try_send("child_native_desktop")
    input_desktop = user32.OpenInputDesktop(0, False, DESKTOP_ALL)
    findings["input_desktop_name"] = desktop_name(input_desktop) if input_desktop else \
        f"<open failed {ctypes.get_last_error()}>"
    with open(log, "w", encoding="utf-8") as handle:
        json.dump(findings, handle, indent=2, sort_keys=True)
    return 0


def main() -> int:
    if len(sys.argv) > 2 and sys.argv[1] == "--child":
        return child_mode("child", sys.argv[2])

    name = "AadDiagHidden"
    results: dict = {"desktop_name": name}

    # -- case 1: explicit DESKTOP_* rights rather than GENERIC_ALL -----------
    created = user32.CreateDesktopW(name, None, None, 0, DESKTOP_ALL, None)
    results["create_with_desktop_all"] = {
        "ok": bool(created), "error": 0 if created else ctypes.get_last_error()}
    if not created:
        print(json.dumps(results, indent=2))
        return 1

    reopened = user32.OpenDesktopW(name, 0, False, DESKTOP_ALL)
    results["reopen_by_name"] = {"ok": bool(reopened),
                                 "error": 0 if reopened else ctypes.get_last_error()}

    original = user32.GetThreadDesktop(kernel32.GetCurrentThreadId())
    results["original_desktop"] = desktop_name(original)

    # -- case 2: what is the *input* desktop, and can we send while on it? ---
    input_desktop = user32.OpenInputDesktop(0, False, DESKTOP_ALL)
    results["input_desktop"] = desktop_name(input_desktop) if input_desktop else \
        f"<open failed {ctypes.get_last_error()}>"

    results["baseline_send_on_default"] = try_send("default_desktop")

    switched = bool(user32.SetThreadDesktop(created))
    results["set_thread_desktop"] = {"ok": switched,
                                     "error": 0 if switched else ctypes.get_last_error()}
    if switched:
        results["send_on_hidden_desktop"] = try_send("hidden_desktop_via_setthreaddesktop")
        if reopened:
            user32.SetThreadDesktop(reopened)
            results["send_on_hidden_reopened"] = try_send("hidden_desktop_reopened_handle")
        user32.SetThreadDesktop(original)
        results["restored_desktop"] = desktop_name(
            user32.GetThreadDesktop(kernel32.GetCurrentThreadId()))

    # -- case 3 (decisive): a process born on the hidden desktop ------------
    child_log = os.path.join(tempfile.gettempdir(), f"aad-diag-child-{os.getpid()}.json")

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

    kernel32.CreateProcessW.argtypes = [
        wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.LPVOID, wintypes.LPVOID,
        wintypes.BOOL, wintypes.DWORD, wintypes.LPVOID, wintypes.LPCWSTR,
        ctypes.POINTER(STARTUPINFOW), ctypes.POINTER(PROCESS_INFORMATION)]
    kernel32.CreateProcessW.restype = wintypes.BOOL

    startup = STARTUPINFOW()
    startup.cb = ctypes.sizeof(STARTUPINFOW)
    startup.lpDesktop = name
    info = PROCESS_INFORMATION()
    command = f'"{sys.executable}" "{os.path.abspath(__file__)}" --child "{child_log}"'
    spawned = bool(kernel32.CreateProcessW(None, ctypes.create_unicode_buffer(command),
                                           None, None, False, 0, None, None,
                                           ctypes.byref(startup), ctypes.byref(info)))
    results["spawn_child_on_hidden"] = {"ok": spawned,
                                        "error": 0 if spawned else ctypes.get_last_error()}
    if spawned:
        kernel32.WaitForSingleObject(info.hProcess, 20000)
        kernel32.CloseHandle(info.hProcess)
        kernel32.CloseHandle(info.hThread)
        if os.path.exists(child_log):
            with open(child_log, "r", encoding="utf-8") as handle:
                results["child"] = json.load(handle)
            os.remove(child_log)
        else:
            results["child"] = {"error": "child produced no report"}

    user32.CloseDesktop(created)
    if reopened:
        user32.CloseDesktop(reopened)

    print(json.dumps(results, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
