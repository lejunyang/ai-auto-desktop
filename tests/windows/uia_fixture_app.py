#!/usr/bin/env python3
"""Small native Win32 application used by the real UIA integration test.

The fixture intentionally uses only ``ctypes`` and stock Win32 controls so the
Windows runner tests the operating system's built-in UI Automation providers.
Stdout is reserved for a single JSON readiness record.
"""

from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
import json
import os
import sys


EDIT_ID = 1001
INVOKE_ID = 1002
STATUS_ID = 1003
DUPLICATE_ONE_ID = 1004
DUPLICATE_TWO_ID = 1005
POINTER_ID = 1006

INITIAL_EDIT_VALUE = "Draft"
INVOKE_BUTTON_NAME = "Apply fixture value"
DUPLICATE_BUTTON_NAME = "Duplicate action"
POINTER_BUTTON_NAME = "Pointer click target"
INITIAL_STATUS = "Status: idle"
INVOKED_STATUS = "Status: invoked"
DUPLICATE_STATUS = "Status: duplicate invoked"
POINTER_STATUS = "Status: pointer clicked"


def main() -> int:
    if sys.platform != "win32":
        print("the UIA fixture requires Windows", file=sys.stderr)
        return 2

    parser = argparse.ArgumentParser()
    parser.add_argument("--title", required=True)
    args = parser.parse_args()

    user32 = ctypes.WinDLL("user32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    lresult = ctypes.c_ssize_t
    wparam = ctypes.c_size_t
    lparam = ctypes.c_ssize_t
    wndproc_type = ctypes.WINFUNCTYPE(
        lresult, wintypes.HWND, wintypes.UINT, wparam, lparam
    )

    class WNDCLASSEXW(ctypes.Structure):
        _fields_ = [
            ("cbSize", wintypes.UINT),
            ("style", wintypes.UINT),
            ("lpfnWndProc", wndproc_type),
            ("cbClsExtra", ctypes.c_int),
            ("cbWndExtra", ctypes.c_int),
            ("hInstance", wintypes.HINSTANCE),
            ("hIcon", wintypes.HICON),
            ("hCursor", wintypes.HANDLE),
            ("hbrBackground", wintypes.HBRUSH),
            ("lpszMenuName", wintypes.LPCWSTR),
            ("lpszClassName", wintypes.LPCWSTR),
            ("hIconSm", wintypes.HICON),
        ]

    user32.DefWindowProcW.argtypes = [wintypes.HWND, wintypes.UINT, wparam, lparam]
    user32.DefWindowProcW.restype = lresult
    user32.RegisterClassExW.argtypes = [ctypes.POINTER(WNDCLASSEXW)]
    user32.RegisterClassExW.restype = wintypes.ATOM
    user32.CreateWindowExW.argtypes = [
        wintypes.DWORD,
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.DWORD,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        wintypes.HWND,
        wintypes.HMENU,
        wintypes.HINSTANCE,
        wintypes.LPVOID,
    ]
    user32.CreateWindowExW.restype = wintypes.HWND
    user32.ShowWindow.argtypes = [wintypes.HWND, ctypes.c_int]
    user32.ShowWindow.restype = wintypes.BOOL
    user32.UpdateWindow.argtypes = [wintypes.HWND]
    user32.UpdateWindow.restype = wintypes.BOOL
    user32.SetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPCWSTR]
    user32.SetWindowTextW.restype = wintypes.BOOL
    user32.PostQuitMessage.argtypes = [ctypes.c_int]
    user32.GetMessageW.argtypes = [
        ctypes.POINTER(wintypes.MSG),
        wintypes.HWND,
        wintypes.UINT,
        wintypes.UINT,
    ]
    user32.GetMessageW.restype = ctypes.c_int
    user32.TranslateMessage.argtypes = [ctypes.POINTER(wintypes.MSG)]
    user32.TranslateMessage.restype = wintypes.BOOL
    user32.DispatchMessageW.argtypes = [ctypes.POINTER(wintypes.MSG)]
    user32.DispatchMessageW.restype = lresult
    kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    kernel32.GetModuleHandleW.restype = wintypes.HMODULE

    WM_DESTROY = 0x0002
    WM_COMMAND = 0x0111
    BN_CLICKED = 0
    WS_OVERLAPPEDWINDOW = 0x00CF0000
    WS_VISIBLE = 0x10000000
    WS_CHILD = 0x40000000
    WS_TABSTOP = 0x00010000
    WS_BORDER = 0x00800000
    ES_AUTOHSCROLL = 0x0080
    SW_SHOW = 5
    COLOR_WINDOW = 5

    status_handle: wintypes.HWND | None = None

    @wndproc_type
    def window_proc(
        hwnd: wintypes.HWND, message: int, raw_wparam: int, raw_lparam: int
    ) -> int:
        if message == WM_COMMAND:
            control_id = int(raw_wparam) & 0xFFFF
            notification = (int(raw_wparam) >> 16) & 0xFFFF
            if notification == BN_CLICKED and status_handle:
                if control_id == INVOKE_ID:
                    user32.SetWindowTextW(status_handle, INVOKED_STATUS)
                    return 0
                if control_id in {DUPLICATE_ONE_ID, DUPLICATE_TWO_ID}:
                    user32.SetWindowTextW(status_handle, DUPLICATE_STATUS)
                    return 0
                if control_id == POINTER_ID:
                    user32.SetWindowTextW(status_handle, POINTER_STATUS)
                    return 0
        if message == WM_DESTROY:
            user32.PostQuitMessage(0)
            return 0
        return int(user32.DefWindowProcW(hwnd, message, raw_wparam, raw_lparam))

    instance = kernel32.GetModuleHandleW(None)
    class_name = f"AiAutoDesktopUIAFixture_{os.getpid()}"
    window_class = WNDCLASSEXW(
        cbSize=ctypes.sizeof(WNDCLASSEXW),
        style=0,
        lpfnWndProc=window_proc,
        cbClsExtra=0,
        cbWndExtra=0,
        hInstance=instance,
        hIcon=None,
        hCursor=None,
        hbrBackground=wintypes.HBRUSH(COLOR_WINDOW + 1),
        lpszMenuName=None,
        lpszClassName=class_name,
        hIconSm=None,
    )
    if not user32.RegisterClassExW(ctypes.byref(window_class)):
        raise ctypes.WinError(ctypes.get_last_error())

    hwnd = user32.CreateWindowExW(
        0,
        class_name,
        args.title,
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        -2147483648,  # CW_USEDEFAULT as a signed int
        -2147483648,
        560,
        300,
        None,
        None,
        instance,
        None,
    )
    if not hwnd:
        raise ctypes.WinError(ctypes.get_last_error())

    def create_control(
        class_name_value: str,
        text: str,
        style: int,
        x: int,
        y: int,
        width: int,
        height: int,
        control_id: int,
    ) -> wintypes.HWND:
        control = user32.CreateWindowExW(
            0,
            class_name_value,
            text,
            style | WS_CHILD | WS_VISIBLE,
            x,
            y,
            width,
            height,
            hwnd,
            wintypes.HMENU(control_id),
            instance,
            None,
        )
        if not control:
            raise ctypes.WinError(ctypes.get_last_error())
        return control

    edit_handle = create_control(
        "EDIT",
        INITIAL_EDIT_VALUE,
        WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
        24,
        24,
        300,
        28,
        EDIT_ID,
    )
    invoke_handle = create_control(
        "BUTTON", INVOKE_BUTTON_NAME, WS_TABSTOP, 24, 68, 180, 32, INVOKE_ID
    )
    status_handle = create_control(
        "STATIC", INITIAL_STATUS, 0, 24, 116, 300, 28, STATUS_ID
    )
    duplicate_one_handle = create_control(
        "BUTTON", DUPLICATE_BUTTON_NAME, WS_TABSTOP, 24, 164, 180, 32, DUPLICATE_ONE_ID
    )
    duplicate_two_handle = create_control(
        "BUTTON", DUPLICATE_BUTTON_NAME, WS_TABSTOP, 220, 164, 180, 32, DUPLICATE_TWO_ID
    )
    pointer_handle = create_control(
        "BUTTON", POINTER_BUTTON_NAME, WS_TABSTOP, 24, 212, 180, 32, POINTER_ID
    )

    user32.ShowWindow(hwnd, SW_SHOW)
    user32.UpdateWindow(hwnd)
    print(
        json.dumps(
            {
                "ready": True,
                "pid": os.getpid(),
                "title": args.title,
                "window_handle": int(hwnd),
                "controls": {
                    "edit": int(edit_handle),
                    "invoke": int(invoke_handle),
                    "status": int(status_handle),
                    "duplicate_one": int(duplicate_one_handle),
                    "duplicate_two": int(duplicate_two_handle),
                    "pointer": int(pointer_handle),
                },
            },
            sort_keys=True,
        ),
        flush=True,
    )

    message = wintypes.MSG()
    while True:
        result = user32.GetMessageW(ctypes.byref(message), None, 0, 0)
        if result == 0:
            return int(message.wParam)
        if result == -1:
            raise ctypes.WinError(ctypes.get_last_error())
        user32.TranslateMessage(ctypes.byref(message))
        user32.DispatchMessageW(ctypes.byref(message))


if __name__ == "__main__":
    raise SystemExit(main())
