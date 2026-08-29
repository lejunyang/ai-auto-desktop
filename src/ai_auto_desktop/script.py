"""Fail-closed, process-isolated script execution helpers."""

from __future__ import annotations

import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
from typing import Any, Mapping

from .errors import AutomationError
from .model import CompiledStep, WorkflowDescriptor, thaw


def validate_script_policy(step: CompiledStep) -> None:
    if step.type != "script":
        raise ValueError("step must be a script step")
    if step.params.get("runtime") != "python":
        raise AutomationError(
            "DESCRIPTOR.UNSUPPORTED_FEATURE",
            "v0 only executes Python scripts",
            category="script",
        )
    if step.params.get("capabilities"):
        raise AutomationError(
            "SCRIPT.SANDBOX_DENIED",
            "v0 cannot grant script capabilities",
            category="script",
        )
    sandbox: Mapping[str, Any] = step.params.get("sandbox", {})
    for boundary in ("network", "filesystem", "environment"):
        config = sandbox.get(boundary, {})
        if isinstance(config, Mapping) and config.get("mode", "deny") != "deny":
            raise AutomationError(
                "SCRIPT.SANDBOX_DENIED",
                f"v0 cannot grant script {boundary}",
                category="script",
            )


def resolve_entrypoint(descriptor: WorkflowDescriptor, entrypoint: str) -> Path:
    path = Path(entrypoint)
    if not path.is_absolute() and descriptor.source is not None:
        path = descriptor.source.parent / path
    try:
        path = path.resolve(strict=True)
    except OSError as exc:
        raise AutomationError(
            "SCRIPT.START_FAILED",
            f"Cannot resolve script entrypoint: {exc}",
            category="script",
        ) from exc
    if not path.is_file():
        raise AutomationError(
            "SCRIPT.START_FAILED",
            "Script entrypoint is not a regular file",
            category="script",
        )
    return path


def sandbox_availability() -> dict[str, Any]:
    """Describe this platform's script sandbox without executing anything.

    Returned ``state`` follows the capability-probe vocabulary:

    ``available``
        Every isolation boundary the contract promises is enforced.
    ``degraded``
        Resource, environment and interpreter isolation are enforced, but at
        least one boundary listed in ``gaps`` is not.  Scripts still run.
    ``unavailable``
        No supported sandbox exists here; script steps fail closed.
    """

    if sys.platform.startswith("linux"):
        missing = []
        if shutil.which("bwrap") is None:
            missing.append("bwrap")
        if shutil.which("prlimit") is None:
            missing.append("prlimit")
        if not Path("/usr/bin/python3").is_file():
            missing.append("/usr/bin/python3")
        if missing:
            return {
                "state": "unavailable",
                "mechanism": "linux.bubblewrap",
                "missing": missing,
                "gaps": [],
            }
        return {
            "state": "available",
            "mechanism": "linux.bubblewrap",
            "missing": [],
            "gaps": [],
        }
    if sys.platform == "win32":
        interpreter = _windows_interpreter()
        if interpreter is None:
            return {
                "state": "unavailable",
                "mechanism": "windows.job_object",
                "missing": ["python interpreter"],
                "gaps": [],
            }
        try:
            from ._win_job import WindowsJob
        except Exception:
            return {
                "state": "unavailable",
                "mechanism": "windows.job_object",
                "missing": ["job object support"],
                "gaps": [],
            }
        try:
            probe = WindowsJob(
                memory_bytes=_WINDOWS_MEMORY_BYTES,
                cpu_seconds=1,
                active_processes=_WINDOWS_ACTIVE_PROCESSES,
            )
        except Exception:
            return {
                "state": "unavailable",
                "mechanism": "windows.job_object",
                "missing": ["job object resource limits"],
                "gaps": [],
            }
        probe.close()
        return {
            "state": "degraded",
            "mechanism": "windows.job_object",
            "missing": [],
            # Stated rather than implied: Windows has no per-process network or
            # mount namespace, so these two boundaries are NOT enforced.
            "gaps": ["network", "filesystem"],
            "interpreter": interpreter,
        }
    return {
        "state": "unavailable",
        "mechanism": None,
        "missing": [sys.platform],
        "gaps": [],
    }


