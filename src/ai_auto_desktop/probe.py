"""Conservative, read-only probes for desktop automation prerequisites.

The probe reports prerequisites visible to the current process.  It never asks
for a permission, opens a portal session, injects input, captures the screen,
or traverses an accessibility tree.  An ``available`` result therefore does
not mean that desktop automation has succeeded.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import ctypes
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import sys
import threading
import time
from typing import Any, Mapping, Sequence
import uuid


PROBE_API_VERSION = "ai-auto-desktop.dev/probe/v1alpha1"
PROBE_KIND = "CapabilityProbe"
PROBE_STATES = frozenset({"available", "degraded", "unavailable", "unknown"})
_POSIX_TRUSTED_COMMAND_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"
_COMMAND_OUTPUT_LIMIT = 64 * 1024
_SYSTEM_LIBRARY_DIRECTORIES = (
    Path("/lib"),
    Path("/lib64"),
    Path("/usr/lib"),
    Path("/usr/lib64"),
)


# Win32 constants used by the read-only Windows probes.
_SM_CXSCREEN = 0
_SM_REMOTESESSION = 0x1000
_DESKTOPHORZRES = 118
_UOI_NAME = 2
_DESKTOP_READOBJECTS = 0x0001
_TOKEN_QUERY = 0x0008
_TokenElevation = 20
_TokenIntegrityLevel = 25
# Mandatory label RIDs, highest first, mapped to stable reportable names.
_INTEGRITY_LEVELS = (
    (0x4000, "system"),
    (0x3000, "high"),
    (0x2000, "medium"),
    (0x1000, "low"),
)
_DPI_AWARENESS = {0: "unaware", 1: "system", 2: "per_monitor"}


def _windows_trusted_command_path() -> str:
    """Fixed system directories for Windows probe helpers.

    Resolved from SystemRoot rather than assuming ``C:\\Windows`` so the probe
    still works on a system installed to another volume, and never from the
    caller's PATH.  A probe helper is executable code, so an attacker-controlled
    PATH entry must not be able to satisfy a lookup.
    """

    system_root = os.environ.get("SystemRoot") or os.environ.get("windir")
    if not system_root:
        system_root = "C:\\Windows"
    root = Path(system_root)
    return os.pathsep.join(
        str(candidate)
        for candidate in (root / "System32", root, root / "System32" / "Wbem")
    )


def _trusted_command_path() -> str:
    """The platform's fixed search path for probe helpers."""

    if os.name == "nt":
        return _windows_trusted_command_path()
    return _POSIX_TRUSTED_COMMAND_PATH


