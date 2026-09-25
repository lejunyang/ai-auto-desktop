#!/usr/bin/env python3
"""End-to-end proof that `aad do key-chord` sends real combinations.

Not "the command returned ok" -- that only says SendInput was accepted. This
drives the whole stack the way an agent would (aad apps -> find -> do) and reads
the *probe's* per-channel log to confirm the exact keys and modifier state that
arrived. The probe is input_channel_probe.py, which records Raw Input, window
messages and GetAsyncKeyState separately.

It briefly takes foreground focus while the chords are sent.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
PROBE = os.path.join(HERE, "input_channel_probe.py")
REPO = os.path.dirname(os.path.dirname(HERE))
AAD = os.path.join(REPO, "target", "debug", "aad.exe")

# VK codes the probe reports, for asserting on what arrived.
VK = {"A": 65, "F5": 0x74, "HOME": 0x24, "LEFT": 0x25, "CONTROL": 0x11, "SHIFT": 0x10}


def aad(*args: str) -> dict:
    result = subprocess.run(
        [AAD, "--compact", *args] if args and args[0] in {"find", "do"} else [AAD, *args],
        capture_output=True, text=True, timeout=30,
    )
    text = result.stdout.strip()
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return {"_raw": text, "_stderr": result.stderr.strip(), "_code": result.returncode}


def read_log(path: str) -> list[dict]:
    if not os.path.exists(path):
        return []
    with open(path, "r", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def main() -> int:
    if not os.path.exists(AAD):
        print(f"aad.exe not built at {AAD}", file=sys.stderr)
        return 2

    log_path = os.path.join(tempfile.gettempdir(), f"aad-e2e-chord-{os.getpid()}.jsonl")
    if os.path.exists(log_path):
        os.remove(log_path)
    title = f"AAD Chord E2E {os.getpid()}"
    probe = subprocess.Popen(
        [sys.executable, PROBE, "--title", title, "--log", log_path],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    ready = json.loads(probe.stdout.readline())
    findings: dict = {"probe": ready, "steps": []}
    time.sleep(0.6)

    try:
        # Find the probe window by title, then the EDIT inside it -- a focusable
        # element that key_chord can be anchored to.
        apps = aad("apps")
        window_id = None
        for window in apps.get("windows", apps if isinstance(apps, list) else []):
            if isinstance(window, dict) and title in str(window.get("title", "")):
                window_id = window.get("window_id")
                break
        findings["window_id"] = window_id
        if not window_id:
            findings["error"] = "probe window not found via aad apps"
            findings["apps_raw"] = apps
            return finish(probe, findings, log_path, 1)

        found = aad("find", window_id, "--role", "Edit")
        target = found.get("target")
        if not target:
            # Fall back to the window itself if the EDIT is not addressable.
            found = aad("find", window_id, "--role", "Window")
            target = found.get("target")
        findings["find"] = found
        if not target:
            findings["error"] = "no focusable element found"
            return finish(probe, findings, log_path, 1)

        target_ref = f"{target['snapshot_id']}:{target['revision']}:{target['node_id']}"

        # Each chord: send it, then read what the probe recorded for it.
        for label, keys, expect_vk, expect_mods in [
            ("ctrl_a", "ctrl,a", VK["A"], {"ctrl"}),
            ("f5", "f5", VK["F5"], set()),
            ("shift_home", "shift,home", VK["HOME"], {"shift"}),
            ("ctrl_shift_left", "ctrl,shift,left", VK["LEFT"], {"ctrl", "shift"}),
        ]:
            before = len(read_log(log_path))
            result = aad("do", "key-chord", "--target", target_ref, "--keys", keys)
            time.sleep(0.5)
            entries = read_log(log_path)[before:]

            # What did Raw Input see for the main key going down?
            raw_down = [
                e for e in entries
                if e["channel"] == "raw_input_keyboard" and e.get("vkey") == expect_vk
                and e.get("message") == 256  # WM_KEYDOWN
            ]
            # Window message for the main key, with modifier flags at that moment.
            win_down = [
                e for e in entries
                if e["channel"] == "window_message" and e.get("vkey") == expect_vk
                and e.get("message") in ("WM_KEYDOWN", "WM_SYSKEYDOWN")
            ]
            mods_seen = set()
            if win_down:
                if win_down[0].get("ctrl_down"):
                    mods_seen.add("ctrl")
                if win_down[0].get("shift_down"):
                    mods_seen.add("shift")
                if win_down[0].get("alt_down"):
                    mods_seen.add("alt")

            findings["steps"].append({
                "label": label, "keys": keys,
                "command_applied": result.get("applied", result),
                "raw_input_saw_key": len(raw_down) > 0,
                "window_msg_saw_key": len(win_down) > 0,
                "modifiers_expected": sorted(expect_mods),
                "modifiers_seen": sorted(mods_seen),
                "modifiers_match": mods_seen == expect_mods,
                "entry_count": len(entries),
            })

        # After all chords: are any modifiers left latched? (leak check on the
        # real path, not the raw SendInput path.)
        import ctypes
        user32 = ctypes.WinDLL("user32", use_last_error=True)
        user32.GetAsyncKeyState.restype = ctypes.c_short
        findings["no_modifier_leak"] = {
            "ctrl": not bool(user32.GetAsyncKeyState(0x11) & 0x8000),
            "shift": not bool(user32.GetAsyncKeyState(0x10) & 0x8000),
            "alt": not bool(user32.GetAsyncKeyState(0x12) & 0x8000),
        }

        return finish(probe, findings, log_path, 0)
    finally:
        pass


def finish(probe, findings: dict, log_path: str, code: int) -> int:
    try:
        probe.terminate()
        probe.wait(timeout=5)
    except Exception:
        probe.kill()
    print(json.dumps(findings, indent=2, ensure_ascii=False))
    return code


if __name__ == "__main__":
    raise SystemExit(main())
