#!/usr/bin/env python3
"""Qualify owned KDE/X11 applications through the production AT-SPI driver.

The runner never attaches to an existing application.  It starts a private
session D-Bus, uses private XDG/HOME directories, launches every candidate in
its own process group, and accepts only the exact PID returned by ``Popen``.
The default qualification is entirely read-only after application launch.
"""

from __future__ import annotations

import argparse
from collections import Counter
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any, Mapping


PROJECT_ROOT = Path(__file__).resolve().parents[2]
SRC_ROOT = PROJECT_ROOT / "src"
DRIVER_PATH = PROJECT_ROOT / "plugins" / "linux_atspi" / "linux_atspi_driver.py"
QML_FIXTURE_PATH = PROJECT_ROOT / "tests" / "linux" / "qml_atspi_fixture.qml"
if str(SRC_ROOT) not in sys.path:
    sys.path.insert(0, str(SRC_ROOT))

from ai_auto_desktop.plugin import PluginError, ProcessPlugin  # noqa: E402


SCHEMA_VERSION = "ai-auto-desktop.kde-x11-qualification/v1"
SESSION_KEYS = (
    "DISPLAY",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
)
QUALIFIER_ENVIRONMENT_KEYS = frozenset({
    *SESSION_KEYS,
    "AI_AUTO_DESKTOP_QUALIFIER_PRIVATE_BUS",
    "AI_AUTO_DESKTOP_TEST_ATSPI_TYPELIB_PATH",
    "DBUS_SESSION_BUS_ADDRESS",
    "HOME",
    "KDE_FULL_SESSION",
    "KDE_SESSION_VERSION",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
})
STATE_NAMES = (
    "enabled", "visible", "showing", "focusable", "focused",
    "editable", "sensitive", "protected", "checked",
    "expandable", "expanded", "selectable", "selected",
)
APP_SPECS: dict[str, dict[str, Any]] = {
    "dolphin": {
        "executables": ("dolphin",),
        # The qualifier supplies a private, empty directory below its
        # temporary work root.  Dolphin therefore cannot enumerate or mutate
        # the operator's real home directory during this read-only audit.
        "launch_args": ("--new-window", "{fixture_dir}"),
    },
    "konsole": {
        "executables": ("konsole",),
        "launch_args": (
            "--separate", "--nofork", "--builtin-profile",
            "--hide-menubar", "--hide-tabbar",
            "-e", "/bin/sleep", "60",
        ),
    },
    "qml-fixture": {
        "executables": ("qmlscene", "qmlscene-qt5"),
        "launch_args": (str(QML_FIXTURE_PATH),),
        # qmlscene treats --version as a scene URL and opens an error window.
        # Qt itself is recorded separately by the host qmake probe.
        "version_args": None,
    },
    "system-settings": {
        "executables": ("systemsettings5", "systemsettings"),
        "launch_args": (),
    },
}


def sanitized_environment(
    source: Mapping[str, str],
) -> dict[str, str]:
    """Keep only display/runtime values needed by owned test processes."""

    result = {
        key: value for key, value in source.items()
        if key in QUALIFIER_ENVIRONMENT_KEYS and value
    }
    result["PATH"] = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin"
    result["LANG"] = "C.UTF-8"
    result["LC_ALL"] = "C.UTF-8"
    return result


def _ratio(numerator: int, denominator: int) -> float | None:
    return None if denominator == 0 else round(numerator / denominator, 4)


