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


def execute_python_script(
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

    validate_script_policy(step)
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
            "/usr/bin/prlimit",
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

    if len(stdout) > output_limit or len(stderr) > min(output_limit, 64 * 1024):
        raise AutomationError(
            "SCRIPT.OUTPUT_INVALID",
            "Script output exceeded its configured limit",
            category="script",
        )
    stderr_text = stderr.decode("utf-8", errors="replace")
    if process.returncode != 0:
        raise AutomationError(
            "SCRIPT.EXIT_NONZERO",
            f"Script exited with status {process.returncode}",
            category="script",
            details={"returncode": process.returncode, "stderr": stderr_text[-4096:]},
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


__all__ = ["execute_python_script", "resolve_entrypoint", "validate_script_policy"]
