#!/usr/bin/env python3
"""Do windows on a hidden desktop actually render? And do modifiers leak?

Two verifications from docs/research/input-synthesis-and-background-execution.md:

  #3  PrintWindow on a hidden desktop returned true earlier but produced a
      single flat colour. "Returned true" is not evidence -- compare a capture
      of the same program on the visible desktop against one on the hidden
      desktop, using identical sampling, and count distinct colours.

  #6  If a batch that presses Ctrl is refused midway (or a key-up is never
      sent), does Ctrl stay latched? That failure escapes the scope of one
      action and poisons everything the user types afterwards, so the answer
      decides whether a reset step is mandatory.

The modifier test deliberately leaves Ctrl down, measures, then always
releases it in a finally block.
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

DESKTOP_ALL = 0x01FF
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
VK_CONTROL = 0x11
VK_SHIFT = 0x10
VK_MENU = 0x12

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
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
user32.SetThreadDesktop.argtypes = [wintypes.HANDLE]
user32.GetThreadDesktop.argtypes = [wintypes.DWORD]
user32.GetThreadDesktop.restype = wintypes.HANDLE
user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT
user32.MapVirtualKeyW.argtypes = [wintypes.UINT, wintypes.UINT]
user32.MapVirtualKeyW.restype = wintypes.UINT
user32.GetAsyncKeyState.argtypes = [ctypes.c_int]
user32.GetAsyncKeyState.restype = ctypes.c_short
user32.GetKeyState.argtypes = [ctypes.c_int]
user32.GetKeyState.restype = ctypes.c_short
user32.PrintWindow.argtypes = [wintypes.HWND, wintypes.HDC, wintypes.UINT]
user32.PrintWindow.restype = wintypes.BOOL
user32.GetWindowDC.argtypes = [wintypes.HWND]
user32.GetWindowDC.restype = wintypes.HDC
user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.EnumDesktopWindows.argtypes = [
    wintypes.HANDLE, ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM),
    wintypes.LPARAM]
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
gdi32.CreateCompatibleDC.argtypes = [wintypes.HDC]
gdi32.CreateCompatibleDC.restype = wintypes.HDC
gdi32.CreateCompatibleBitmap.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int]
gdi32.CreateCompatibleBitmap.restype = wintypes.HBITMAP
gdi32.SelectObject.argtypes = [wintypes.HDC, wintypes.HGDIOBJ]
gdi32.SelectObject.restype = wintypes.HGDIOBJ
# Without explicit restypes ctypes assumes int, and a 64-bit handle overflows
# on the way back out -- the call succeeds and the *cleanup* raises.
gdi32.DeleteDC.argtypes = [wintypes.HDC]
gdi32.DeleteDC.restype = wintypes.BOOL
gdi32.DeleteObject.argtypes = [wintypes.HGDIOBJ]
gdi32.DeleteObject.restype = wintypes.BOOL
gdi32.GetPixel.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int]
gdi32.GetPixel.restype = wintypes.COLORREF
kernel32.CreateProcessW.argtypes = [
    wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.LPVOID, wintypes.LPVOID,
    wintypes.BOOL, wintypes.DWORD, wintypes.LPVOID, wintypes.LPCWSTR,
    ctypes.POINTER(STARTUPINFOW), ctypes.POINTER(PROCESS_INFORMATION)]
kernel32.CreateProcessW.restype = wintypes.BOOL
kernel32.GetCurrentThreadId.restype = wintypes.DWORD

WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)


def key(vk: int, up: bool = False) -> INPUT:
    return INPUT(type=INPUT_KEYBOARD, union=INPUTUNION(
        ki=KEYBDINPUT(wVk=vk, wScan=user32.MapVirtualKeyW(vk, 0),
                      dwFlags=KEYEVENTF_KEYUP if up else 0, time=0, dwExtraInfo=0)))


def capture_stats(hwnd: int) -> dict:
    """Grid-sample a PrintWindow capture and count distinct colours."""
    handle = wintypes.HWND(hwnd)
    rect = wintypes.RECT()
    user32.GetWindowRect(handle, ctypes.byref(rect))
    width = rect.right - rect.left
    height = rect.bottom - rect.top
    if width <= 0 or height <= 0:
        return {"error": "window has no extent"}
    window_dc = user32.GetWindowDC(handle)
    memory_dc = gdi32.CreateCompatibleDC(window_dc)
    bitmap = gdi32.CreateCompatibleBitmap(window_dc, width, height)
    gdi32.SelectObject(memory_dc, bitmap)
    ok = bool(user32.PrintWindow(handle, memory_dc, 2))  # PW_RENDERFULLCONTENT
    colours: dict[int, int] = {}
    for row in range(12):
        for column in range(16):
            x = int(width * (column + 0.5) / 16)
            y = int(height * (row + 0.5) / 12)
            value = int(gdi32.GetPixel(memory_dc, x, y))
            colours[value] = colours.get(value, 0) + 1
    gdi32.DeleteObject(bitmap)
    gdi32.DeleteDC(memory_dc)
    user32.ReleaseDC(handle, window_dc)
    ranked = sorted(colours.items(), key=lambda item: -item[1])[:5]
    return {"print_window": ok, "size": [width, height],
            "samples": sum(colours.values()), "distinct_colours": len(colours),
            "top_colours": [{"bgr": f"0x{value:06X}", "count": count} for value, count in ranked]}


def find_window(desktop, title: str, timeout: float = 20.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        found: list[int] = []

        @WNDENUMPROC
        def collect(hwnd, _lparam):
            buffer = ctypes.create_unicode_buffer(256)
            user32.GetWindowTextW(hwnd, buffer, 256)
            if buffer.value == title:
                found.append(int(hwnd))
            return True

        user32.EnumDesktopWindows(desktop, collect, 0)
        if found:
            return found[0]
        time.sleep(0.3)
    return None


def main() -> int:
    results: dict = {}

    # ---------- verification 3: rendering on a hidden desktop --------------
    visible_log = os.path.join(tempfile.gettempdir(), f"aad-vis-{os.getpid()}.jsonl")
    visible_title = f"AAD Visible {os.getpid()}"
    visible = subprocess.Popen(
        [sys.executable, PROBE, "--title", visible_title, "--log", visible_log],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    ready = json.loads(visible.stdout.readline())
    time.sleep(1.0)
    results["visible_desktop_capture"] = capture_stats(ready["window_handle"])

    hidden_name = "AadRenderTest"
    hidden = user32.CreateDesktopW(hidden_name, None, None, 0, DESKTOP_ALL, None)
    hidden_title = f"AAD Hidden {os.getpid()}"
    hidden_log = os.path.join(tempfile.gettempdir(), f"aad-hid-{os.getpid()}.jsonl")
    startup = STARTUPINFOW()
    startup.cb = ctypes.sizeof(STARTUPINFOW)
    startup.lpDesktop = hidden_name
    info = PROCESS_INFORMATION()
    command = f'"{sys.executable}" "{PROBE}" --title "{hidden_title}" --log "{hidden_log}"'
    kernel32.CreateProcessW(None, ctypes.create_unicode_buffer(command), None, None,
                            False, 0, None, None, ctypes.byref(startup), ctypes.byref(info))
    hidden_hwnd = find_window(hidden, hidden_title)
    results["hidden_window_found"] = hidden_hwnd is not None
    if hidden_hwnd is not None:
        time.sleep(1.0)
        original = user32.GetThreadDesktop(kernel32.GetCurrentThreadId())
        user32.SetThreadDesktop(hidden)
        results["hidden_desktop_capture"] = capture_stats(hidden_hwnd)
        user32.SetThreadDesktop(original)

    visible.terminate()
    kernel32.TerminateProcess(info.hProcess, 0)
    kernel32.CloseHandle(info.hProcess)
    kernel32.CloseHandle(info.hThread)
    user32.CloseDesktop(hidden)

    # ---------- verification 6: does a stuck modifier stay stuck? ----------
    modifier: dict = {}
    try:
        modifier["before"] = {
            "async_ctrl_down": bool(user32.GetAsyncKeyState(VK_CONTROL) & 0x8000),
            "keystate_ctrl_down": bool(user32.GetKeyState(VK_CONTROL) & 0x8000),
        }
        # Press Ctrl and deliberately never release it -- the exact shape of a
        # batch that dies after the modifier went down.
        array = (INPUT * 1)(key(VK_CONTROL))
        modifier["press_sent"] = user32.SendInput(1, array, ctypes.sizeof(INPUT))
        time.sleep(0.5)
        modifier["while_held"] = {
            "async_ctrl_down": bool(user32.GetAsyncKeyState(VK_CONTROL) & 0x8000),
            "keystate_ctrl_down": bool(user32.GetKeyState(VK_CONTROL) & 0x8000),
        }
        time.sleep(2.0)
        modifier["after_2s_no_release"] = {
            "async_ctrl_down": bool(user32.GetAsyncKeyState(VK_CONTROL) & 0x8000),
            "keystate_ctrl_down": bool(user32.GetKeyState(VK_CONTROL) & 0x8000),
        }
        modifier["self_healed"] = not modifier["after_2s_no_release"]["async_ctrl_down"]
    finally:
        # Release unconditionally: leaving Ctrl latched would affect the user's
        # session long after this script exits.
        release = (INPUT * 1)(key(VK_CONTROL, up=True))
        user32.SendInput(1, release, ctypes.sizeof(INPUT))
        time.sleep(0.3)
        modifier["after_explicit_release"] = {
            "async_ctrl_down": bool(user32.GetAsyncKeyState(VK_CONTROL) & 0x8000),
            "keystate_ctrl_down": bool(user32.GetKeyState(VK_CONTROL) & 0x8000),
        }
    results["modifier_leak"] = modifier

    print(json.dumps(results, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