@dataclass(frozen=True, slots=True)
class CapabilityCheck:
    """One narrowly scoped prerequisite observation."""

    name: str
    state: str
    summary: str
    evidence: Mapping[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.state not in PROBE_STATES:
            raise ValueError(f"invalid probe state: {self.state}")

    def to_dict(self) -> dict[str, Any]:
        return {
            "state": self.state,
            "summary": self.summary,
            "evidence": dict(self.evidence),
        }


@dataclass(frozen=True, slots=True)
class ProbeReport:
    """Versioned JSON-facing result of a successful probe run."""

    platform: Mapping[str, Any]
    session: Mapping[str, Any]
    checks: tuple[CapabilityCheck, ...]

    def to_dict(self) -> dict[str, Any]:
        counts = {state: 0 for state in sorted(PROBE_STATES)}
        for check in self.checks:
            counts[check.state] += 1
        return {
            "api_version": PROBE_API_VERSION,
            "kind": PROBE_KIND,
            "status": "completed",
            "platform": dict(self.platform),
            "session": dict(self.session),
            "checks": {check.name: check.to_dict() for check in self.checks},
            "summary": counts,
            "notice": (
                "Read-only prerequisite observations only; this report does not "
                "prove that UI discovery, input, capture, or automation succeeds."
            ),
        }


@dataclass(frozen=True, slots=True)
class _CommandResult:
    outcome: str
    returncode: int | None = None
    stdout: str = ""


def _canonical_platform(system: str) -> str:
    return {"Windows": "windows", "Darwin": "macos", "Linux": "linux"}.get(
        system, "unknown"
    )


def _session_info(name: str, environ: Mapping[str, str]) -> dict[str, Any]:
    display = bool(environ.get("DISPLAY"))
    wayland = bool(environ.get("WAYLAND_DISPLAY"))
    ssh = bool(environ.get("SSH_CONNECTION") or environ.get("SSH_TTY"))
    xdg_type = environ.get("XDG_SESSION_TYPE", "").lower()
    windows_session = environ.get("SESSIONNAME", "").lower()

    kind = "unknown"
    interactive: bool | None = None
    if name == "linux":
        if wayland or xdg_type == "wayland":
            kind, interactive = "wayland", True
        elif display or xdg_type == "x11":
            kind, interactive = "x11", True
        elif ssh:
            kind, interactive = "ssh", bool(display or wayland)
        elif xdg_type in {"tty", "console"}:
            kind, interactive = "tty", True
    elif name == "windows":
        if windows_session.startswith("rdp"):
            kind, interactive = "remote_desktop", True
        elif windows_session == "console":
            kind, interactive = "console", True
        elif windows_session == "services":
            kind, interactive = "service", False
    elif name == "macos" and ssh:
        kind, interactive = "ssh", False
    if name == "linux" and ssh:
        kind = f"ssh_{kind}" if kind in {"x11", "wayland"} else "ssh"
        # Environment variables alone do not prove the forwarded display is
        # connected or controllable.  Individual checks report that evidence.
        interactive = None

    try:
        stdin_is_tty = bool(sys.stdin is not None and sys.stdin.isatty())
    except (AttributeError, OSError, ValueError):
        stdin_is_tty = False

    return {
        "kind": kind,
        "interactive": interactive,
        "signals": {
            "display_advertised": display,
            "wayland_display_advertised": wayland,
            "xdg_session_type_advertised": bool(xdg_type),
            "ssh_advertised": ssh,
            "windows_session_advertised": bool(windows_session),
            "stdin_is_tty": stdin_is_tty,
        },
    }


def _which(command: str) -> str | None:
    # Probe helpers are executable code.  Resolve only from fixed system
    # directories rather than trusting the caller's PATH, then execute the
    # returned absolute path to avoid a second PATH lookup.
    return shutil.which(command, path=_trusted_command_path())


def _is_absolute_program_path(program: str) -> bool:
    """Whether ``program`` is an absolute path on the *current* platform.

    ``Path('/usr/bin/gdbus').is_absolute()`` is False on Windows and
    ``Path('C:/Windows/System32/whoami.exe').is_absolute()`` is False on POSIX,
    so a single pathlib call cannot express "already resolved, do not search
    PATH".  The check below accepts what the running kernel would treat as
    absolute, and additionally accepts a POSIX-rooted path on POSIX only, so a
    Windows drive-relative path such as ``C:helper.exe`` is still rejected.
    """

    if not program:
        return False
    if os.name == "nt":
        drive, tail = os.path.splitdrive(program)
        if drive and tail[:1] in ("\\", "/"):
            return True
        # A UNC path (\\server\share) has a drive component and a rooted tail,
        # so it is covered above.  Anything else is relative.
        return False
    return program.startswith("/")


def _library_found(name: str) -> bool:
    # ctypes.util.find_library may invoke compiler/linker commands found on
    # PATH.  A passive diagnostic must not execute them, so inspect common
    # system library directories instead.  This is intentionally conservative
    # and is reported only as supporting evidence.
    candidate = f"lib{name}.so"
    for directory in _SYSTEM_LIBRARY_DIRECTORIES:
        try:
            if any(directory.glob(candidate + "*")):
                return True
            if any(directory.glob("*/" + candidate + "*")):
                return True
        except OSError:
            continue
    return False


def _run_read_only(
    argv: Sequence[str],
    timeout: float = 1.5,
    *,
    environ: Mapping[str, str] | None = None,
    pass_environment: Sequence[str] = (),
) -> _CommandResult:
    if not argv or not _is_absolute_program_path(argv[0]):
        return _CommandResult("error")
    source_environment = os.environ if environ is None else environ
    child_environment = {
        "PATH": _trusted_command_path(),
        "LANG": "C",
        "LC_ALL": "C",
    }
    if os.name == "nt":
        # Windows resolves a bare program name against SystemRoot even with a
        # restricted PATH, and several console helpers fail outright without
        # SystemRoot present.  Pass the minimum needed to run, nothing more.
        for name in ("SystemRoot", "windir", "SystemDrive"):
            value = source_environment.get(name) or os.environ.get(name)
            if value:
                child_environment[name] = value
    for name in pass_environment:
        value = source_environment.get(name)
        if value:
            child_environment[name] = value
    output = bytearray()
    overflow = threading.Event()
    process: subprocess.Popen[bytes] | None = None
    try:
        process = subprocess.Popen(
            list(argv),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=child_environment,
            start_new_session=os.name == "posix",
        )
    except (FileNotFoundError, OSError, ValueError):
        return _CommandResult("error")
    assert process.stdout is not None

    def drain() -> None:
        try:
            while True:
                chunk = process.stdout.read(8192)
                if not chunk:
                    return
                remaining = _COMMAND_OUTPUT_LIMIT - len(output)
                if remaining > 0:
                    output.extend(chunk[:remaining])
                if len(chunk) > remaining:
                    overflow.set()
                    return
        except (OSError, ValueError):
            return

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    deadline = time.monotonic() + timeout
    while process.poll() is None and not overflow.is_set():
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        overflow.wait(timeout=min(0.02, remaining))
    if process.poll() is None and not overflow.is_set():
        _terminate_probe_process(process)
        reader.join(timeout=0.2)
        process.stdout.close()
        return _CommandResult("timeout")
    if overflow.is_set():
        _terminate_probe_process(process)
        reader.join(timeout=0.2)
        process.stdout.close()
        return _CommandResult("error")
    returncode = process.wait()
    reader.join(timeout=0.2)
    process.stdout.close()
    return _CommandResult(
        "ok" if returncode == 0 else "nonzero",
        returncode,
        bytes(output).decode("utf-8", errors="replace"),
    )


def _terminate_probe_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        if os.name == "posix":
            os.killpg(process.pid, 15)
        else:
            process.terminate()
        process.wait(timeout=0.1)
    except (OSError, subprocess.TimeoutExpired):
        try:
            if os.name == "posix":
                os.killpg(process.pid, 9)
            else:
                process.kill()
        except OSError:
            pass


def _probe_windows() -> tuple[CapabilityCheck, ...]:
    return (
        _check_windows_uia(),
        _check_windows_session(),
        _check_windows_input_desktop(),
        _check_windows_integrity(),
        _check_windows_dpi(),
        _check_script_sandbox(),
    )


def _check_script_sandbox() -> CapabilityCheck:
    """Report whether script steps can run here, and which boundaries hold.

    This is deliberately not a boolean.  A ``degraded`` sandbox still executes
    scripts with kernel-enforced resource limits, an empty environment and an
    isolated interpreter, but leaves the boundaries named in ``gaps`` to the
    ambient user account.  Reporting that difference is the point: an operator
    deciding whether to pass ``--allow-scripts`` needs to know which
    boundaries are real on this host.
    """

    try:
        from .script import sandbox_availability

        report = sandbox_availability()
    except Exception as exc:  # pragma: no cover - defensive
        return CapabilityCheck(
            "script.sandbox",
            "unknown",
            "The script sandbox could not be inspected.",
            {"exception_type": type(exc).__name__},
        )

    state = str(report.get("state", "unknown"))
    mechanism = report.get("mechanism")
    gaps = tuple(report.get("gaps", ()))
    missing = tuple(report.get("missing", ()))
    evidence: dict[str, Any] = {
        "mechanism": mechanism,
        "enforced": (
            "memory_limit",
            "cpu_time_limit",
            "process_count_limit",
            "process_tree_reclamation",
            "empty_environment",
            "isolated_working_directory",
            "isolated_interpreter",
        )
        if state in {"available", "degraded"}
        else (),
        "not_enforced": gaps,
        "missing_prerequisites": missing,
    }
    if "interpreter" in report:
        # The path itself is withheld: an interpreter under a user profile
        # contains the account name, and the probe report must not carry
        # environment-identifying values.  What matters for diagnosis is that a
        # concrete interpreter was resolved and pinned, not where it lives.
        evidence["interpreter_resolved"] = bool(report["interpreter"])

    if state == "unavailable":
        return CapabilityCheck(
            "script.sandbox",
            "unavailable",
            "No supported script sandbox is available here, so script steps "
            "fail closed. Nothing was executed.",
            evidence,
        )
    if state == "degraded":
        return CapabilityCheck(
            "script.sandbox",
            "degraded",
            "Script steps can run with kernel-enforced resource limits, an "
            "empty environment and an isolated interpreter, but "
            f"{', '.join(gaps) if gaps else 'some boundaries'} are not "
            "isolated: a script retains the ambient user account's access "
            "there. Scripts still require --allow-scripts.",
            evidence,
        )
    return CapabilityCheck(
        "script.sandbox",
        "available",
        "Script steps can run with every documented boundary enforced; "
        "scripts still require --allow-scripts. Nothing was executed.",
        evidence,
    )


def _windows_dll(name: str) -> Any:
    """Load a system DLL by bare name, letting Windows resolve from System32.

    ``ctypes.WinDLL`` on a bare well-known system name resolves through the
    standard search order, which begins with the already-loaded module and
    System32.  The probe never accepts a caller-supplied library name.
    """

    return ctypes.WinDLL(name, use_last_error=True)  # type: ignore[attr-defined]


def _check_windows_session() -> CapabilityCheck:
    """Report whether this process sits in an interactive session.

    Session 0 isolation is the single most common reason a service-hosted
    automation host sees an empty desktop: it can create UIA objects and still
    have no interactive window station to drive.  The check is pure
    observation - it never attaches to another session or desktop.
    """

    evidence: dict[str, Any] = {
        "session_id_available": False,
        "interactive_session": None,
        "session_zero": None,
        "remote_session": None,
    }
    try:
        kernel = _windows_dll("kernel32")
        session_id = ctypes.c_uint32()
        kernel.ProcessIdToSessionId.argtypes = [
            ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32)
        ]
        kernel.ProcessIdToSessionId.restype = ctypes.c_int
        if not kernel.ProcessIdToSessionId(
            ctypes.c_uint32(os.getpid()), ctypes.byref(session_id)
        ):
            return CapabilityCheck(
                "windows.session",
                "unknown",
                "The session identifier for this process could not be read.",
                evidence,
            )
        evidence["session_id_available"] = True
        # The numeric id is deliberately not reported: it is environment
        # identifying detail, and only the Session 0 distinction is actionable.
        evidence["session_zero"] = session_id.value == 0
        evidence["interactive_session"] = session_id.value != 0

        user32 = _windows_dll("user32")
        user32.GetSystemMetrics.argtypes = [ctypes.c_int]
        user32.GetSystemMetrics.restype = ctypes.c_int
        evidence["remote_session"] = bool(
            user32.GetSystemMetrics(_SM_REMOTESESSION)
        )
    except (AttributeError, OSError) as exc:
        evidence["error_type"] = type(exc).__name__
        return CapabilityCheck(
            "windows.session",
            "unknown",
            "The Windows session prerequisite check could not run.",
            evidence,
        )
    if evidence["session_zero"]:
        return CapabilityCheck(
            "windows.session",
            "unavailable",
            "This process runs in Session 0, which has no interactive desktop; "
            "UI automation cannot reach user applications from here.",
            evidence,
        )
    return CapabilityCheck(
        "windows.session",
        "available",
        "This process runs in an interactive session; no desktop was attached "
        "and no window was enumerated.",
        evidence,
    )


