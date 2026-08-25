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
_TRUSTED_COMMAND_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"
_COMMAND_OUTPUT_LIMIT = 64 * 1024
_SYSTEM_LIBRARY_DIRECTORIES = (
    Path("/lib"),
    Path("/lib64"),
    Path("/usr/lib"),
    Path("/usr/lib64"),
)


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
    return shutil.which(command, path=_TRUSTED_COMMAND_PATH)


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
    if not argv or not Path(argv[0]).is_absolute():
        return _CommandResult("error")
    source_environment = os.environ if environ is None else environ
    child_environment = {
        "PATH": _TRUSTED_COMMAND_PATH,
        "LANG": "C",
        "LC_ALL": "C",
    }
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
    return (_check_windows_uia(),)


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
    return ax, capture


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