def _windows_interpreter() -> str | None:
    """Resolve the interpreter the Windows sandbox will run.

    Linux pins ``/usr/bin/python3``; Windows has no equivalent fixed path, so
    the running interpreter is used and reported in the probe evidence.  A
    launcher stub (``py.exe``) is never acceptable because the sandbox must know
    exactly which binary it executes.
    """

    candidate = sys.executable
    if not candidate:
        return None
    path = Path(candidate)
    if not path.is_file():
        return None
    if path.name.lower() in {"py.exe", "pyw.exe"}:
        return None
    return str(path)


def execute_python_script(
    descriptor: WorkflowDescriptor,
    step: CompiledStep,
    inputs: Any,
    timeout: float | None,
) -> Any:
    """Execute a script step in this platform's sandbox, or fail closed.

    Linux uses bubblewrap and enforces every boundary. Windows uses a Job
    Object and enforces resource, environment, interpreter and working-directory
    isolation, but not network or filesystem isolation; that gap is reported by
    :func:`sandbox_availability` rather than hidden. Every other platform fails
    closed.
    """

    validate_script_policy(step)
    if sys.platform == "win32":
        return _execute_windows(descriptor, step, inputs, timeout)
    return _execute_linux(descriptor, step, inputs, timeout)


def _execute_linux(
    descriptor: WorkflowDescriptor,
    step: CompiledStep,
    inputs: Any,
    timeout: float | None,
) -> Any:
    """Execute Python inside a minimal Linux bubblewrap sandbox.

    Other platforms fail closed until equivalent OS isolation is implemented.
    The sandbox has no host ``/etc`` or home mount, a private network/PID
    namespace, an empty environment, a tmpfs working directory, and a
    read-only bind of the one script file.
    """

    bubblewrap = shutil.which("bwrap")
    prlimit = shutil.which("prlimit")
    python = Path("/usr/bin/python3")
    if not sys.platform.startswith("linux") or not bubblewrap or not prlimit or not python.is_file():
        raise AutomationError(
            "SCRIPT.SANDBOX_UNAVAILABLE",
            "A supported Linux bubblewrap sandbox is not available",
            category="script",
        )

    sandbox = thaw(step.params.get("sandbox", {}))
    output_limit = int(sandbox.get("max_output_bytes", 1024 * 1024))
    wall_timeout = timeout if timeout is not None else 30.0
    cpu_limit = max(1, math.ceil(wall_timeout) + 1)

    with tempfile.TemporaryDirectory(prefix="aad-script-") as directory:
        directory_path = Path(directory)
        if "source" in step.params:
            source_path = directory_path / "script.py"
            source_path.write_text(str(step.params["source"]), encoding="utf-8")
        else:
            source_path = resolve_entrypoint(descriptor, str(step.params["entrypoint"]))

        command = [
            bubblewrap,
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/usr",
            "/usr",
        ]
        for system_dir in ("/lib", "/lib64"):
            if Path(system_dir).exists():
                command += ["--ro-bind", system_dir, system_dir]
        command += [
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/workflow",
            "--ro-bind",
            str(source_path),
            "/workflow/script.py",
            "--chdir",
            "/tmp",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "PYTHONIOENCODING",
            "utf-8",
            prlimit,
            f"--fsize={output_limit}",
            "--as=536870912",
            f"--cpu={cpu_limit}",
            "--nofile=64",
            "--core=0",
            "--",
            str(python),
            "-I",
            "/workflow/script.py",
        ]

        stdout_path = directory_path / "stdout"
        stderr_path = directory_path / "stderr"
        try:
            with stdout_path.open("w+b") as stdout_file, stderr_path.open("w+b") as stderr_file:
                process = subprocess.Popen(
                    command,
                    stdin=subprocess.PIPE,
                    stdout=stdout_file,
                    stderr=stderr_file,
                    text=True,
                    encoding="utf-8",
                    env={"PATH": os.environ.get("PATH", "/usr/bin:/bin")},
                    start_new_session=True,
                )
                try:
                    process.communicate(
                        json.dumps(inputs, ensure_ascii=False, allow_nan=False),
                        timeout=wall_timeout,
                    )
                except subprocess.TimeoutExpired as exc:
                    _kill_process_group(process)
                    raise AutomationError(
                        "SCRIPT.TIMEOUT",
                        "Script timed out",
                        category="script",
                        cause=exc,
                    ) from exc
                stdout_file.seek(0)
                stderr_file.seek(0)
                stdout = stdout_file.read(output_limit + 1)
                stderr = stderr_file.read(min(output_limit, 64 * 1024) + 1)
        except AutomationError:
            raise
        except (OSError, ValueError) as exc:
            raise AutomationError(
                "SCRIPT.START_FAILED",
                f"Could not start sandboxed script: {exc}",
                category="script",
                cause=exc,
            ) from exc

    return _decode_script_result(stdout, stderr, process.returncode, output_limit)