def _check_windows_input_desktop() -> CapabilityCheck:
    """Report whether the process is on the window station receiving input.

    A process whose thread desktop is not the current input desktop can read
    parts of UIA yet never see or drive what the user sees.  While a UAC or
    credential prompt owns the input desktop, that desktop is the secure
    desktop and is unreachable to a normal process by design; this check
    observes that condition rather than trying to defeat it.
    """

    evidence: dict[str, Any] = {
        "window_station_named": False,
        "interactive_window_station": None,
        "input_desktop_readable": None,
        "thread_desktop_is_input_desktop": None,
    }
    input_desktop = None
    user32 = None
    try:
        user32 = _windows_dll("user32")
        kernel = _windows_dll("kernel32")
        user32.GetProcessWindowStation.restype = ctypes.c_void_p
        user32.GetThreadDesktop.argtypes = [ctypes.c_uint32]
        user32.GetThreadDesktop.restype = ctypes.c_void_p
        user32.OpenInputDesktop.argtypes = [
            ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32
        ]
        user32.OpenInputDesktop.restype = ctypes.c_void_p
        user32.CloseDesktop.argtypes = [ctypes.c_void_p]
        user32.CloseDesktop.restype = ctypes.c_int
        kernel.GetCurrentThreadId.restype = ctypes.c_uint32

        station = user32.GetProcessWindowStation()
        station_name = _windows_user_object_name(user32, station)
        if station_name is not None:
            evidence["window_station_named"] = True
            # WinSta0 is the only interactive window station.  The name itself
            # is a fixed OS constant, so reporting the comparison is safe.
            evidence["interactive_window_station"] = (
                station_name.upper() == "WINSTA0"
            )

        thread_desktop = user32.GetThreadDesktop(kernel.GetCurrentThreadId())
        thread_desktop_name = _windows_user_object_name(user32, thread_desktop)

        input_desktop = user32.OpenInputDesktop(0, False, _DESKTOP_READOBJECTS)
        if not input_desktop:
            # Access is denied while the secure desktop is in front, which is
            # a legitimate state rather than a misconfiguration.
            evidence["input_desktop_readable"] = False
            evidence["last_error"] = ctypes.get_last_error()
            return CapabilityCheck(
                "windows.input_desktop",
                "degraded",
                "The current input desktop cannot be opened; a secure desktop "
                "or another session may own user input.",
                evidence,
            )
        evidence["input_desktop_readable"] = True
        input_desktop_name = _windows_user_object_name(user32, input_desktop)
        if thread_desktop_name is not None and input_desktop_name is not None:
            evidence["thread_desktop_is_input_desktop"] = (
                thread_desktop_name == input_desktop_name
            )
    except (AttributeError, OSError) as exc:
        evidence["error_type"] = type(exc).__name__
        return CapabilityCheck(
            "windows.input_desktop",
            "unknown",
            "The Windows desktop prerequisite check could not run.",
            evidence,
        )
    finally:
        if input_desktop and user32 is not None:
            try:
                user32.CloseDesktop(input_desktop)
            except OSError:
                pass

    if evidence["interactive_window_station"] is False:
        return CapabilityCheck(
            "windows.input_desktop",
            "unavailable",
            "This process is not on the interactive window station, so it "
            "cannot observe or drive the user's desktop.",
            evidence,
        )
    if evidence["thread_desktop_is_input_desktop"] is False:
        return CapabilityCheck(
            "windows.input_desktop",
            "degraded",
            "This thread's desktop is not the desktop currently receiving "
            "input; injected input would not reach the visible session.",
            evidence,
        )
    if evidence["thread_desktop_is_input_desktop"] is None:
        return CapabilityCheck(
            "windows.input_desktop",
            "unknown",
            "The relationship between this thread's desktop and the input "
            "desktop could not be established.",
            evidence,
        )
    return CapabilityCheck(
        "windows.input_desktop",
        "available",
        "This process is on the interactive window station and its thread "
        "desktop is the current input desktop; no input was injected.",
        evidence,
    )


