#!/usr/bin/env python3
"""Verify type_text --mode keys through the full aad stack on an isolated window."""

from __future__ import annotations

import ctypes
from ctypes import wintypes
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
WM_GETTEXT = 0x000D
WM_GETTEXTLENGTH = 0x000E

user32 = ctypes.WinDLL("user32", use_last_error=True)
user32.SendMessageW.argtypes = [
    wintypes.HWND,
    wintypes.UINT,
    ctypes.c_size_t,
    ctypes.c_ssize_t,
]
user32.SendMessageW.restype = ctypes.c_ssize_t


def aad(*args: str, expect_ok: bool = True) -> tuple[dict, int]:
    prefix = [AAD, "--compact"] if args[0] in {"find", "do"} else [AAD]
    result = subprocess.run([*prefix, *args], capture_output=True, text=True, timeout=30)
    try:
        payload = json.loads(result.stdout.strip())
    except json.JSONDecodeError:
        raise RuntimeError(f"aad returned non-JSON: {result.stdout!r} {result.stderr!r}")
    if expect_ok and result.returncode != 0:
        raise RuntimeError(f"aad {' '.join(args)} failed: {payload}")
    return payload, result.returncode


def read_log(path: str) -> list[dict]:
    if not os.path.exists(path):
        return []
    with open(path, "r", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def edit_text(hwnd: int) -> str:
    length = int(user32.SendMessageW(wintypes.HWND(hwnd), WM_GETTEXTLENGTH, 0, 0))
    buffer = ctypes.create_unicode_buffer(length + 1)
    user32.SendMessageW(wintypes.HWND(hwnd), WM_GETTEXT, length + 1, ctypes.addressof(buffer))
    return buffer.value


def target_ref(window_id: str) -> str:
    found, _ = aad("find", window_id, "--role", "edit")
    target = found["target"]
    return f"{target['snapshot_id']}:{target['revision']}:{target['node_id']}"


def main() -> int:
    log_path = os.path.join(tempfile.gettempdir(), f"aad-key-text-{os.getpid()}.jsonl")
    try:
        os.remove(log_path)
    except FileNotFoundError:
        pass
    title = f"AAD Key Text Probe {os.getpid()}"
    probe = subprocess.Popen(
        [
            sys.executable,
            PROBE,
            "--title",
            title,
            "--log",
            log_path,
            "--keyboard-layout",
            "00000409",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        ready_line = probe.stdout.readline()
        if not ready_line.strip():
            raise RuntimeError(probe.stderr.read())
        ready = json.loads(ready_line)
        window_id = f"hwnd:{ready['window_handle']}"
        ref = target_ref(window_id)

        aad("do", "set-value", "--target", ref, "--value", "")
        before = len(read_log(log_path))
        sample = "aA1!"
        response, code = aad(
            "do", "type-text", "--target", ref, "--text", sample, "--mode", "keys",
            expect_ok=False,
        )
        time.sleep(0.6)
        entries = read_log(log_path)[before:]
        raw_keys = [entry for entry in entries if entry.get("channel") == "raw_input_keyboard"]
        actual = edit_text(ready["edit_handle"])

        # The mapper must reject the whole string before focusing or sending if
        # one character cannot be represented by the target layout.
        ref = target_ref(window_id)
        aad("do", "set-value", "--target", ref, "--value", "")
        before_bad = len(read_log(log_path))
        bad, bad_code = aad(
            "do", "type-text", "--target", ref, "--text", "a😀", "--mode", "keys",
            expect_ok=False,
        )
        time.sleep(0.3)
        bad_entries = read_log(log_path)[before_bad:]
        bad_raw = [entry for entry in bad_entries if entry.get("channel") == "raw_input_keyboard"]
        after_bad = edit_text(ready["edit_handle"])

        findings = {
            "isolated_probe": True,
            "probe_keyboard_layout": ready.get("active_keyboard_layout"),
            "keys_exit_code": code,
            "keys_applied": response.get("applied") is True,
            "expected_text": sample,
            "actual_text": actual,
            "text_exact": actual == sample,
            "raw_input_keyboard_events": len(raw_keys),
            "raw_input_visible": len(raw_keys) >= len(sample) * 2,
            "raw_input_vkeys_nonzero": bool(raw_keys)
            and all(int(entry.get("vkey", 0)) != 0 for entry in raw_keys),
            "unmappable_exit_nonzero": bad_code != 0,
            "unmappable_code": bad.get("error", {}).get("code"),
            "unmappable_left_no_prefix": after_bad == "",
            "unmappable_sent_no_keys": len(bad_raw) == 0,
        }
        required = [
            "keys_applied",
            "text_exact",
            "raw_input_visible",
            "raw_input_vkeys_nonzero",
            "unmappable_exit_nonzero",
            "unmappable_left_no_prefix",
            "unmappable_sent_no_keys",
        ]
        findings["passed"] = all(findings[key] is True for key in required)
        print(json.dumps(findings, indent=2, ensure_ascii=False, sort_keys=True))
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
