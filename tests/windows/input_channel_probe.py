#!/usr/bin/env python3
"""Win32 probe that records *which input channel* an event arrived on.

The fixture in ``uia_fixture_app.py`` answers "did the app react". This one
answers a different question: when input is synthesised, which of the three
channels an application can read actually sees it?

    - Raw Input (``WM_INPUT``)  -- what games and DirectInput-style code read
    - Window messages           -- ``WM_KEYDOWN`` / ``WM_CHAR`` / mouse messages
    - Async key state           -- ``GetAsyncKeyState``, polled

Findings are appended to a log file rather than shown on screen, because the
interesting runs happen on a *hidden desktop* where nothing can be read off the
screen. The log is the evidence; stdout only carries one readiness record.
"""

from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
import json
import os
import sys
import threading
import time


WM_INPUT = 0x00FF
WM_KEYDOWN = 0x0100
WM_KEYUP = 0x0101
WM_CHAR = 0x0102
WM_SYSKEYDOWN = 0x0104
WM_LBUTTONDOWN = 0x0201
WM_LBUTTONUP = 0x0202
WM_RBUTTONDOWN = 0x0204
WM_MBUTTONDOWN = 0x0207
WM_MOUSEWHEEL = 0x020A
WM_MOUSEHWHEEL = 0x020E
WM_LBUTTONDBLCLK = 0x0203
WM_DESTROY = 0x0002

RID_INPUT = 0x10000003
RIM_TYPEMOUSE = 0
RIM_TYPEKEYBOARD = 1
RIDEV_INPUTSINK = 0x00000100

MOUSE_MESSAGE_NAMES = {
    WM_LBUTTONDOWN: "WM_LBUTTONDOWN",
    WM_LBUTTONUP: "WM_LBUTTONUP",
    WM_RBUTTONDOWN: "WM_RBUTTONDOWN",
    WM_MBUTTONDOWN: "WM_MBUTTONDOWN",
    WM_MOUSEWHEEL: "WM_MOUSEWHEEL",
    WM_MOUSEHWHEEL: "WM_MOUSEHWHEEL",
    WM_LBUTTONDBLCLK: "WM_LBUTTONDBLCLK",
}


class RAWINPUTDEVICE(ctypes.Structure):
    _fields_ = [
        ("usUsagePage", wintypes.USHORT),
        ("usUsage", wintypes.USHORT),
        ("dwFlags", wintypes.DWORD),
        ("hwndTarget", wintypes.HWND),
    ]


class RAWINPUTHEADER(ctypes.Structure):
    _fields_ = [
        ("dwType", wintypes.DWORD),
        ("dwSize", wintypes.DWORD),
        ("hDevice", wintypes.HANDLE),
        ("wParam", ctypes.c_size_t),
    ]


class RAWKEYBOARD(ctypes.Structure):
    _fields_ = [
        ("MakeCode", wintypes.USHORT),
        ("Flags", wintypes.USHORT),
        ("Reserved", wintypes.USHORT),
        ("VKey", wintypes.USHORT),
        ("Message", wintypes.UINT),
        ("ExtraInformation", wintypes.ULONG),
    ]


class RAWMOUSEBUTTONPAIR(ctypes.Structure):
    _fields_ = [
        ("usButtonFlags", wintypes.USHORT),
        ("usButtonData", wintypes.USHORT),
    ]


class RAWMOUSEBUTTONS(ctypes.Union):
    _anonymous_ = ("pair",)
    _fields_ = [("ulButtons", wintypes.ULONG), ("pair", RAWMOUSEBUTTONPAIR)]


class RAWMOUSE(ctypes.Structure):
    _anonymous_ = ("buttons",)
    _fields_ = [
        ("usFlags", wintypes.USHORT),
        # RAWMOUSE aligns this union to a 4-byte boundary. Modelling the two
        # USHORTs directly puts them at offsets 2/4, shifting every button flag
        # into usButtonData. The real union starts at offset 4.
        ("buttons", RAWMOUSEBUTTONS),
        ("ulRawButtons", wintypes.ULONG),
        ("lLastX", wintypes.LONG),
        ("lLastY", wintypes.LONG),
        ("ulExtraInformation", wintypes.ULONG),
    ]


class RAWINPUTUNION(ctypes.Union):
    _fields_ = [("mouse", RAWMOUSE), ("keyboard", RAWKEYBOARD)]


class RAWINPUT(ctypes.Structure):
    _fields_ = [("header", RAWINPUTHEADER), ("data", RAWINPUTUNION)]