def _windows_user_object_name(user32: Any, handle: Any) -> str | None:
    """Read a window station or desktop name, or None when unavailable."""

    if not handle:
        return None
    try:
        user32.GetUserObjectInformationW.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.POINTER(ctypes.c_uint32),
        ]
        user32.GetUserObjectInformationW.restype = ctypes.c_int
        buffer = ctypes.create_unicode_buffer(256)
        needed = ctypes.c_uint32()
        if not user32.GetUserObjectInformationW(
            handle,
            _UOI_NAME,
            buffer,
            ctypes.sizeof(buffer),
            ctypes.byref(needed),
        ):
            return None
        return buffer.value or None
    except (AttributeError, OSError, ValueError):
        return None


def _check_windows_integrity() -> CapabilityCheck:
    """Report this process's integrity level and elevation.

    UIPI blocks input and most window messages from a lower-integrity process
    to a higher-integrity one.  A medium-integrity host therefore cannot drive
    an elevated application even though every UIA object creates successfully,
    which is precisely the failure that looks like a driver bug at runtime.
    """

    evidence: dict[str, Any] = {
        "token_readable": False,
        "elevated": None,
        "integrity_level": None,
    }
    token = ctypes.c_void_p()
    kernel = None
    try:
        advapi = _windows_dll("advapi32")
        kernel = _windows_dll("kernel32")
        kernel.GetCurrentProcess.restype = ctypes.c_void_p
        advapi.OpenProcessToken.argtypes = [
            ctypes.c_void_p, ctypes.c_uint32, ctypes.POINTER(ctypes.c_void_p)
        ]
        advapi.OpenProcessToken.restype = ctypes.c_int
        if not advapi.OpenProcessToken(
            kernel.GetCurrentProcess(), _TOKEN_QUERY, ctypes.byref(token)
        ):
            return CapabilityCheck(
                "windows.integrity",
                "unknown",
                "The process token could not be opened for the UIPI check.",
                evidence,
            )
        evidence["token_readable"] = True

        advapi.GetTokenInformation.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.POINTER(ctypes.c_uint32),
        ]
        advapi.GetTokenInformation.restype = ctypes.c_int

        elevation = ctypes.c_uint32()
        returned = ctypes.c_uint32()
        if advapi.GetTokenInformation(
            token,
            _TokenElevation,
            ctypes.byref(elevation),
            ctypes.sizeof(elevation),
            ctypes.byref(returned),
        ):
            evidence["elevated"] = bool(elevation.value)

        evidence["integrity_level"] = _windows_integrity_level(advapi, token)
    except (AttributeError, OSError) as exc:
        evidence["error_type"] = type(exc).__name__
        return CapabilityCheck(
            "windows.integrity",
            "unknown",
            "The Windows integrity prerequisite check could not run.",
            evidence,
        )
    finally:
        if token and kernel is not None:
            try:
                kernel.CloseHandle.argtypes = [ctypes.c_void_p]
                kernel.CloseHandle(token)
            except OSError:
                pass

    level = evidence["integrity_level"]
    if level in {"untrusted", "low"}:
        return CapabilityCheck(
            "windows.integrity",
            "unavailable",
            f"This process runs at {level} integrity, so UIPI blocks input to "
            "ordinary applications.",
            evidence,
        )
    if level is None:
        return CapabilityCheck(
            "windows.integrity",
            "unknown",
            "The integrity level of this process could not be determined.",
            evidence,
        )
    # Medium integrity is the normal, expected case.  It is reported as
    # available with the UIPI ceiling stated, not as degraded, because nothing
    # is misconfigured; driving an elevated target is simply out of scope.
    return CapabilityCheck(
        "windows.integrity",
        "available",
        f"This process runs at {level} integrity; UIPI still prevents driving "
        "any application at a higher integrity level.",
        evidence,
    )


