"""Private Windows Job Object supervisor for worker process trees.

POSIX hosts put every worker in its own session/process group so a single
``killpg`` reclaims the worker and every descendant it spawned.  Windows has no
equivalent signal, and ``Popen.terminate``/``kill`` only ends the immediate
child: a worker that spawned helpers (a shell wrapper, an interpreter, a native
helper) leaves those helpers running after the host gives up on it.

A Job Object closes that gap.  The host creates one job per worker *before*
``Popen``, starts the worker suspended, assigns it to the job, then resumes it.
Because job membership is inherited by every process the worker creates,
``TerminateJobObject`` reclaims the entire tree in one call, and
``JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`` guarantees the same reclamation if the
host itself dies without unwinding.

The module is importable on every platform so the portable contract tests can
exercise validation without pretending to provide Windows semantics.  Native
operations fail closed unless ``sys.platform == "win32"``.

Two limits are deliberate and documented rather than hidden:

* Assignment happens while the worker is suspended, so it cannot spawn anything
  before it belongs to the job.  There is no window in which a descendant
  escapes supervision.
* A process already in a job it may not leave can still refuse nested
  assignment on kernels without nested-job support (before Windows 8 /
  Server 2012).  ``assign`` reports that as ``unsupported`` and the caller
  degrades to direct termination instead of failing the run.
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
import sys
import threading
from typing import Any


_INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value

# Job information classes and limit flags.
_JobObjectExtendedLimitInformation = 9
_JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000
_JOB_OBJECT_LIMIT_PROCESS_MEMORY = 0x00000100
_JOB_OBJECT_LIMIT_PROCESS_TIME = 0x00000002
_JOB_OBJECT_LIMIT_ACTIVE_PROCESS = 0x00000008

# PerProcessUserTimeLimit is expressed in 100-nanosecond units.
_HUNDRED_NS_PER_SECOND = 10_000_000

# Process/thread access rights.
_PROCESS_TERMINATE = 0x0001
_PROCESS_SET_QUOTA = 0x0100
_PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
_THREAD_SUSPEND_RESUME = 0x0002
_SYNCHRONIZE = 0x00100000

_TH32CS_SNAPTHREAD = 0x00000004
_STILL_ACTIVE = 259
_WAIT_OBJECT_0 = 0
_WAIT_TIMEOUT = 258
_ERROR_ACCESS_DENIED = 5
_ERROR_NOT_SUPPORTED = 50
_RESUME_THREAD_FAILED = 0xFFFFFFFF


class WindowsJobError(OSError):
    """Internal, path-redacted Windows Job Object failure.

    ``kind`` is one of ``unavailable`` (the platform or API surface cannot be
    used), ``unsupported`` (this kernel/job nesting combination refuses the
    assignment) or ``resume`` (the suspended worker could not be resumed, which
    the caller must treat as a failed start).
    """

    def __init__(self, kind: str) -> None:
        super().__init__(kind)
        self.kind = kind


class _IO_COUNTERS(ctypes.Structure):
    _fields_ = [
        ("ReadOperationCount", ctypes.c_ulonglong),
        ("WriteOperationCount", ctypes.c_ulonglong),
        ("OtherOperationCount", ctypes.c_ulonglong),
        ("ReadTransferCount", ctypes.c_ulonglong),
        ("WriteTransferCount", ctypes.c_ulonglong),
        ("OtherTransferCount", ctypes.c_ulonglong),
    ]


class _JOBOBJECT_BASIC_LIMIT_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("PerProcessUserTimeLimit", wintypes.LARGE_INTEGER),
        ("PerJobUserTimeLimit", wintypes.LARGE_INTEGER),
        ("LimitFlags", wintypes.DWORD),
        ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t),
        ("ActiveProcessLimit", wintypes.DWORD),
        ("Affinity", ctypes.c_size_t),
        ("PriorityClass", wintypes.DWORD),
        ("SchedulingClass", wintypes.DWORD),
    ]


class _JOBOBJECT_EXTENDED_LIMIT_INFORMATION(ctypes.Structure):
    _fields_ = [
        ("BasicLimitInformation", _JOBOBJECT_BASIC_LIMIT_INFORMATION),
        ("IoInfo", _IO_COUNTERS),
        ("ProcessMemoryLimit", ctypes.c_size_t),
        ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t),
        ("PeakJobMemoryUsed", ctypes.c_size_t),
    ]


class _THREADENTRY32(ctypes.Structure):
    _fields_ = [
        ("dwSize", wintypes.DWORD),
        ("cntUsage", wintypes.DWORD),
        ("th32ThreadID", wintypes.DWORD),
        ("th32OwnerProcessID", wintypes.DWORD),
        ("tpBasePri", wintypes.LONG),
        ("tpDeltaPri", wintypes.LONG),
        ("dwFlags", wintypes.DWORD),
    ]


def _require_windows() -> None:
    if sys.platform != "win32":
        raise WindowsJobError("unavailable")


def _validate_limit(value: Any, name: str) -> int | None:
    """Accept a positive int limit or None; reject anything else fail-closed."""

    if value is None:
        return None
    if type(value) is not int or value <= 0:
        raise WindowsJobError("unavailable")
    return value


def _validate_pid(value: Any) -> int:
    if type(value) is not int or value <= 0 or value > 0xFFFFFFFF:
        raise WindowsJobError("unavailable")
    return value


def _kernel32() -> Any:
    _require_windows()
    return ctypes.WinDLL("kernel32", use_last_error=True)


def _configure_apis(kernel: Any) -> None:
    kernel.CreateJobObjectW.argtypes = [wintypes.LPVOID, wintypes.LPCWSTR]
    kernel.CreateJobObjectW.restype = wintypes.HANDLE
    kernel.SetInformationJobObject.argtypes = [
        wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD
    ]
    kernel.SetInformationJobObject.restype = wintypes.BOOL
    kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel.AssignProcessToJobObject.restype = wintypes.BOOL
    kernel.IsProcessInJob.argtypes = [
        wintypes.HANDLE, wintypes.HANDLE, ctypes.POINTER(wintypes.BOOL)
    ]
    kernel.IsProcessInJob.restype = wintypes.BOOL
    kernel.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
    kernel.TerminateJobObject.restype = wintypes.BOOL
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.GetExitCodeProcess.argtypes = [
        wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)
    ]
    kernel.GetExitCodeProcess.restype = wintypes.BOOL
    kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    kernel.WaitForSingleObject.restype = wintypes.DWORD
    kernel.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    kernel.Thread32First.argtypes = [
        wintypes.HANDLE, ctypes.POINTER(_THREADENTRY32)
    ]
    kernel.Thread32First.restype = wintypes.BOOL
    kernel.Thread32Next.argtypes = [
        wintypes.HANDLE, ctypes.POINTER(_THREADENTRY32)
    ]
    kernel.Thread32Next.restype = wintypes.BOOL
    kernel.OpenThread.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenThread.restype = wintypes.HANDLE
    kernel.ResumeThread.argtypes = [wintypes.HANDLE]
    kernel.ResumeThread.restype = wintypes.DWORD
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL


def _close_handle(kernel: Any, handle: int | None) -> None:
    if handle not in (None, 0, _INVALID_HANDLE_VALUE):
        kernel.CloseHandle(handle)


def resume_process(pid: int) -> None:
    """Resume every thread of a process started with ``CREATE_SUSPENDED``.

    A freshly created suspended worker has exactly one thread, but the
    enumeration is written generally so a partially resumed process can never
    be left half-running.  Failure raises ``WindowsJobError('resume')``; the
    caller must reclaim the worker because it was created and never ran.
    """

    pid = _validate_pid(pid)
    kernel = _kernel32()
    _configure_apis(kernel)
    snapshot = kernel.CreateToolhelp32Snapshot(_TH32CS_SNAPTHREAD, 0)
    if not snapshot or snapshot == _INVALID_HANDLE_VALUE:
        raise WindowsJobError("resume")
    resumed = 0
    try:
        entry = _THREADENTRY32()
        entry.dwSize = ctypes.sizeof(_THREADENTRY32)
        if not kernel.Thread32First(snapshot, ctypes.byref(entry)):
            raise WindowsJobError("resume")
        while True:
            if entry.th32OwnerProcessID == pid:
                thread = kernel.OpenThread(
                    _THREAD_SUSPEND_RESUME, False, entry.th32ThreadID
                )
                if not thread or thread == _INVALID_HANDLE_VALUE:
                    raise WindowsJobError("resume")
                try:
                    previous = int(kernel.ResumeThread(thread))
                finally:
                    _close_handle(kernel, thread)
                if previous == _RESUME_THREAD_FAILED:
                    raise WindowsJobError("resume")
                resumed += 1
            if not kernel.Thread32Next(snapshot, ctypes.byref(entry)):
                break
    finally:
        _close_handle(kernel, snapshot)
    if resumed <= 0:
        raise WindowsJobError("resume")


def process_is_running(pid: int) -> bool:
    """Report whether a PID is still an active process.

    Used by tests and diagnostics to prove a descendant really was reclaimed
    rather than merely detached from the host's pipes.
    """

    pid = _validate_pid(pid)
    kernel = _kernel32()
    _configure_apis(kernel)
    # SYNCHRONIZE is required for the WaitForSingleObject confirmation below;
    # querying the exit code alone needs only QUERY_LIMITED_INFORMATION.
    process = kernel.OpenProcess(
        _SYNCHRONIZE | _PROCESS_QUERY_LIMITED_INFORMATION, False, pid
    )
    if not process or process == _INVALID_HANDLE_VALUE:
        return False
    try:
        code = wintypes.DWORD()
        if not kernel.GetExitCodeProcess(process, ctypes.byref(code)):
            return False
        if code.value != _STILL_ACTIVE:
            return False
        # A process can exit with the literal value 259, so confirm with a
        # zero-timeout wait rather than trusting the sentinel alone.
        wait = kernel.WaitForSingleObject(process, 0)
        if wait == _WAIT_TIMEOUT:
            return True
        if wait == _WAIT_OBJECT_0:
            return False
        # The wait itself was unavailable; fall back to the exit-code sentinel
        # rather than reporting a live process as reclaimed.
        return True
    finally:
        _close_handle(kernel, process)


class WindowsJob:
    """One worker's job object: assign while suspended, then kill as a tree.

    The intended lifecycle is exactly::

        WindowsJob() -> Popen(..., CREATE_SUSPENDED) -> assign(pid)
                     -> resume(pid) -> ... -> terminate() / close()

    Calling ``assign`` before ``resume`` is what makes the guarantee hold;
    assigning an already-running worker would leave a window in which it could
    spawn an unsupervised descendant.
    """

    def __init__(
        self,
        *,
        memory_bytes: int | None = None,
        cpu_seconds: int | None = None,
        active_processes: int | None = None,
    ) -> None:
        """Create the job, optionally capping memory, CPU time and process count.

        The caps are opt-in so worker supervision keeps its existing behaviour
        unchanged; the script sandbox is the caller that asks for them.  Each cap
        is enforced by the kernel, so a script cannot exceed it by any means
        available inside the process.
        """

        memory_bytes = _validate_limit(memory_bytes, "memory_bytes")
        cpu_seconds = _validate_limit(cpu_seconds, "cpu_seconds")
        active_processes = _validate_limit(active_processes, "active_processes")
        kernel = _kernel32()
        _configure_apis(kernel)
        handle = kernel.CreateJobObjectW(None, None)
        if not handle or handle == _INVALID_HANDLE_VALUE:
            raise WindowsJobError("unavailable")
        information = _JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
        # KILL_ON_JOB_CLOSE reclaims the tree even if the host dies without
        # unwinding.  Breakaway is intentionally NOT granted: a worker must not
        # be able to place a descendant outside the supervised tree.
        flags = _JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if memory_bytes is not None:
            flags |= _JOB_OBJECT_LIMIT_PROCESS_MEMORY
            information.ProcessMemoryLimit = memory_bytes
        if cpu_seconds is not None:
            # A process that exceeds the CPU limit is terminated by the kernel.
            flags |= _JOB_OBJECT_LIMIT_PROCESS_TIME
            information.BasicLimitInformation.PerProcessUserTimeLimit = (
                cpu_seconds * _HUNDRED_NS_PER_SECOND
            )
        if active_processes is not None:
            flags |= _JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            information.BasicLimitInformation.ActiveProcessLimit = active_processes
        information.BasicLimitInformation.LimitFlags = flags
        if not kernel.SetInformationJobObject(
            handle,
            _JobObjectExtendedLimitInformation,
            ctypes.byref(information),
            ctypes.sizeof(information),
        ):
            _close_handle(kernel, handle)
            raise WindowsJobError("unavailable")
        self._kernel = kernel
        self._handle: int | None = handle
        self._lock = threading.Lock()
        self._assigned_pid: int | None = None

    @property
    def assigned_pid(self) -> int | None:
        """The worker PID this job supervises, or ``None`` before ``assign``."""

        return self._assigned_pid

    def assign(self, pid: int) -> None:
        """Assign a suspended worker to this job.

        Raises ``WindowsJobError('unsupported')`` when the kernel refuses the
        nested assignment.  The caller must then fall back to direct
        termination rather than abandoning the worker.
        """

        pid = _validate_pid(pid)
        kernel = self._kernel
        handle = self._handle
        if handle is None:
            raise WindowsJobError("unavailable")
        process = kernel.OpenProcess(
            _PROCESS_SET_QUOTA | _PROCESS_TERMINATE
            | _PROCESS_QUERY_LIMITED_INFORMATION,
            False,
            pid,
        )
        if not process or process == _INVALID_HANDLE_VALUE:
            raise WindowsJobError("unavailable")
        try:
            if not kernel.AssignProcessToJobObject(handle, process):
                error = ctypes.get_last_error()
                if error in {_ERROR_ACCESS_DENIED, _ERROR_NOT_SUPPORTED}:
                    raise WindowsJobError("unsupported")
                raise WindowsJobError("unavailable")
            member = wintypes.BOOL()
            if not kernel.IsProcessInJob(
                process, handle, ctypes.byref(member)
            ) or not member.value:
                raise WindowsJobError("unsupported")
        finally:
            _close_handle(kernel, process)
        self._assigned_pid = pid

    def resume(self, pid: int) -> None:
        """Resume the assigned worker after it has become a job member."""

        resume_process(pid)

    def terminate(self, exit_code: int = 1) -> bool:
        """Terminate every process in the job.  Returns whether the call ran.

        This is the Windows counterpart of ``killpg``: one call reclaims the
        worker and every descendant, including processes the host never saw.
        """

        with self._lock:
            handle = self._handle
            if handle is None:
                return False
            return bool(self._kernel.TerminateJobObject(handle, exit_code))

    def close(self) -> None:
        """Close the job handle; survivors die via KILL_ON_JOB_CLOSE."""

        with self._lock:
            handle = self._handle
            if handle is None:
                return
            self._handle = None
            _close_handle(self._kernel, handle)

    def __enter__(self) -> "WindowsJob":
        return self

    def __exit__(self, *_exception: object) -> None:
        self.close()


__all__ = [
    "WindowsJob",
    "WindowsJobError",
    "process_is_running",
    "resume_process",
]