# Windows sandbox tuning.  These mirror the Linux prlimit values so the two
# platforms impose comparable resource ceilings.
_WINDOWS_MEMORY_BYTES = 536_870_912  # matches Linux --as=536870912
_WINDOWS_ACTIVE_PROCESSES = 8  # the interpreter plus a small margin
_WINDOWS_CREATE_SUSPENDED = 0x00000004
_WINDOWS_CREATE_NO_WINDOW = 0x08000000
_WINDOWS_CREATE_NEW_PROCESS_GROUP = 0x00000200


def _execute_windows(
    descriptor: WorkflowDescriptor,
    step: CompiledStep,
    inputs: Any,
    timeout: float | None,
) -> Any:
    """Execute Python under a Job Object with kernel-enforced resource caps.

    What is enforced: a memory ceiling, a CPU-time ceiling, a process-count
    ceiling, guaranteed reclamation of the whole tree, an empty environment, an
    isolated working directory, and ``-I`` isolated interpreter mode (no
    PYTHONPATH, no user site-packages, no environment influence).

    What is NOT enforced, and why: Windows has no per-process network namespace
    or mount namespace, so a script can still reach the network and read paths
    the user can read.  :func:`sandbox_availability` reports this as ``degraded``
    with an explicit ``gaps`` list; callers must not treat it as equivalent to
    the Linux sandbox.
    """

    interpreter = _windows_interpreter()
    if interpreter is None:
        raise AutomationError(
            "SCRIPT.SANDBOX_UNAVAILABLE",
            "No usable Python interpreter was found for the Windows sandbox",
            category="script",
        )
    try:
        from ._win_job import WindowsJob, WindowsJobError, resume_process
    except Exception as exc:  # pragma: no cover - import guard
        raise AutomationError(
            "SCRIPT.SANDBOX_UNAVAILABLE",
            "Windows Job Object support is not available",
            category="script",
            cause=exc,
        ) from exc

    sandbox = thaw(step.params.get("sandbox", {}))
    output_limit = int(sandbox.get("max_output_bytes", 1024 * 1024))
    wall_timeout = timeout if timeout is not None else 30.0
    cpu_limit = max(1, math.ceil(wall_timeout) + 1)

    with tempfile.TemporaryDirectory(prefix="aad-script-") as directory:
        directory_path = Path(directory)
        if "source" in step.params:
            source_path = directory_path / "script.py"
            source_path.write_text(str(step.params["source"]), encoding="utf-8")
        else:
            source_path = resolve_entrypoint(descriptor, str(step.params["entrypoint"]))

        # The script runs from its own empty directory, not the host's cwd, so a
        # bare relative path inside the script cannot reach workflow files.
        working_directory = directory_path / "cwd"
        working_directory.mkdir()

        try:
            job = WindowsJob(
                memory_bytes=_WINDOWS_MEMORY_BYTES,
                cpu_seconds=cpu_limit,
                active_processes=_WINDOWS_ACTIVE_PROCESSES,
            )
        except WindowsJobError as exc:
            raise AutomationError(
                "SCRIPT.SANDBOX_UNAVAILABLE",
                "A supported Windows Job Object sandbox is not available",
                category="script",
                cause=exc,
            ) from exc

        stdout_path = directory_path / "stdout"
        stderr_path = directory_path / "stderr"
        process: subprocess.Popen[str] | None = None
        try:
            with stdout_path.open("w+b") as stdout_file, stderr_path.open("w+b") as stderr_file:
                try:
                    process = subprocess.Popen(
                        [interpreter, "-I", "-B", "-E", "-s", "-S", str(source_path)],
                        stdin=subprocess.PIPE,
                        stdout=stdout_file,
                        stderr=stderr_file,
                        text=True,
                        encoding="utf-8",
                        # Empty environment: nothing about the host leaks in.
                        env={},
                        cwd=str(working_directory),
                        creationflags=(
                            _WINDOWS_CREATE_SUSPENDED
                            | _WINDOWS_CREATE_NO_WINDOW
                            | _WINDOWS_CREATE_NEW_PROCESS_GROUP
                        ),
                    )
                except (OSError, ValueError) as exc:
                    raise AutomationError(
                        "SCRIPT.START_FAILED",
                        f"Could not start sandboxed script: {exc}",
                        category="script",
                        cause=exc,
                    ) from exc

                # Assign before resuming: the script must never run outside the
                # job, or it could spawn descendants past the caps.
                try:
                    job.assign(process.pid)
                except WindowsJobError as exc:
                    process.kill()
                    process.wait(timeout=5)
                    raise AutomationError(
                        "SCRIPT.SANDBOX_UNAVAILABLE",
                        "The script could not be confined to a Job Object",
                        category="script",
                        cause=exc,
                    ) from exc
                try:
                    resume_process(process.pid)
                except WindowsJobError as exc:
                    job.terminate()
                    raise AutomationError(
                        "SCRIPT.START_FAILED",
                        "The sandboxed script could not be resumed",
                        category="script",
                        cause=exc,
                    ) from exc

                try:
                    process.communicate(
                        json.dumps(inputs, ensure_ascii=False, allow_nan=False),
                        timeout=wall_timeout,
                    )
                except subprocess.TimeoutExpired as exc:
                    job.terminate()
                    _reap(process)
                    raise AutomationError(
                        "SCRIPT.TIMEOUT",
                        "Script timed out",
                        category="script",
                        cause=exc,
                    ) from exc
                stdout_file.seek(0)
                stderr_file.seek(0)
                stdout = stdout_file.read(output_limit + 1)
                stderr = stderr_file.read(min(output_limit, 64 * 1024) + 1)
            returncode = process.returncode
        except AutomationError:
            raise
        except (OSError, ValueError) as exc:
            raise AutomationError(
                "SCRIPT.START_FAILED",
                f"Could not start sandboxed script: {exc}",
                category="script",
                cause=exc,
            ) from exc
        finally:
            # terminate() then close() reclaims the tree even if the script
            # spawned helpers the host never saw.
            job.terminate()
            job.close()
            if process is not None:
                _reap(process)

    return _decode_script_result(stdout, stderr, returncode, output_limit)