def _windows_integrity_level(advapi: Any, token: Any) -> str | None:
    """Map the token's mandatory label RID onto a stable name."""

    try:
        size = ctypes.c_uint32()
        advapi.GetTokenInformation(
            token, _TokenIntegrityLevel, None, 0, ctypes.byref(size)
        )
        if not size.value:
            return None
        buffer = ctypes.create_string_buffer(size.value)
        if not advapi.GetTokenInformation(
            token,
            _TokenIntegrityLevel,
            buffer,
            size.value,
            ctypes.byref(size),
        ):
            return None
        advapi.GetSidSubAuthorityCount.argtypes = [ctypes.c_void_p]
        advapi.GetSidSubAuthorityCount.restype = ctypes.POINTER(ctypes.c_ubyte)
        advapi.GetSidSubAuthority.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        advapi.GetSidSubAuthority.restype = ctypes.POINTER(ctypes.c_uint32)
        # TOKEN_MANDATORY_LABEL begins with a SID pointer.
        sid = ctypes.cast(buffer, ctypes.POINTER(ctypes.c_void_p))[0]
        if not sid:
            return None
        count = advapi.GetSidSubAuthorityCount(sid)[0]
        if count <= 0:
            return None
        rid = int(advapi.GetSidSubAuthority(sid, count - 1)[0])
    except (AttributeError, OSError, ValueError, IndexError):
        return None
    for threshold, name in _INTEGRITY_LEVELS:
        if rid >= threshold:
            return name
    return "untrusted"


def _check_windows_dpi() -> CapabilityCheck:
    """Report DPI awareness and the resulting pointer-coordinate resolution.

    UIA bounding rectangles and ``GetSystemMetrics`` are reported in the same
    space as each other, so an unaware host is internally consistent and does
    not mis-aim clicks.  What it loses is resolution: on a scaled display the
    whole desktop is addressed through a virtualized coordinate grid, so
    distinct physical pixels collapse onto one addressable point.  This check
    reports that quantisation instead of implying a coordinate mismatch.
    """

    evidence: dict[str, Any] = {
        "awareness_readable": False,
        "awareness": None,
        "scaled_display": None,
        "pointer_quantisation": None,
    }
    try:
        try:
            shcore = _windows_dll("shcore")
            shcore.GetProcessDpiAwareness.argtypes = [
                ctypes.c_void_p, ctypes.POINTER(ctypes.c_int)
            ]
            shcore.GetProcessDpiAwareness.restype = ctypes.c_long
            awareness = ctypes.c_int()
            if shcore.GetProcessDpiAwareness(None, ctypes.byref(awareness)) == 0:
                evidence["awareness_readable"] = True
                evidence["awareness"] = _DPI_AWARENESS.get(
                    awareness.value, "unknown"
                )
        except OSError:
            # shcore.dll predates Windows 8.1; an older system is always the
            # equivalent of system-DPI awareness.
            evidence["awareness"] = "unsupported_api"

        user32 = _windows_dll("user32")
        gdi32 = _windows_dll("gdi32")
        user32.GetSystemMetrics.argtypes = [ctypes.c_int]
        user32.GetSystemMetrics.restype = ctypes.c_int
        user32.GetDC.argtypes = [ctypes.c_void_p]
        user32.GetDC.restype = ctypes.c_void_p
        user32.ReleaseDC.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        user32.ReleaseDC.restype = ctypes.c_int
        gdi32.GetDeviceCaps.argtypes = [ctypes.c_void_p, ctypes.c_int]
        gdi32.GetDeviceCaps.restype = ctypes.c_int

        reported_width = int(user32.GetSystemMetrics(_SM_CXSCREEN))
        device = user32.GetDC(None)
        if device:
            try:
                physical_width = int(
                    gdi32.GetDeviceCaps(device, _DESKTOPHORZRES)
                )
            finally:
                user32.ReleaseDC(None, device)
        else:
            physical_width = 0

        if reported_width > 0 and physical_width > 0:
            evidence["scaled_display"] = physical_width != reported_width
            # Physical pixels per addressable coordinate step.  1.0 means the
            # host can aim at every physical pixel.
            evidence["pointer_quantisation"] = round(
                physical_width / reported_width, 4
            )
    except (AttributeError, OSError) as exc:
        evidence["error_type"] = type(exc).__name__
        return CapabilityCheck(
            "windows.dpi",
            "unknown",
            "The Windows DPI prerequisite check could not run.",
            evidence,
        )

    quantisation = evidence["pointer_quantisation"]
    if quantisation is None:
        return CapabilityCheck(
            "windows.dpi",
            "unknown",
            "Display scaling could not be determined for this process.",
            evidence,
        )
    if quantisation > 1.0:
        return CapabilityCheck(
            "windows.dpi",
            "degraded",
            "This process is not DPI aware on a scaled display, so pointer "
            f"coordinates quantise to about {quantisation} physical pixels; "
            "targets smaller than that step cannot be addressed exactly.",
            evidence,
        )
    return CapabilityCheck(
        "windows.dpi",
        "available",
        "Pointer coordinates map one-to-one onto physical pixels for this "
        "process; no input was injected.",
        evidence,
    )


