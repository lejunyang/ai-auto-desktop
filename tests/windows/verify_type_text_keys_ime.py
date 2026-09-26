#!/usr/bin/env python3
"""Prove keys mode refuses an open IME before sending any input."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
AAD = os.path.join(REPO, "target", "debug", "aad.exe")
PROBE = os.path.join(HERE, "input_channel_probe.py")


def aad(*args: str) -> tuple[dict, int]:
    prefix = [AAD, "--compact"] if args[0] in {"find", "do"} else [AAD]
    result = subprocess.run([*prefix, *args], capture_output=True, text=True, timeout=30)
    return json.loads(result.stdout.strip()), result.returncode


def read_log(path: str) -> list[dict]:
    if not os.path.exists(path):
        return []
    with open(path, "r", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def main() -> int:
    log_path = os.path.join(tempfile.gettempdir(), f"aad-ime-probe-{os.getpid()}.jsonl")
    probe = subprocess.Popen(
        [sys.executable, PROBE, "--title", f"AAD IME Probe {os.getpid()}",
         "--log", log_path, "--ime-open"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        ready_line = probe.stdout.readline()
        if not ready_line.strip():
            raise RuntimeError(probe.stderr.read())
        ready = json.loads(ready_line)
        window_id = f"hwnd:{ready['window_handle']}"
        found, find_code = aad("find", window_id, "--role", "edit")
        if find_code != 0:
            raise RuntimeError(found)
        target = found["target"]
        ref = f"{target['snapshot_id']}:{target['revision']}:{target['node_id']}"
        before = len(read_log(log_path))
        result, code = aad(
            "do", "type-text", "--target", ref, "--text", "abc", "--mode", "keys"
        )
        time.sleep(0.3)
        raw = [
            entry for entry in read_log(log_path)[before:]
            if entry.get("channel") == "raw_input_keyboard"
        ]
        findings = {
            "isolated_probe": True,
            "ime_open_requested": ready.get("ime_open_requested"),
            "ime_opened": ready.get("ime_opened"),
            "exit_nonzero": code != 0,
            "error_code": result.get("error", {}).get("code"),
            "raw_input_events": len(raw),
            "sent_no_keys": len(raw) == 0,
        }
        findings["passed"] = (
            findings["ime_opened"] is True
            and findings["exit_nonzero"] is True
            and findings["error_code"] == "DRIVER.INPUT_METHOD_ACTIVE"
            and findings["sent_no_keys"] is True
        )
        print(json.dumps(findings, indent=2, sort_keys=True))
        return 0 if findings["passed"] else 1
    finally:
        probe.terminate()
        try:
            probe.wait(timeout=5)
        except subprocess.TimeoutExpired:
            probe.kill()
        try:
            os.remove(log_path)
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