def summarize_nodes(nodes: list[dict[str, Any]]) -> dict[str, Any]:
    """Return aggregate semantic completeness without retaining UI text."""

    total = len(nodes)
    roles = Counter(str(node.get("role") or "") for node in nodes)
    roles.pop("", None)
    actions = Counter(
        str(action)
        for node in nodes
        for action in node.get("actions", [])
        if isinstance(action, str) and action
    )
    native_actions = Counter(
        str(node.get("provenance", {}).get("native_action_name"))
        for node in nodes
        if isinstance(node.get("provenance"), dict)
        and node["provenance"].get("native_action_name")
    )

    def field(name: str) -> dict[str, Any]:
        non_null = sum(node.get(name) is not None for node in nodes)
        non_empty = sum(
            isinstance(node.get(name), str) and bool(node[name]) for node in nodes
        )
        return {
            "non_null": non_null,
            "non_empty": non_empty,
            "non_null_ratio": _ratio(non_null, total),
            "non_empty_ratio": _ratio(non_empty, total),
        }

    state_known: dict[str, int] = {}
    for name in STATE_NAMES:
        state_known[name] = sum(
            isinstance(node.get("states"), dict)
            and isinstance(node["states"].get(name), bool)
            for node in nodes
        )
    known_total = sum(state_known.values())
    possible_states = total * len(STATE_NAMES)
    return {
        "element_count": total,
        "role": {
            "non_empty": sum(roles.values()),
            "non_empty_ratio": _ratio(sum(roles.values()), total),
            "counts": dict(sorted(roles.items())),
        },
        "name": field("name"),
        "value": field("value"),
        "description": field("description"),
        "state": {
            "known": known_total,
            "possible": possible_states,
            "known_ratio": _ratio(known_total, possible_states),
            "known_by_state": state_known,
        },
        "semantic_actions": {
            "node_action_counts": dict(sorted(actions.items())),
            "native_action_name_counts": dict(sorted(native_actions.items())),
        },
    }


def _read_process_environment(pid: int) -> dict[str, str]:
    raw = (Path("/proc") / str(pid) / "environ").read_bytes()
    if len(raw) > 1024 * 1024:
        raise ValueError("process environment exceeds qualification limit")
    allowed = set(SESSION_KEYS)
    values: dict[str, str] = {}
    for field in raw.split(b"\0"):
        if b"=" not in field:
            continue
        key_raw, value_raw = field.split(b"=", 1)
        key = key_raw.decode("utf-8", errors="strict")
        if key in allowed:
            values[key] = value_raw.decode("utf-8", errors="strict")
    return values


def discover_kde_x11_session(display: str | None = None) -> dict[str, str] | None:
    current = {key: os.environ[key] for key in SESSION_KEYS if os.environ.get(key)}
    if display:
        current["DISPLAY"] = display
    desktops = {
        part.upper()
        for part in re.split(r"[:;]", current.get("XDG_CURRENT_DESKTOP", ""))
        if part
    }
    if (
        current.get("XDG_SESSION_TYPE", "").lower() == "x11"
        and "KDE" in desktops
        and current.get("DISPLAY")
    ):
        return current

    proc = Path("/proc")
    if not proc.is_dir():
        return None
    for entry in sorted(proc.iterdir(), key=lambda item: item.name):
        if not entry.name.isdigit():
            continue
        try:
            if entry.stat().st_uid != os.getuid():
                continue
            if (entry / "comm").read_text(encoding="utf-8").strip() != "kwin_x11":
                continue
            candidate = _read_process_environment(int(entry.name))
        except (OSError, UnicodeError, ValueError):
            continue
        desktops = {
            part.upper()
            for part in re.split(
                r"[:;]", candidate.get("XDG_CURRENT_DESKTOP", "")
            )
            if part
        }
        if (
            candidate.get("XDG_SESSION_TYPE", "").lower() == "x11"
            and "KDE" in desktops
            and candidate.get("DISPLAY")
        ):
            if display:
                candidate["DISPLAY"] = display
            return candidate
    return None


def _command_output(
    argv: list[str], environment: dict[str, str]
) -> dict[str, Any]:
    process: subprocess.Popen[Any] | None = None
    try:
        # A Qt command can activate private D-Bus services which inherit its
        # descriptors.  A temporary file avoids waiting forever for an
        # inherited PIPE after the command itself has exited.
        output = tempfile.TemporaryFile()
        process = subprocess.Popen(
            argv, env=environment, stdin=subprocess.DEVNULL,
            stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
        )
        process.wait(timeout=5)
        output.seek(0)
        text = output.read(4096).decode("utf-8", errors="replace").strip()
        output.close()
    except (OSError, subprocess.TimeoutExpired) as exc:
        if process is not None and process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=2)
            except (OSError, subprocess.TimeoutExpired):
                pass
        return {"available": False, "error": type(exc).__name__}
    return {
        "available": True,
        "returncode": process.returncode,
        "output": text[:1000],
    }