def _reap(process: subprocess.Popen[Any]) -> None:
    """Wait briefly for an already-terminated process to be collected."""

    try:
        process.wait(timeout=5)
    except (subprocess.TimeoutExpired, OSError):
        try:
            process.kill()
        except OSError:
            pass
        try:
            process.wait(timeout=5)
        except (subprocess.TimeoutExpired, OSError):
            pass


def _decode_script_result(
    stdout: bytes, stderr: bytes, returncode: int | None, output_limit: int
) -> Any:
    """Apply the shared output contract: size limit, exit status, one JSON value."""

    if len(stdout) > output_limit or len(stderr) > min(output_limit, 64 * 1024):
        raise AutomationError(
            "SCRIPT.OUTPUT_INVALID",
            "Script output exceeded its configured limit",
            category="script",
        )
    stderr_text = stderr.decode("utf-8", errors="replace")
    if returncode != 0:
        raise AutomationError(
            "SCRIPT.EXIT_NONZERO",
            f"Script exited with status {returncode}",
            category="script",
            details={"returncode": returncode, "stderr": stderr_text[-4096:]},
        )
    try:
        return json.loads(stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise AutomationError(
            "SCRIPT.OUTPUT_INVALID",
            "Script stdout must be one UTF-8 JSON value",
            category="script",
            details={"error": str(exc)},
        ) from exc


def _kill_process_group(process: subprocess.Popen[Any]) -> None:
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError, OSError):
        pass
    try:
        process.wait(timeout=0.25)
        return
    except (subprocess.TimeoutExpired, OSError):
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass
    try:
        process.wait(timeout=0.25)
    except (subprocess.TimeoutExpired, OSError):
        pass


__all__ = [
    "execute_python_script",
    "resolve_entrypoint",
    "sandbox_availability",
    "validate_script_policy",
]