def _check_windows_uia() -> CapabilityCheck:
    evidence: dict[str, Any] = {
        "runtime_loaded": False,
        "com_initialized": False,
        "automation_object_created": False,
        "tree_access_attempted": False,
    }
    initialized = False
    automation = ctypes.c_void_p()
    try:
        ctypes.WinDLL("UIAutomationCore.dll")  # type: ignore[attr-defined]
        evidence["runtime_loaded"] = True
        ole32 = ctypes.WinDLL("ole32.dll")  # type: ignore[attr-defined]

        class _GUID(ctypes.Structure):
            _fields_ = [
                ("Data1", ctypes.c_uint32),
                ("Data2", ctypes.c_uint16),
                ("Data3", ctypes.c_uint16),
                ("Data4", ctypes.c_ubyte * 8),
            ]

        def guid(value: str) -> _GUID:
            parsed = uuid.UUID(value)
            tail = bytes((parsed.clock_seq_hi_variant, parsed.clock_seq_low)) + parsed.node.to_bytes(6, "big")
            return _GUID(parsed.time_low, parsed.time_mid, parsed.time_hi_version, (ctypes.c_ubyte * 8)(*tail))

        ole32.CoInitializeEx.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        ole32.CoInitializeEx.restype = ctypes.c_long
        init_hr = int(ole32.CoInitializeEx(None, 2)) & 0xFFFFFFFF
        initialized = init_hr in {0, 1}
        if init_hr not in {0, 1, 0x80010106}:  # RPC_E_CHANGED_MODE is safe to continue.
            evidence["hresult"] = f"0x{init_hr:08x}"
            return CapabilityCheck(
                "windows.uia", "unknown", "COM could not be initialized for the UIA prerequisite check.", evidence
            )
        evidence["com_initialized"] = True

        clsid = guid("ff48dba4-60ef-4201-aa87-54103eef594e")
        iid = guid("30cbe57d-d9d0-452a-ab13-7ac5ac4825ee")
        ole32.CoCreateInstance.argtypes = [
            ctypes.POINTER(_GUID), ctypes.c_void_p, ctypes.c_uint32, ctypes.POINTER(_GUID), ctypes.POINTER(ctypes.c_void_p)
        ]
        ole32.CoCreateInstance.restype = ctypes.c_long
        create_hr = int(ole32.CoCreateInstance(ctypes.byref(clsid), None, 1, ctypes.byref(iid), ctypes.byref(automation))) & 0xFFFFFFFF
        if create_hr != 0 or not automation.value:
            evidence["hresult"] = f"0x{create_hr:08x}"
            return CapabilityCheck(
                "windows.uia", "unavailable", "The UI Automation COM object could not be created.", evidence
            )
        evidence["automation_object_created"] = True
        return CapabilityCheck(
            "windows.uia",
            "available",
            "The UIA runtime loaded and its base COM object was created; no UI tree was read.",
            evidence,
        )
    except (AttributeError, OSError) as exc:
        evidence["error_type"] = type(exc).__name__
        return CapabilityCheck(
            "windows.uia", "unavailable", "The Windows UI Automation runtime is not loadable.", evidence
        )
    except Exception as exc:
        evidence["error_type"] = type(exc).__name__
        return CapabilityCheck(
            "windows.uia", "unknown", "The UI Automation prerequisite check did not complete.", evidence
        )
    finally:
        if automation.value:
            try:
                table = ctypes.cast(automation, ctypes.POINTER(ctypes.POINTER(ctypes.c_void_p))).contents
                release = ctypes.WINFUNCTYPE(ctypes.c_ulong, ctypes.c_void_p)(table[2])  # type: ignore[attr-defined]
                release(automation)
            except Exception:
                pass
        if initialized:
            try:
                ole32.CoUninitialize()
            except Exception:
                pass


def _call_boolean_symbol(library: str, symbol: str) -> tuple[str, bool | None]:
    try:
        framework = ctypes.CDLL(library)
        function = getattr(framework, symbol)
        function.argtypes = []
        function.restype = ctypes.c_bool
        return "ok", bool(function())
    except AttributeError:
        return "missing_symbol", None
    except OSError:
        return "load_error", None
    except Exception:
        return "error", None