def _error(stage: str, exc: BaseException) -> dict[str, Any]:
    if isinstance(exc, PluginError):
        return {"stage": stage, **exc.to_dict()}
    return {
        "stage": stage,
        "code": type(exc).__name__,
        "message": str(exc)[:2000],
        "retryable": False,
    }


def _owned_exact_pid(process: subprocess.Popen[Any], pid: int) -> bool:
    if pid != process.pid or process.poll() is not None:
        return False
    try:
        return (
            os.getpgid(pid) == process.pid
            and (Path("/proc") / str(pid)).stat().st_uid == os.getuid()
        )
    except (OSError, ProcessLookupError):
        return False


def _stop_owned_process(process: subprocess.Popen[Any]) -> None:
    if process.poll() is not None:
        return
    try:
        if os.getpgid(process.pid) != process.pid:
            return
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        try:
            if os.getpgid(process.pid) == process.pid:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
        except (OSError, ProcessLookupError, subprocess.TimeoutExpired):
            pass
    except (OSError, ProcessLookupError):
        pass


def _process_start_time(pid: int) -> int | None:
    """Return Linux /proc starttime without being confused by ')' in comm."""

    try:
        raw = (Path("/proc") / str(pid) / "stat").read_text(encoding="ascii")
        fields = raw[raw.rindex(")") + 2:].split()
        return int(fields[19])
    except (OSError, UnicodeError, ValueError, IndexError):
        return None


def _process_table() -> dict[int, tuple[int, int, int]]:
    """Return same-user PID -> (PPID, process group, starttime)."""

    table: dict[int, tuple[int, int, int]] = {}
    try:
        entries = list(Path("/proc").iterdir())
    except OSError:
        return table
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            if entry.stat().st_uid != os.getuid():
                continue
            raw = (entry / "stat").read_text(encoding="ascii")
            fields = raw[raw.rindex(")") + 2:].split()
            table[int(entry.name)] = (
                int(fields[1]), int(fields[2]), int(fields[19])
            )
        except (OSError, UnicodeError, ValueError, IndexError):
            continue
    return table


def _owned_descendant_identities(root_pid: int) -> list[tuple[int, int, int]]:
    """Snapshot exact identities of the same-user process subtree."""

    table = _process_table()
    descendants = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, (parent_pid, _group_id, _start_time) in table.items():
            if parent_pid in descendants and pid not in descendants:
                descendants.add(pid)
                changed = True
    return [
        (pid, table[pid][1], table[pid][2])
        for pid in sorted(descendants, reverse=True)
        if pid in table
    ]


def _identity_is_current(identity: tuple[int, int, int]) -> bool:
    pid, group_id, start_time = identity
    try:
        stat_path = Path("/proc") / str(pid) / "stat"
        raw = stat_path.read_text(encoding="ascii")
        fields = raw[raw.rindex(")") + 2:].split()
        return (
            fields[0] != "Z"
            and int(fields[2]) == group_id
            and int(fields[19]) == start_time
            and stat_path.parent.stat().st_uid == os.getuid()
        )
    except (OSError, ProcessLookupError, UnicodeError, ValueError, IndexError):
        return False


def _stop_exact_processes(
    identities: list[tuple[int, int, int]], *, grace_seconds: float,
) -> bool:
    """Boundedly stop only PID/starttime identities observed as descendants."""

    unique = list(dict.fromkeys(identities))
    group_leaders = [item for item in unique if item[0] == item[1]]
    for pid, _group_id, _start_time in group_leaders:
        identity = next(item for item in unique if item[0] == pid)
        if _identity_is_current(identity):
            try:
                os.killpg(pid, signal.SIGTERM)
            except (OSError, ProcessLookupError):
                pass
    deadline = time.monotonic() + grace_seconds
    while time.monotonic() < deadline and any(
        _identity_is_current(item) for item in unique
    ):
        time.sleep(min(0.02, max(0.0, deadline - time.monotonic())))
    for pid, _group_id, _start_time in group_leaders:
        identity = next(item for item in unique if item[0] == pid)
        if _identity_is_current(identity):
            try:
                os.killpg(pid, signal.SIGKILL)
            except (OSError, ProcessLookupError):
                pass
    for identity in unique:
        if _identity_is_current(identity):
            try:
                os.kill(identity[0], signal.SIGKILL)
            except (OSError, ProcessLookupError):
                pass
    deadline = time.monotonic() + grace_seconds
    while time.monotonic() < deadline and any(
        _identity_is_current(item) for item in unique
    ):
        time.sleep(min(0.02, max(0.0, deadline - time.monotonic())))
    return not any(_identity_is_current(item) for item in unique)


