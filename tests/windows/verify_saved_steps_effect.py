#!/usr/bin/env python3
"""Replay saved control steps with `aad run` against an isolated Win32 probe window.

The probe is created and destroyed here. A fill and two waits go through the
control.session provider (the format the GUI saves); the edit box is then read
back independently with WM_GETTEXT.
"""
import ctypes, json, os, subprocess, sys, tempfile

REPO = r"E:\Projects\ai-auto-desktop"
AAD = os.path.join(REPO, "target", "debug", "aad.exe")
PROBE = os.path.join(REPO, "tests", "windows", "input_channel_probe.py")

title = f"AAD Steps Probe {os.getpid()}"
log = os.path.join(tempfile.gettempdir(), f"aad-steps-{os.getpid()}.jsonl")
probe = subprocess.Popen([sys.executable, PROBE, "--title", title, "--log", log],
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
try:
    ready = json.loads(probe.stdout.readline())
    app = {"provider": "desktop.windows_uia", "title": title}
    steps = [
        {"id": "write", "app": app, "locator": {"role": "edit"},
         "action": {"type": "fill", "value": "from saved steps"}},
        {"id": "button_there", "app": app, "locator": {"role": "button", "name": "Probe target"},
         "check": {"present": True, "within_ms": 3000}},
        {"id": "gone", "app": app, "locator": {"role": "button", "name": "No such button"},
         "check": {"present": False, "within_ms": 1000}},
    ]
    workflow = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1", "kind": "Workflow",
        "metadata": {"name": "steps-probe", "version": "0.1.0"},
        "requires": {"capabilities": [{"name": "control.session", "version": ">=0.1.0"}],
                     "permissions": ["desktop.input", "desktop.observe"]},
        "budgets": {"max_duration": "2m", "max_executed_steps": 3},
        "steps": [{"id": s["id"], "type": "action",
                   "uses": "control.session." + ("check" if "check" in s else "act") + "@1",
                   "with": {"step": s},
                   "effect": {"class": "read_only" if "check" in s else "non_idempotent"},
                   "risk": {"category": "observe", "level": "low"} if "check" in s
                   else {"category": "modify", "level": "high"}} for s in steps],
    }
    path = os.path.join(tempfile.gettempdir(), "aad-steps-probe.json")
    json.dump(workflow, open(path, "w", encoding="utf-8"))
    run = subprocess.run([AAD, "--compact", "run", path, "--permission", "desktop.input",
                          "--permission", "desktop.observe"],
                         capture_output=True, text=True, encoding="utf-8", timeout=120)
    result = json.loads(run.stdout)
    buffer = ctypes.create_unicode_buffer(256)
    ctypes.windll.user32.SendMessageW(ready["edit_handle"], 0x000D, 256, buffer)
    print(json.dumps({"exit": run.returncode, "status": result.get("status"),
                      "error": result.get("error"), "edit_text": buffer.value,
                      "passed": run.returncode == 0 and buffer.value == "from saved steps"},
                     ensure_ascii=False, indent=1))
    sys.exit(0 if run.returncode == 0 and buffer.value == "from saved steps" else 1)
finally:
    probe.terminate()