def _probe_macos() -> tuple[CapabilityCheck, ...]:
    application_services = "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices"
    core_graphics = "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics"
    ax_outcome, ax_trusted = _call_boolean_symbol(application_services, "AXIsProcessTrusted")
    capture_outcome, capture_allowed = _call_boolean_symbol(core_graphics, "CGPreflightScreenCaptureAccess")

    if ax_outcome == "ok":
        ax = CapabilityCheck(
            "macos.accessibility",
            "available" if ax_trusted else "unavailable",
            "Accessibility trust is granted to this process identity." if ax_trusted else "Accessibility trust is not granted to this process identity.",
            {"preflight_completed": True, "authorized": bool(ax_trusted), "prompt_requested": False},
        )
    else:
        ax = CapabilityCheck(
            "macos.accessibility", "unknown", "Accessibility trust could not be checked without prompting.",
            {"preflight_completed": False, "authorized": None, "prompt_requested": False, "outcome": ax_outcome},
        )

    if capture_outcome == "ok":
        capture = CapabilityCheck(
            "macos.screen_capture",
            "available" if capture_allowed else "unavailable",
            "Screen Capture permission is granted to this process identity." if capture_allowed else "Screen Capture permission is not granted to this process identity.",
            {"preflight_completed": True, "authorized": bool(capture_allowed), "prompt_requested": False, "capture_attempted": False},
        )
    else:
        capture = CapabilityCheck(
            "macos.screen_capture", "unknown", "Screen Capture permission could not be checked with the safe preflight API.",
            {"preflight_completed": False, "authorized": None, "prompt_requested": False, "capture_attempted": False, "outcome": capture_outcome},
        )
    return ax, capture, _check_script_sandbox()


def _path_state(path: Path) -> str:
    try:
        mode = path.stat().st_mode
    except FileNotFoundError:
        return "missing"
    except OSError:
        return "unknown"
    return "socket" if stat.S_ISSOCK(mode) else "other"


def _probe_linux_atspi(environ: Mapping[str, str]) -> CapabilityCheck:
    gdbus_path = _which("gdbus")
    gdbus = gdbus_path is not None
    evidence: dict[str, Any] = {
        "address_advertised": bool(environ.get("AT_SPI_BUS_ADDRESS")),
        "session_bus_advertised": bool(environ.get("DBUS_SESSION_BUS_ADDRESS")),
        "libatspi_found": _library_found("atspi"),
        "gdbus_found": gdbus,
        "query": "not_run",
    }
    if gdbus and evidence["session_bus_advertised"]:
        assert gdbus_path is not None
        result = _run_read_only((
            gdbus_path, "call", "--session", "--dest", "org.a11y.Bus",
            "--object-path", "/org/a11y/bus", "--method", "org.a11y.Bus.GetAddress",
        ), environ=environ, pass_environment=("DBUS_SESSION_BUS_ADDRESS",))
        evidence["query"] = result.outcome
        evidence["returncode"] = result.returncode
        if result.outcome == "ok" and result.stdout.strip():
            return CapabilityCheck("linux.at_spi", "available", "The session AT-SPI bus returned an address.", evidence)
        if result.outcome in {"timeout", "error"}:
            return CapabilityCheck("linux.at_spi", "unknown", "The AT-SPI bus query could not be completed.", evidence)
        if evidence["address_advertised"] or evidence["libatspi_found"]:
            return CapabilityCheck("linux.at_spi", "degraded", "AT-SPI components are visible, but the session bus did not answer.", evidence)
        return CapabilityCheck("linux.at_spi", "unknown", "The advertised session bus did not provide a verified AT-SPI address.", evidence)
    if evidence["address_advertised"]:
        return CapabilityCheck("linux.at_spi", "degraded", "An AT-SPI address is advertised, but no safe query tool is installed.", evidence)
    if evidence["libatspi_found"]:
        return CapabilityCheck("linux.at_spi", "unknown", "libatspi is installed, but bus availability could not be checked.", evidence)
    return CapabilityCheck("linux.at_spi", "unavailable", "No AT-SPI bus signal or client library was found.", evidence)


def _probe_linux_x11(environ: Mapping[str, str]) -> CapabilityCheck:
    display = bool(environ.get("DISPLAY"))
    # ``xdpyinfo`` dumps every visual by default and can easily exceed the
    # generic probe output cap on modern or remote displays.  ``xprop`` with
    # one root-window atom is a bounded, read-only round trip that proves the
    # advertised display and Xauthority are usable without consuming pixels
    # or injecting input.
    tool_path = _which("xprop")
    tool = tool_path is not None
    evidence: dict[str, Any] = {
        "display_advertised": display,
        "libx11_found": _library_found("X11"),
        "xprop_found": tool,
        "query": "not_run",
    }
    if not display:
        return CapabilityCheck("linux.x11", "unavailable", "No X11 display is advertised to this process.", evidence)
    if not tool:
        return CapabilityCheck("linux.x11", "degraded", "An X11 display is advertised, but it was not possible to query it.", evidence)
    assert tool_path is not None
    result = _run_read_only(
        (tool_path, "-root", "_NET_SUPPORTING_WM_CHECK"),
        environ=environ,
        pass_environment=("DISPLAY", "XAUTHORITY"),
    )
    evidence["query"] = result.outcome
    evidence["returncode"] = result.returncode
    if result.outcome == "ok":
        return CapabilityCheck("linux.x11", "available", "The advertised X11 display answered a metadata query.", evidence)
    if result.outcome in {"error", "timeout"}:
        return CapabilityCheck("linux.x11", "unknown", "The X11 query could not be executed.", evidence)
    return CapabilityCheck("linux.x11", "degraded", "An X11 display is advertised but did not answer the bounded query.", evidence)


def _probe_linux_wayland(environ: Mapping[str, str]) -> CapabilityCheck:
    display = environ.get("WAYLAND_DISPLAY")
    runtime = environ.get("XDG_RUNTIME_DIR")
    socket_state = "not_checked"
    if display:
        endpoint = Path(display) if os.path.isabs(display) else Path(runtime, display) if runtime else None
        if endpoint is not None:
            socket_state = _path_state(endpoint)
    evidence = {"display_advertised": bool(display), "runtime_dir_advertised": bool(runtime), "socket_state": socket_state}
    if not display:
        return CapabilityCheck("linux.wayland", "unavailable", "No Wayland display is advertised to this process.", evidence)
    if socket_state == "socket":
        return CapabilityCheck("linux.wayland", "available", "The advertised Wayland endpoint exists as a socket.", evidence)
    return CapabilityCheck("linux.wayland", "degraded", "Wayland is advertised, but its endpoint could not be confirmed.", evidence)