def _qualification_passed(report: Mapping[str, Any], required_apps: list[str]) -> bool:
    summary = report.get("summary")
    applications = report.get("applications")
    if not isinstance(summary, dict) or not isinstance(applications, list):
        return False
    names = [
        item.get("application")
        for item in applications
        if isinstance(item, dict)
    ]
    if not all(isinstance(name, str) for name in names):
        return False
    return (
        report.get("status") == "completed"
        and len(applications) == len(required_apps)
        and len(names) == len(applications)
        and len(set(names)) == len(names)
        and set(names) == set(required_apps)
        and summary.get("total") == len(required_apps)
        and summary.get("supported") == len(required_apps)
        and summary.get("unsupported") == 0
        and summary.get("error") == 0
        and all(
            item.get("status") == "supported"
            and item.get("support_level") == "observed_read_only"
            and isinstance(item.get("snapshot"), dict)
            and item["snapshot"].get("truncated") is False
            and item.get("writes_dispatched") == []
            and isinstance(item.get("cleanup"), dict)
            and item["cleanup"].get("owned_process_group_stopped") is True
            for item in applications
            if isinstance(item, dict)
        )
    )


def _run_private_qualifier(
    command: list[str], environment: dict[str, str], timeout: float,
    *, termination_grace: float = 3.0,
) -> tuple[int | None, bool, bool]:
    """Run the private bus as an owned group and clean it on timeout."""

    process = subprocess.Popen(
        command, env=environment, stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        return process.wait(timeout=timeout), False, True
    except subprocess.TimeoutExpired:
        identities = _owned_descendant_identities(process.pid)
        cleanup_succeeded = _stop_exact_processes(
            identities, grace_seconds=termination_grace
        )
        try:
            process.wait(timeout=termination_grace)
        except subprocess.TimeoutExpired:
            cleanup_succeeded = False
        return None, True, cleanup_succeeded
    except Exception:
        identities = _owned_descendant_identities(process.pid)
        _stop_exact_processes(identities, grace_seconds=termination_grace)
        raise


def qualify_application(
    name: str, environment: dict[str, str], *, registration_timeout: float,
    snapshot_timeout: float, max_depth: int, max_nodes: int, work_dir: Path,
) -> dict[str, Any]:
    spec = APP_SPECS[name]
    executable = next(
        (
            resolved
            for candidate in spec["executables"]
            if (resolved := shutil.which(candidate, path=environment.get("PATH")))
        ),
        None,
    )
    result: dict[str, Any] = {
        "application": name,
        "status": "unsupported",
        "support_level": "none",
        "executable": executable,
        "version": None,
        "launch_pid": None,
        "pid_selection": "exact_popen_pid",
        "registration_latency_ms": None,
        "snapshot_latency_ms": None,
        "snapshot": None,
        "errors": [],
        "writes_dispatched": [],
    }
    if executable is None:
        result["errors"].append({
            "stage": "preflight", "code": "executable_not_found",
            "message": f"none of {spec['executables']!r} is installed",
            "retryable": False,
        })
        return result
    version_args = spec.get("version_args", ("--version",))
    result["version"] = (
        {"available": True, "source": "host.qt"}
        if version_args is None
        else _command_output([executable, *version_args], environment)
    )

    plugin = ProcessPlugin(
        [sys.executable, str(DRIVER_PATH)], env=environment, timeout=20,
        name=f"desktop.linux_atspi qualifier {name}",
    )
    process: subprocess.Popen[Any] | None = None
    log_path = work_dir / f"{name}.stderr.log"
    try:
        manifest = plugin.start(timeout=10)
        result["driver_version"] = manifest.get("metadata", {}).get("version")
        baseline = plugin.invoke(
            "desktop.linux_atspi.list_applications@1", {}, timeout=5
        )
        result["backend"] = baseline.get("backend")
        result["private_registry_baseline_count"] = len(
            baseline.get("applications", [])
        )
        fixture_dir = work_dir / f"{name}-fixture"
        fixture_dir.mkdir(mode=0o700, exist_ok=True)
        launch_args = [
            str(fixture_dir) if item == "{fixture_dir}" else item
            for item in spec["launch_args"]
        ]
        command = [executable, *launch_args]
        with log_path.open("wb") as stderr_file:
            launch_started = time.monotonic()
            process = subprocess.Popen(
                command, env=environment, cwd=str(work_dir),
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=stderr_file, start_new_session=True,
            )
            result["launch_pid"] = process.pid
            result["launch_args"] = command[1:]
            deadline = launch_started + registration_timeout
            found: dict[str, Any] | None = None
            while time.monotonic() < deadline:
                if not _owned_exact_pid(process, process.pid):
                    break
                try:
                    listing = plugin.invoke(
                        "desktop.linux_atspi.list_applications@1", {},
                        timeout=min(3.0, max(0.2, deadline - time.monotonic())),
                    )
                except PluginError as exc:
                    result["errors"].append(_error("list_applications", exc))
                    break
                matches = [
                    item for item in listing.get("applications", [])
                    if item.get("process_id") == process.pid
                ]
                if len(matches) == 1:
                    found = matches[0]
                    break
                if len(matches) > 1:
                    result["errors"].append({
                        "stage": "registration",
                        "code": "ambiguous_exact_pid",
                        "message": "multiple AT-SPI applications reported the owned PID",
                        "retryable": False,
                        "candidate_count": len(matches),
                    })
                    break
                time.sleep(min(0.2, max(0.0, deadline - time.monotonic())))
            result["registration_latency_ms"] = round(
                (time.monotonic() - launch_started) * 1000, 2
            )
            if found is None:
                if not result["errors"]:
                    reason = (
                        "owned_process_exited_before_registration"
                        if process.poll() is not None
                        else "atspi_application_not_registered_within_deadline"
                    )
                    result["errors"].append({
                        "stage": "registration", "code": reason,
                        "message": (
                            f"the exact owned PID was not present in the private "
                            f"AT-SPI registry within {registration_timeout:g}s"
                        ),
                        "retryable": False,
                        "process_returncode": process.poll(),
                    })
                return result
            if not _owned_exact_pid(process, int(found["process_id"])):
                result["errors"].append({
                    "stage": "ownership", "code": "pid_ownership_not_proven",
                    "message": "AT-SPI PID was not the live owned process-group leader",
                    "retryable": False,
                })
                return result

            result["atspi_application"] = found
            selector = {"process_id": process.pid}
            for key in ("bus_name", "toolkit_name"):
                if isinstance(found.get(key), str) and found[key]:
                    selector[key] = found[key]
            snapshot_started = time.monotonic()
            snapshot = plugin.invoke(
                "desktop.linux_atspi.snapshot@1",
                {
                    "application": selector,
                    "max_depth": max_depth,
                    "max_nodes": max_nodes,
                },
                timeout=snapshot_timeout,
            )
            result["snapshot_latency_ms"] = round(
                (time.monotonic() - snapshot_started) * 1000, 2
            )
            nodes = snapshot.get("nodes", [])
            result["snapshot"] = {
                "selector": selector,
                "max_depth": max_depth,
                "max_nodes": max_nodes,
                "truncated": snapshot.get("truncated"),
                "encoded_bytes": len(
                    json.dumps(
                        snapshot, ensure_ascii=False, separators=(",", ":")
                    ).encode("utf-8")
                ),
                "completeness": summarize_nodes(nodes),
                "content_retention": "aggregate_only_no_ui_text",
            }
            result["status"] = "supported"
            result["support_level"] = "observed_read_only"
            return result
    except Exception as exc:
        result["status"] = "error"
        result["errors"].append(_error("qualify", exc))
        return result
    finally:
        if process is not None:
            _stop_owned_process(process)
            result["cleanup"] = {
                "owned_process_group_stopped": process.poll() is not None,
                "returncode": process.poll(),
            }
        plugin.close()
        if log_path.is_file():
            raw = log_path.read_bytes()[-4096:]
            text = raw.decode("utf-8", errors="replace").strip()
            if text:
                result["stderr_tail"] = text


def _host_facts(environment: dict[str, str]) -> dict[str, Any]:
    os_release: dict[str, str] = {}
    path = Path("/etc/os-release")
    if path.is_file():
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            if "=" in line:
                key, value = line.split("=", 1)
                os_release[key] = value.strip().strip('\"')
    return {
        "os": os_release.get("PRETTY_NAME", platform.platform()),
        "kernel": platform.release(),
        "architecture": platform.machine(),
        "plasma": _command_output(["plasmashell", "--version"], environment),
        "qt": _command_output(["qmake", "-query", "QT_VERSION"], environment),
        "x11": _command_output(["xdpyinfo"], environment),
    }


def run_qualification(args: argparse.Namespace) -> dict[str, Any]:
    environment = sanitized_environment(os.environ)
    environment["QT_QPA_PLATFORM"] = "xcb"
    environment["QT_LINUX_ACCESSIBILITY_ALWAYS_ON"] = "1"
    environment["QT_ACCESSIBILITY"] = "1"
    environment.pop("NO_AT_BRIDGE", None)
    environment.pop("AT_SPI_BUS_ADDRESS", None)
    environment["PYTHONPATH"] = str(SRC_ROOT)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="aad-kde-qualifier-work-") as temporary:
        work_dir = Path(temporary)
        applications = [
            qualify_application(
                name, environment, registration_timeout=args.registration_timeout,
                snapshot_timeout=args.snapshot_timeout, max_depth=args.max_depth,
                max_nodes=args.max_nodes, work_dir=work_dir,
            )
            for name in args.app
        ]
    counts = Counter(str(item["status"]) for item in applications)
    return {
        "schema_version": SCHEMA_VERSION,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "status": "completed",
        "environment": {
            "session_type": environment.get("XDG_SESSION_TYPE"),
            "desktop": environment.get("XDG_CURRENT_DESKTOP"),
            "display": environment.get("DISPLAY"),
            "private_session_bus": (
                environment.get("AI_AUTO_DESKTOP_QUALIFIER_PRIVATE_BUS") == "1"
            ),
            "private_home_and_xdg_dirs": True,
            "inherited_at_spi_bus_address": False,
        },
        "host": _host_facts(environment),
        "safety": {
            "existing_windows_selected": False,
            "application_selector": "exact owned Popen PID plus available bus/toolkit identity",
            "write_actions_enabled": False,
            "write_actions_dispatched": 0,
            "screenshots_or_ocr": False,
            "node_ui_text_retained": False,
        },
        "limits": {
            "registration_timeout_seconds": args.registration_timeout,
            "snapshot_timeout_seconds": args.snapshot_timeout,
            "max_depth": args.max_depth,
            "max_nodes": args.max_nodes,
        },
        "summary": {
            "total": len(applications),
            "supported": counts["supported"],
            "unsupported": counts["unsupported"],
            "error": counts["error"],
            "duration_ms": round((time.monotonic() - started) * 1000, 2),
        },
        "applications": applications,
    }


