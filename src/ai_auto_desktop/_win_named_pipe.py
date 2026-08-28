"""Private Windows named-pipe byte stream for artifact transport.

The module is importable on every platform so the portable contract tests can
exercise validation without pretending to provide Windows semantics.  Native
operations fail closed unless ``sys.platform == "win32"``.
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
import os
import re
import secrets
import sys
import threading
import time
from typing import Any


PIPE_NAME_ENV = "AAD_ARTIFACT_PIPE_NAME"
HOST_PID_ENV = "AAD_ARTIFACT_HOST_PID"

_PIPE_NAME = re.compile(r"\\\\\.\\pipe\\aad-artifact-[0-9]+-[0-9a-f]{64}\Z")
_INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value

_GENERIC_READ = 0x80000000
_GENERIC_WRITE = 0x40000000
_FILE_FLAG_OVERLAPPED = 0x40000000
_OPEN_EXISTING = 3
_PIPE_ACCESS_DUPLEX = 0x00000003
_FILE_FLAG_FIRST_PIPE_INSTANCE = 0x00080000
_PIPE_TYPE_BYTE = 0
_PIPE_READMODE_BYTE = 0
_PIPE_WAIT = 0
_PIPE_REJECT_REMOTE_CLIENTS = 0x00000008
_ERROR_IO_PENDING = 997
_ERROR_PIPE_CONNECTED = 535
_ERROR_FILE_NOT_FOUND = 2
_ERROR_PIPE_BUSY = 231
_ERROR_SEM_TIMEOUT = 121
_ERROR_BROKEN_PIPE = 109
_ERROR_NO_DATA = 232
_ERROR_PIPE_NOT_CONNECTED = 233
_ERROR_OPERATION_ABORTED = 995
_WAIT_OBJECT_0 = 0
_WAIT_TIMEOUT = 258
_TOKEN_QUERY = 0x0008
_TOKEN_USER = 1
_SECURITY_DESCRIPTOR_REVISION = 1


class WindowsPipeError(OSError):
    """Internal, path-redacted Windows pipe failure."""

    def __init__(self, kind: str) -> None:
        super().__init__(kind)
        self.kind = kind


class _SECURITY_ATTRIBUTES(ctypes.Structure):
    _fields_ = [
        ("nLength", wintypes.DWORD),
        ("lpSecurityDescriptor", wintypes.LPVOID),
        ("bInheritHandle", wintypes.BOOL),
    ]


class _OVERLAPPED(ctypes.Structure):
    _fields_ = [
        ("Internal", ctypes.c_size_t),
        ("InternalHigh", ctypes.c_size_t),
        ("Offset", wintypes.DWORD),
        ("OffsetHigh", wintypes.DWORD),
        ("hEvent", wintypes.HANDLE),
    ]


def _require_windows() -> None:
    if sys.platform != "win32":
        raise WindowsPipeError("unavailable")


def _validate_pipe_name(value: Any) -> str:
    if type(value) is not str or _PIPE_NAME.fullmatch(value) is None:
        raise WindowsPipeError("unavailable")
    try:
        pid_text = value.split("aad-artifact-", 1)[1].split("-", 1)[0]
        _validate_pid(int(pid_text))
    except (IndexError, ValueError):
        raise WindowsPipeError("unavailable") from None
    return value


def _validate_pid(value: Any) -> int:
    if type(value) is not int or value <= 0 or value > 0xFFFFFFFF:
        raise WindowsPipeError("unavailable")
    return value


def _deadline_timeout_ms(deadline_ms: int | None) -> int:
    if deadline_ms is None:
        return 0xFFFFFFFF
    if isinstance(deadline_ms, bool) or not isinstance(deadline_ms, int):
        raise WindowsPipeError("deadline")
    remaining = deadline_ms - int(time.time() * 1000)
    if remaining <= 0:
        raise WindowsPipeError("deadline")
    return min(remaining, 0xFFFFFFFE)


def _kernel32() -> Any:
    _require_windows()
    return ctypes.WinDLL("kernel32", use_last_error=True)


def _advapi32() -> Any:
    _require_windows()
    return ctypes.WinDLL("advapi32", use_last_error=True)


def _configure_apis(kernel: Any, advapi: Any) -> None:
    kernel.CreateNamedPipeW.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
        wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
        ctypes.POINTER(_SECURITY_ATTRIBUTES),
    ]
    kernel.CreateNamedPipeW.restype = wintypes.HANDLE
    kernel.CreateFileW.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
        wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE,
    ]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.CreateEventW.argtypes = [
        wintypes.LPVOID, wintypes.BOOL, wintypes.BOOL, wintypes.LPCWSTR
    ]
    kernel.CreateEventW.restype = wintypes.HANDLE
    kernel.ConnectNamedPipe.argtypes = [wintypes.HANDLE, ctypes.POINTER(_OVERLAPPED)]
    kernel.ConnectNamedPipe.restype = wintypes.BOOL
    kernel.ReadFile.argtypes = [
        wintypes.HANDLE, wintypes.LPVOID, wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD), ctypes.POINTER(_OVERLAPPED),
    ]
    kernel.ReadFile.restype = wintypes.BOOL
    kernel.WriteFile.argtypes = kernel.ReadFile.argtypes
    kernel.WriteFile.restype = wintypes.BOOL
    kernel.GetOverlappedResult.argtypes = [
        wintypes.HANDLE, ctypes.POINTER(_OVERLAPPED),
        ctypes.POINTER(wintypes.DWORD), wintypes.BOOL,
    ]
    kernel.GetOverlappedResult.restype = wintypes.BOOL
    kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    kernel.WaitForSingleObject.restype = wintypes.DWORD
    kernel.CancelIoEx.argtypes = [wintypes.HANDLE, ctypes.POINTER(_OVERLAPPED)]
    kernel.CancelIoEx.restype = wintypes.BOOL
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    kernel.DisconnectNamedPipe.argtypes = [wintypes.HANDLE]
    kernel.DisconnectNamedPipe.restype = wintypes.BOOL
    kernel.WaitNamedPipeW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD]
    kernel.WaitNamedPipeW.restype = wintypes.BOOL
    kernel.GetNamedPipeClientProcessId.argtypes = [
        wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)
    ]
    kernel.GetNamedPipeClientProcessId.restype = wintypes.BOOL
    kernel.GetNamedPipeServerProcessId.argtypes = [
        wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)
    ]
    kernel.GetNamedPipeServerProcessId.restype = wintypes.BOOL
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.LocalFree.argtypes = [wintypes.HLOCAL]
    kernel.LocalFree.restype = wintypes.HLOCAL
    advapi.OpenProcessToken.argtypes = [
        wintypes.HANDLE, wintypes.DWORD, ctypes.POINTER(wintypes.HANDLE)
    ]
    advapi.OpenProcessToken.restype = wintypes.BOOL
    advapi.GetTokenInformation.argtypes = [
        wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
    ]
    advapi.GetTokenInformation.restype = wintypes.BOOL
    advapi.ConvertSidToStringSidW.argtypes = [wintypes.LPVOID, ctypes.POINTER(wintypes.LPWSTR)]
    advapi.ConvertSidToStringSidW.restype = wintypes.BOOL
    advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, ctypes.POINTER(wintypes.LPVOID),
        ctypes.POINTER(wintypes.DWORD),
    ]
    advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = wintypes.BOOL


def _close_handle(kernel: Any, handle: int | None) -> None:
    if handle not in (None, 0, _INVALID_HANDLE_VALUE):
        kernel.CloseHandle(handle)


def _current_user_sid_string(kernel: Any, advapi: Any) -> str:
    token = wintypes.HANDLE()
    if not advapi.OpenProcessToken(
        kernel.GetCurrentProcess(), _TOKEN_QUERY, ctypes.byref(token)
    ):
        raise WindowsPipeError("security")
    try:
        needed = wintypes.DWORD()
        advapi.GetTokenInformation(
            token, _TOKEN_USER, None, 0, ctypes.byref(needed)
        )
        if not needed.value:
            raise WindowsPipeError("security")
        buffer = ctypes.create_string_buffer(needed.value)
        if not advapi.GetTokenInformation(
            token, _TOKEN_USER, buffer, needed, ctypes.byref(needed)
        ):
            raise WindowsPipeError("security")
        sid_pointer = ctypes.c_void_p.from_buffer(buffer).value
        text = wintypes.LPWSTR()
        if not advapi.ConvertSidToStringSidW(
            sid_pointer, ctypes.byref(text)
        ):
            raise WindowsPipeError("security")
        try:
            return text.value
        finally:
            kernel.LocalFree(ctypes.cast(text, wintypes.HLOCAL))
    finally:
        _close_handle(kernel, token.value)


class _SecurityDescriptor:
    def __init__(self, kernel: Any, advapi: Any) -> None:
        sid = _current_user_sid_string(kernel, advapi)
        pointer = wintypes.LPVOID()
        sddl = f"D:P(A;;GA;;;SY)(A;;GA;;;{sid})"
        if not advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl, _SECURITY_DESCRIPTOR_REVISION, ctypes.byref(pointer), None
        ):
            raise WindowsPipeError("security")
        self._kernel = kernel
        self._pointer = pointer
        self.attributes = _SECURITY_ATTRIBUTES(
            ctypes.sizeof(_SECURITY_ATTRIBUTES), pointer, False
        )

    def close(self) -> None:
        if self._pointer:
            self._kernel.LocalFree(ctypes.cast(self._pointer, wintypes.HLOCAL))
            self._pointer = wintypes.LPVOID()


class WindowsPipeChannel:
    """One duplex named-pipe byte stream with deadline-aware overlapped I/O."""

    def __init__(self, handle: int, *, server: bool) -> None:
        self._kernel = _kernel32()
        self._advapi = _advapi32()
        _configure_apis(self._kernel, self._advapi)
        self._handle = handle
        self._server = server
        self._read_lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._close_lock = threading.Lock()

    def send_all(self, data: bytes, deadline_ms: int | None) -> None:
        view = memoryview(data)
        with self._write_lock:
            while view:
                written = self._overlapped_io(
                    self._kernel.WriteFile, bytes(view), deadline_ms
                )
                if written <= 0:
                    raise WindowsPipeError("truncated")
                view = view[written:]

    def recv_exact(self, size: int, deadline_ms: int | None) -> bytes:
        result = bytearray()
        with self._read_lock:
            while len(result) < size:
                buffer = ctypes.create_string_buffer(size - len(result))
                read = self._overlapped_io(
                    self._kernel.ReadFile, buffer, deadline_ms
                )
                if read <= 0:
                    raise WindowsPipeError("truncated")
                result.extend(buffer.raw[:read])
        return bytes(result)

    def readable(self, timeout: float) -> bool:
        if timeout <= 0:
            return False
        available = wintypes.DWORD()
        self._kernel.PeekNamedPipe.argtypes = [
            wintypes.HANDLE, wintypes.LPVOID, wintypes.DWORD,
            ctypes.POINTER(wintypes.DWORD), ctypes.POINTER(wintypes.DWORD),
            ctypes.POINTER(wintypes.DWORD),
        ]
        self._kernel.PeekNamedPipe.restype = wintypes.BOOL
        deadline = time.monotonic() + timeout
        while True:
            if not self._kernel.PeekNamedPipe(
                self._handle, None, 0, None, ctypes.byref(available), None
            ):
                self._raise_io_error(ctypes.get_last_error())
            if available.value:
                return True
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            time.sleep(min(remaining, 0.002))

    def shutdown_write(self) -> None:
        self.close()

    def shutdown_both(self) -> None:
        self.close()

    def close(self) -> None:
        with self._close_lock:
            handle = self._handle
            if handle in (None, 0, _INVALID_HANDLE_VALUE):
                return
            self._handle = None
            self._kernel.CancelIoEx(handle, None)
            if self._server:
                self._kernel.DisconnectNamedPipe(handle)
            _close_handle(self._kernel, handle)

    def _overlapped_io(
        self, operation: Any, buffer: Any, deadline_ms: int | None
    ) -> int:
        handle = self._handle
        if handle in (None, 0, _INVALID_HANDLE_VALUE):
            raise WindowsPipeError("channel")
        event = self._kernel.CreateEventW(None, True, False, None)
        if not event:
            raise WindowsPipeError("channel")
        overlapped = _OVERLAPPED(hEvent=event)
        transferred = wintypes.DWORD()
        try:
            length = ctypes.sizeof(buffer) if isinstance(buffer, ctypes.Array) else len(buffer)
            ok = operation(
                handle, buffer, length, ctypes.byref(transferred),
                ctypes.byref(overlapped),
            )
            error = 0 if ok else ctypes.get_last_error()
            if not ok and error != _ERROR_IO_PENDING:
                self._raise_io_error(error)
            if not ok:
                wait = self._kernel.WaitForSingleObject(
                    event, _deadline_timeout_ms(deadline_ms)
                )
                if wait == _WAIT_TIMEOUT:
                    self._kernel.CancelIoEx(handle, ctypes.byref(overlapped))
                    self._kernel.WaitForSingleObject(event, 1000)
                    raise WindowsPipeError("deadline")
                if wait != _WAIT_OBJECT_0:
                    self._kernel.CancelIoEx(handle, ctypes.byref(overlapped))
                    raise WindowsPipeError("channel")
                if not self._kernel.GetOverlappedResult(
                    handle, ctypes.byref(overlapped), ctypes.byref(transferred),
                    False,
                ):
                    self._raise_io_error(ctypes.get_last_error())
            return transferred.value
        finally:
            _close_handle(self._kernel, event)

    @staticmethod
    def _raise_io_error(error: int) -> None:
        if error in {_ERROR_BROKEN_PIPE, _ERROR_NO_DATA, _ERROR_PIPE_NOT_CONNECTED}:
            raise WindowsPipeError("truncated")
        if error == _ERROR_OPERATION_ABORTED:
            raise WindowsPipeError("channel")
        raise WindowsPipeError("channel")


class WindowsPipeServer:
    """Single-instance current-user pipe listener created before Popen."""

    def __init__(self, name: str | None = None) -> None:
        kernel = _kernel32()
        advapi = _advapi32()
        _configure_apis(kernel, advapi)
        if name is None:
            name = (
                f"\\\\.\\pipe\\aad-artifact-{os.getpid()}-"
                f"{secrets.token_hex(32)}"
            )
        else:
            name = _validate_pipe_name(name)
        descriptor = _SecurityDescriptor(kernel, advapi)
        try:
            handle = kernel.CreateNamedPipeW(
                name,
                _PIPE_ACCESS_DUPLEX | _FILE_FLAG_OVERLAPPED | _FILE_FLAG_FIRST_PIPE_INSTANCE,
                _PIPE_TYPE_BYTE | _PIPE_READMODE_BYTE | _PIPE_WAIT | _PIPE_REJECT_REMOTE_CLIENTS,
                1, 65536, 65536, 0, ctypes.byref(descriptor.attributes),
            )
        finally:
            descriptor.close()
        if handle == _INVALID_HANDLE_VALUE:
            raise WindowsPipeError("unavailable")
        self.name = name
        self._kernel = kernel
        self._handle = handle

    def accept(self, expected_pid: int, deadline_ms: int) -> WindowsPipeChannel:
        expected_pid = _validate_pid(expected_pid)
        handle = self._handle
        if handle in (None, 0, _INVALID_HANDLE_VALUE):
            raise WindowsPipeError("unavailable")
        event = self._kernel.CreateEventW(None, True, False, None)
        if not event:
            raise WindowsPipeError("channel")
        overlapped = _OVERLAPPED(hEvent=event)
        try:
            ok = self._kernel.ConnectNamedPipe(handle, ctypes.byref(overlapped))
            error = 0 if ok else ctypes.get_last_error()
            if not ok and error == _ERROR_PIPE_CONNECTED:
                pass
            elif not ok and error == _ERROR_IO_PENDING:
                wait = self._kernel.WaitForSingleObject(
                    event, _deadline_timeout_ms(deadline_ms)
                )
                if wait == _WAIT_TIMEOUT:
                    self._kernel.CancelIoEx(handle, ctypes.byref(overlapped))
                    self._kernel.WaitForSingleObject(event, 1000)
                    raise WindowsPipeError("deadline")
                if wait != _WAIT_OBJECT_0:
                    raise WindowsPipeError("channel")
            elif not ok:
                raise WindowsPipeError("channel")
            actual_pid = wintypes.DWORD()
            if not self._kernel.GetNamedPipeClientProcessId(
                handle, ctypes.byref(actual_pid)
            ) or actual_pid.value != expected_pid:
                self._kernel.DisconnectNamedPipe(handle)
                raise WindowsPipeError("identity")
            self._handle = None
            return WindowsPipeChannel(handle, server=True)
        finally:
            _close_handle(self._kernel, event)

    def close(self) -> None:
        handle = self._handle
        self._handle = None
        if handle not in (None, 0, _INVALID_HANDLE_VALUE):
            self._kernel.CancelIoEx(handle, None)
            self._kernel.DisconnectNamedPipe(handle)
            _close_handle(self._kernel, handle)


def connect_worker(
    pipe_name: str, expected_host_pid: int, deadline_ms: int
) -> WindowsPipeChannel:
    """Connect a worker and verify the named pipe belongs to its Host PID."""

    name = _validate_pipe_name(pipe_name)
    expected_pid = _validate_pid(expected_host_pid)
    kernel = _kernel32()
    advapi = _advapi32()
    _configure_apis(kernel, advapi)
    handle: int | None = None
    while handle is None:
        remaining = _deadline_timeout_ms(deadline_ms)
        candidate = kernel.CreateFileW(
            name, _GENERIC_READ | _GENERIC_WRITE, 0, None, _OPEN_EXISTING,
            _FILE_FLAG_OVERLAPPED, None,
        )
        if candidate != _INVALID_HANDLE_VALUE:
            handle = candidate
            break
        error = ctypes.get_last_error()
        if error not in {_ERROR_FILE_NOT_FOUND, _ERROR_PIPE_BUSY, _ERROR_SEM_TIMEOUT}:
            raise WindowsPipeError("unavailable")
        kernel.WaitNamedPipeW(name, min(remaining, 50))
    actual_pid = wintypes.DWORD()
    if not kernel.GetNamedPipeServerProcessId(
        handle, ctypes.byref(actual_pid)
    ) or actual_pid.value != expected_pid:
        _close_handle(kernel, handle)
        raise WindowsPipeError("identity")
    return WindowsPipeChannel(handle, server=False)


__all__ = [
    "HOST_PID_ENV", "PIPE_NAME_ENV", "WindowsPipeChannel",
    "WindowsPipeError", "WindowsPipeServer", "connect_worker",
]