def _probe_linux_portal(environ: Mapping[str, str]) -> CapabilityCheck:
    gdbus_path = _which("gdbus")
    gdbus = gdbus_path is not None
    evidence: dict[str, Any] = {"gdbus_found": gdbus, "session_bus_advertised": bool(environ.get("DBUS_SESSION_BUS_ADDRESS")), "query": "not_run", "permission_requested": False, "session_created": False}
    if not gdbus:
        return CapabilityCheck("linux.remote_desktop_portal", "unknown", "The RemoteDesktop portal interface could not be queried.", evidence)
    if not evidence["session_bus_advertised"]:
        return CapabilityCheck("linux.remote_desktop_portal", "unavailable", "No D-Bus session address is available for the RemoteDesktop portal.", evidence)
    assert gdbus_path is not None
    result = _run_read_only((
        gdbus_path, "call", "--session", "--dest", "org.freedesktop.portal.Desktop",
        "--object-path", "/org/freedesktop/portal/desktop",
        "--method", "org.freedesktop.DBus.Properties.Get",
        "org.freedesktop.portal.RemoteDesktop", "version",
    ), environ=environ, pass_environment=("DBUS_SESSION_BUS_ADDRESS",))
    evidence["query"] = result.outcome
    evidence["returncode"] = result.returncode
    if result.outcome == "ok":
        return CapabilityCheck("linux.remote_desktop_portal", "available", "The RemoteDesktop portal interface is exposed; authorization was not requested.", evidence)
    if result.outcome in {"timeout", "error", "nonzero"}:
        return CapabilityCheck("linux.remote_desktop_portal", "unknown", "The RemoteDesktop portal query could not be completed.", evidence)
    return CapabilityCheck("linux.remote_desktop_portal", "unknown", "The RemoteDesktop portal interface could not be verified.", evidence)


def _probe_linux_libei() -> CapabilityCheck:
    evidence = {
        "libei_found": _library_found("ei"),
        "liboeffis_found": _library_found("oeffis"),
        "ei_debug_events_found": _which("ei-debug-events") is not None,
        "ei_demo_found": _which("ei-demo") is not None,
        "connection_attempted": False,
    }
    if evidence["libei_found"]:
        return CapabilityCheck("linux.libei", "available", "The libei client library is discoverable; no compositor connection was attempted.", evidence)
    if any((evidence["liboeffis_found"], evidence["ei_debug_events_found"], evidence["ei_demo_found"])):
        return CapabilityCheck("linux.libei", "degraded", "Some libei tooling is installed, but the client library was not found.", evidence)
    return CapabilityCheck("linux.libei", "unavailable", "No libei client library or known diagnostic command was found.", evidence)


def _uinput_device() -> tuple[str | None, str, bool, bool]:
    for raw in ("/dev/uinput", "/dev/input/uinput"):
        path = Path(raw)
        try:
            mode = path.stat().st_mode
        except FileNotFoundError:
            continue
        except OSError:
            return raw, "unknown", False, False
        return raw, "character_device" if stat.S_ISCHR(mode) else "other", os.access(path, os.R_OK), os.access(path, os.W_OK)
    return None, "missing", False, False


def _probe_linux_uinput() -> CapabilityCheck:
    device, device_state, readable, writable = _uinput_device()
    evidence = {
        "device": device, "device_state": device_state, "readable": readable, "writable": writable,
        "libevdev_found": _library_found("evdev"), "ydotool_found": _which("ydotool") is not None,
        "evemu_device_found": _which("evemu-device") is not None, "device_opened": False,
    }
    if device_state == "character_device" and writable:
        return CapabilityCheck("linux.uinput", "available", "Write access to a uinput character device is advertised for the current process; it was not opened.", evidence)
    if device_state != "missing":
        return CapabilityCheck("linux.uinput", "degraded", "A uinput path exists, but current-process access is incomplete or uncertain.", evidence)
    return CapabilityCheck("linux.uinput", "unavailable", "No uinput device is exposed to the current process.", evidence)


def _probe_linux(environ: Mapping[str, str]) -> tuple[CapabilityCheck, ...]:
    return (
        _probe_linux_atspi(environ),
        _probe_linux_x11(environ),
        _probe_linux_wayland(environ),
        _probe_linux_portal(environ),
        _probe_linux_libei(),
        _probe_linux_uinput(),
        _check_script_sandbox(),
    )


def probe_capabilities(*, environ: Mapping[str, str] | None = None) -> ProbeReport:
    """Observe platform prerequisites without requesting or exercising them."""

    environment = os.environ if environ is None else environ
    system = platform.system()
    name = _canonical_platform(system)
    platform_info = {
        "name": name,
        "system": system or "unknown",
        "release": platform.release() or "unknown",
        "version": platform.version() or "unknown",
        "machine": platform.machine() or "unknown",
        "python": platform.python_version(),
    }
    if name == "windows":
        checks = _probe_windows()
    elif name == "macos":
        checks = _probe_macos()
    elif name == "linux":
        checks = _probe_linux(environment)
    else:
        checks = (CapabilityCheck("platform.supported", "unavailable", "This operating system has no platform-specific probe.", {"system_known": bool(system)}),)
    return ProbeReport(platform_info, _session_info(name, environment), checks)


__all__ = ["CapabilityCheck", "ProbeReport", "probe_capabilities"]