def _atomic_write(path: Path, report: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + f".tmp-{os.getpid()}")
    temporary.write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    temporary.chmod(0o600)
    temporary.replace(path)


def _unsupported_report(reason: str) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "status": "unsupported",
        "errors": [{"stage": "preflight", "code": reason}],
        "applications": [],
    }


def _terminal_error_report(
    code: str, *, details: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    error: dict[str, Any] = {"stage": "private_session", "code": code}
    if details:
        error.update(details)
    return {
        "schema_version": SCHEMA_VERSION,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "status": "error",
        "errors": [error],
        "applications": [],
    }


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Read-only qualification of newly launched KDE/X11 applications"
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=PROJECT_ROOT / "artifacts" / "kde-x11-qualification.json",
    )
    parser.add_argument("--display")
    parser.add_argument("--app", action="append", choices=sorted(APP_SPECS), default=None)
    parser.add_argument("--registration-timeout", type=float, default=15.0)
    parser.add_argument("--snapshot-timeout", type=float, default=15.0)
    parser.add_argument("--max-depth", type=int, default=32)
    parser.add_argument("--max-nodes", type=int, default=2000)
    parser.add_argument("--inside-private-bus", action="store_true", help=argparse.SUPPRESS)
    parsed = parser.parse_args(argv)
    parsed.app = parsed.app or list(APP_SPECS)
    if not 0 < parsed.registration_timeout <= 15:
        parser.error("--registration-timeout must be in (0, 15]")
    if not 0 < parsed.snapshot_timeout <= 30:
        parser.error("--snapshot-timeout must be in (0, 30]")
    if not 0 <= parsed.max_depth <= 128 or not 1 <= parsed.max_nodes <= 5000:
        parser.error("snapshot bounds exceed driver limits")
    parsed.output = parsed.output.resolve()
    return parsed


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    if args.inside_private_bus:
        try:
            report = run_qualification(args)
        except Exception as exc:
            report = _terminal_error_report(
                "qualification_failed",
                details={"reason": type(exc).__name__},
            )
        _atomic_write(args.output, report)
        print(
            json.dumps(
                {
                    "status": report.get("status"),
                    "summary": report.get("summary"),
                    "output": str(args.output),
                },
                ensure_ascii=False,
            )
        )
        # A filtered run is useful for diagnosis but cannot qualify the full
        # release matrix.  Exit zero only for the complete required set.
        passed = _qualification_passed(report, list(APP_SPECS))
        return 0 if passed else 1

    session = discover_kde_x11_session(args.display)
    if session is None:
        report = _unsupported_report("kde_x11_session_not_found")
        _atomic_write(args.output, report)
        print(json.dumps({"status": "unsupported", "output": str(args.output)}, ensure_ascii=False))
        return 2
    dbus_run_session = shutil.which("dbus-run-session")
    if dbus_run_session is None:
        report = _unsupported_report("dbus_run_session_not_found")
        _atomic_write(args.output, report)
        print(json.dumps({"status": "unsupported", "output": str(args.output)}, ensure_ascii=False))
        return 2

    with tempfile.TemporaryDirectory(prefix="aad-kde-qualifier-") as temporary:
        root = Path(temporary)
        private_output = root / "qualification.json"
        environment = sanitized_environment(os.environ)
        environment.update(session)
        environment.pop("DBUS_SESSION_BUS_ADDRESS", None)
        environment.pop("AT_SPI_BUS_ADDRESS", None)
        for key, leaf in (
            ("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"), ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "runtime"),
        ):
            directory = root / leaf
            directory.mkdir(mode=0o700)
            environment[key] = str(directory)
        environment["AI_AUTO_DESKTOP_QUALIFIER_PRIVATE_BUS"] = "1"
        command = [
            dbus_run_session, "--", sys.executable, str(Path(__file__).resolve()),
            "--inside-private-bus", "--output", str(private_output),
            "--registration-timeout", str(args.registration_timeout),
            "--snapshot-timeout", str(args.snapshot_timeout),
            "--max-depth", str(args.max_depth), "--max-nodes", str(args.max_nodes),
        ]
        for app in args.app:
            command.extend(("--app", app))
        # Private bus services may inherit stdout/stderr.  DEVNULL keeps their
        # lifetime from extending communicate() after the qualifier exits.
        try:
            returncode, timed_out, cleanup_succeeded = _run_private_qualifier(
                command, environment, 75
            )
        except Exception as exc:
            returncode, timed_out = None, False
            report = _terminal_error_report(
                "private_qualifier_start_failed",
                details={"reason": type(exc).__name__},
            )
        else:
            if timed_out:
                report = _terminal_error_report(
                    "private_qualifier_timeout",
                    details={"cleanup_succeeded": cleanup_succeeded},
                )
            else:
                try:
                    loaded = json.loads(private_output.read_text(encoding="utf-8"))
                    if not isinstance(loaded, dict):
                        raise ValueError("qualification report is not an object")
                    report = loaded
                except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
                    report = _terminal_error_report(
                        "private_qualifier_result_unreadable",
                        details={
                            "reason": type(exc).__name__,
                            "returncode": returncode,
                        },
                    )
        _atomic_write(args.output, report)
    print(json.dumps({
        "status": report.get("status"),
        "summary": report.get("summary"),
        "output": str(args.output),
    }, ensure_ascii=False))
    return 1 if timed_out or returncode is None else int(returncode)


if __name__ == "__main__":
    raise SystemExit(main())