def main() -> int:
    if sys.platform != "win32":
        print("the input probe requires Windows", file=sys.stderr)
        return 2

    parser = argparse.ArgumentParser()
    parser.add_argument("--title", required=True)
    parser.add_argument("--log", required=True)
    args = parser.parse_args()

    user32 = ctypes.WinDLL("user32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    log_lock = threading.Lock()
    started = time.monotonic()

    def record(channel: str, **fields: object) -> None:
        entry = {"t_ms": round((time.monotonic() - started) * 1000, 1), "channel": channel}
        entry.update(fields)
        with log_lock:
            with open(args.log, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(entry, sort_keys=True) + "\n")
                handle.flush()
                os.fsync(handle.fileno())

    lresult = ctypes.c_ssize_t
    wparam = ctypes.c_size_t
    lparam = ctypes.c_ssize_t
    wndproc_type = ctypes.WINFUNCTYPE(lresult, wintypes.HWND, wintypes.UINT, wparam, lparam)

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
        wintypes.DWORD, wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.DWORD,
        ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
        wintypes.HWND, wintypes.HMENU, wintypes.HINSTANCE, wintypes.LPVOID,
    ]
    user32.CreateWindowExW.restype = wintypes.HWND
    user32.RegisterRawInputDevices.argtypes = [
        ctypes.POINTER(RAWINPUTDEVICE), wintypes.UINT, wintypes.UINT,
    ]
    user32.RegisterRawInputDevices.restype = wintypes.BOOL
    user32.GetRawInputData.argtypes = [
        wintypes.HANDLE, wintypes.UINT, wintypes.LPVOID,
        ctypes.POINTER(wintypes.UINT), wintypes.UINT,
    ]
    user32.GetRawInputData.restype = wintypes.UINT
    user32.GetAsyncKeyState.argtypes = [ctypes.c_int]
    user32.GetAsyncKeyState.restype = ctypes.c_short
    user32.GetKeyState.argtypes = [ctypes.c_int]
    user32.GetKeyState.restype = ctypes.c_short
    user32.GetMessageW.argtypes = [
        ctypes.POINTER(wintypes.MSG), wintypes.HWND, wintypes.UINT, wintypes.UINT,
    ]
    user32.GetMessageW.restype = ctypes.c_int
    kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    kernel32.GetModuleHandleW.restype = wintypes.HMODULE

    WS_OVERLAPPEDWINDOW = 0x00CF0000
    WS_VISIBLE = 0x10000000
    WS_CHILD = 0x40000000
    WS_TABSTOP = 0x00010000
    WS_BORDER = 0x00800000
    ES_AUTOHSCROLL = 0x0080
    SW_SHOW = 5
    COLOR_WINDOW = 5

    @wndproc_type
    def window_proc(hwnd: wintypes.HWND, message: int, raw_wparam: int, raw_lparam: int) -> int:
        if message == WM_INPUT:
            size = wintypes.UINT(0)
            user32.GetRawInputData(
                wintypes.HANDLE(raw_lparam), RID_INPUT, None,
                ctypes.byref(size), ctypes.sizeof(RAWINPUTHEADER),
            )
            buffer = ctypes.create_string_buffer(size.value)
            copied = user32.GetRawInputData(
                wintypes.HANDLE(raw_lparam), RID_INPUT, buffer,
                ctypes.byref(size), ctypes.sizeof(RAWINPUTHEADER),
            )
            if copied and copied != 0xFFFFFFFF:
                raw = ctypes.cast(buffer, ctypes.POINTER(RAWINPUT)).contents
                if raw.header.dwType == RIM_TYPEKEYBOARD:
                    record(
                        "raw_input_keyboard",
                        vkey=raw.data.keyboard.VKey,
                        make_code=raw.data.keyboard.MakeCode,
                        flags=raw.data.keyboard.Flags,
                        message=raw.data.keyboard.Message,
                    )
                elif raw.header.dwType == RIM_TYPEMOUSE:
                    record(
                        "raw_input_mouse",
                        button_flags=raw.data.mouse.usButtonFlags,
                        button_data=raw.data.mouse.usButtonData,
                        last_x=raw.data.mouse.lLastX,
                        last_y=raw.data.mouse.lLastY,
                    )
            return 0
        if message in (WM_KEYDOWN, WM_SYSKEYDOWN):
            record(
                "window_message",
                message="WM_SYSKEYDOWN" if message == WM_SYSKEYDOWN else "WM_KEYDOWN",
                vkey=int(raw_wparam),
                scan_code=(int(raw_lparam) >> 16) & 0xFF,
                ctrl_down=bool(user32.GetKeyState(0x11) & 0x8000),
                shift_down=bool(user32.GetKeyState(0x10) & 0x8000),
                alt_down=bool(user32.GetKeyState(0x12) & 0x8000),
            )
            return 0
        if message == WM_KEYUP:
            record("window_message", message="WM_KEYUP", vkey=int(raw_wparam))
            return 0
        if message == WM_CHAR:
            record("window_message", message="WM_CHAR", char=chr(int(raw_wparam)))
            return 0
        if message in MOUSE_MESSAGE_NAMES:
            record(
                "window_message",
                message=MOUSE_MESSAGE_NAMES[message],
                wheel_delta=ctypes.c_short((int(raw_wparam) >> 16) & 0xFFFF).value,
                ctrl_down=bool(user32.GetKeyState(0x11) & 0x8000),
            )
            return 0
        if message == WM_DESTROY:
            user32.PostQuitMessage(0)
            return 0
        return int(user32.DefWindowProcW(hwnd, message, raw_wparam, raw_lparam))

    instance = kernel32.GetModuleHandleW(None)
    class_name = f"AadInputProbe_{os.getpid()}"
    window_class = WNDCLASSEXW(
        cbSize=ctypes.sizeof(WNDCLASSEXW), style=0, lpfnWndProc=window_proc,
        cbClsExtra=0, cbWndExtra=0, hInstance=instance, hIcon=None, hCursor=None,
        hbrBackground=wintypes.HBRUSH(COLOR_WINDOW + 1), lpszMenuName=None,
        lpszClassName=class_name, hIconSm=None,
    )
    if not user32.RegisterClassExW(ctypes.byref(window_class)):
        raise ctypes.WinError(ctypes.get_last_error())

    hwnd = user32.CreateWindowExW(
        0, class_name, args.title, WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        100, 100, 640, 320, None, None, instance, None,
    )
    if not hwnd:
        raise ctypes.WinError(ctypes.get_last_error())

    # An EDIT child so typed characters land somewhere observable, and a
    # BUTTON so pointer events have a real control to hit.
    edit_handle = user32.CreateWindowExW(
        0, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
        24, 24, 560, 28, hwnd, wintypes.HMENU(1001), instance, None,
    )
    button_handle = user32.CreateWindowExW(
        0, "BUTTON", "Probe target", WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        24, 72, 200, 36, hwnd, wintypes.HMENU(1002), instance, None,
    )

    # INPUTSINK: deliver raw input even when this window is not foreground,
    # which is exactly the case being tested on a hidden desktop.
    devices = (RAWINPUTDEVICE * 2)(
        RAWINPUTDEVICE(usUsagePage=0x01, usUsage=0x06, dwFlags=RIDEV_INPUTSINK, hwndTarget=hwnd),
        RAWINPUTDEVICE(usUsagePage=0x01, usUsage=0x02, dwFlags=RIDEV_INPUTSINK, hwndTarget=hwnd),
    )
    raw_input_registered = bool(
        user32.RegisterRawInputDevices(devices, 2, ctypes.sizeof(RAWINPUTDEVICE))
    )
    if not raw_input_registered:
        record("setup_error", what="RegisterRawInputDevices", error=ctypes.get_last_error())

    # Poll GetAsyncKeyState on its own thread: this is the third channel, the
    # one games use most, and it is not driven by the message loop at all.
    watched = {0x41: "A", 0x42: "B", 0x11: "CTRL", 0x10: "SHIFT", 0x12: "ALT", 0x70: "F1"}
    stop_polling = threading.Event()

    def poll_async_state() -> None:
        previous = {vk: False for vk in watched}
        while not stop_polling.is_set():
            for vk, label in watched.items():
                down = bool(user32.GetAsyncKeyState(vk) & 0x8000)
                if down != previous[vk]:
                    record("async_key_state", key=label, vkey=vk, down=down)
                    previous[vk] = down
            time.sleep(0.005)

    poller = threading.Thread(target=poll_async_state, daemon=True)
    poller.start()

    user32.ShowWindow(hwnd, SW_SHOW)
    user32.UpdateWindow(hwnd)
    # Focus stays on the top-level window, not the EDIT child. Keyboard
    # messages go to the focused HWND's own wndproc, so focusing the child
    # would route them into the EDIT's built-in handler and this probe would
    # report "no window messages" for input that in fact arrived.
    user32.SetFocus(hwnd)

    print(
        json.dumps(
            {
                "ready": True,
                "pid": os.getpid(),
                "title": args.title,
                "window_handle": int(hwnd),
                "edit_handle": int(edit_handle),
                "button_handle": int(button_handle),
                "raw_input_registered": raw_input_registered,
                "log": os.path.abspath(args.log),
            },
            sort_keys=True,
        ),
        flush=True,
    )

    message = wintypes.MSG()
    while True:
        result = user32.GetMessageW(ctypes.byref(message), None, 0, 0)
        if result == 0:
            stop_polling.set()
            return int(message.wParam)
        if result == -1:
            stop_polling.set()
            raise ctypes.WinError(ctypes.get_last_error())
        user32.TranslateMessage(ctypes.byref(message))
        user32.DispatchMessageW(ctypes.byref(message))


if __name__ == "__main__":
    raise SystemExit(main())
