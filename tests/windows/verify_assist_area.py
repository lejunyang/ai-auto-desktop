#!/usr/bin/env python3
"""Check sharing one display or one window with `aad assist --desktop`.

- --list-screens names every display and titled window with its bounds;
- --monitor N streams that display at its own size; a missing one is refused;
- --window HANDLE streams just that window: the first frame is the window's
  size; a click at a place in the picture lands at that place in the window,
  also after the window moved (input follows it); after a resize the stream
  restarts at the new size; the pointer is reported relative to the window;
  the picture patched together matches a fresh keyframe tile by tile.

The window part creates an isolated probe window and moves the real pointer
and keyboard, so run it on its own.
"""
import ctypes
import ctypes.wintypes as wintypes
import json
import os
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from verify_assist import AAD, REPO, Viewer, start_assist  # noqa: E402
from verify_assist_tiles import apply, worst_tile  # noqa: E402
from verify_assist_video import frames_until, listen, reported  # noqa: E402

user32 = ctypes.windll.user32
dwmapi = ctypes.windll.dwmapi
user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))  # per-monitor v2, as aad is


def frame_bounds(window):
    rect = wintypes.RECT()
    dwmapi.DwmGetWindowAttribute(wintypes.HWND(window), 9, ctypes.byref(rect), ctypes.sizeof(rect))
    return rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top


@reported
def listing_check():
    listed = json.loads(subprocess.run([AAD, "--compact", "assist", "--list-screens"], capture_output=True,
                                       text=True, encoding="utf-8").stdout)
    width, height = user32.GetSystemMetrics(0), user32.GetSystemMetrics(1)
    primary = [m for m in listed["monitors"] if m["primary"]]
    missing = subprocess.run([AAD, "--compact", "assist", "--desktop", "--monitor", "99"], capture_output=True,
                             text=True, encoding="utf-8").stdout
    assist, announce = start_assist("--desktop", "--view-only", "--monitor", str(listed["monitors"].index(primary[0])),
                                    "--codecs", "tiles", "--max-width", "0", "--hide-cursor")
    try:
        viewer = listen(Viewer(announce["websocket"]))
        viewer.send({"type": "hello", "accept": ["tiles"]})
        _, frames = frames_until(viewer, 8, lambda s, f: any(x["codec"] == 6 for x in f))
        viewer.close()
    finally:
        assist.terminate()
        assist.wait()
    first = next(f for f in frames if f["codec"] == 6)
    result = {"monitors": len(listed["monitors"]), "windows": len(listed["windows"]),
              "primary_matches_screen": bool(primary) and (primary[0]["width"], primary[0]["height"]) == (width, height),
              "windows_have_bounds_and_titles": all(w["title"] and w["width"] >= 0 for w in listed["windows"]),
              "monitor_frame_size": [first["width"], first["height"]],
              "monitor_frame_is_the_display": (first["width"], first["height"]) == (width, height),
              "missing_monitor_refused": "there are" in missing}
    result["passed"] = all(v for k, v in result.items() if k not in ("monitors", "windows", "monitor_frame_size"))
    return result


@reported
def window_check():
    log = os.path.join(tempfile.gettempdir(), f"aad-assist-area-{os.getpid()}.jsonl")
    probe = subprocess.Popen([sys.executable, os.path.join(REPO, "tests", "windows", "input_channel_probe.py"),
                              "--title", f"AAD Assist Area Probe {os.getpid()}", "--log", log],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        ready = json.loads(probe.stdout.readline())
        window, edit = ready["window_handle"], ready["edit_handle"]
        user32.SetForegroundWindow(window)
        time.sleep(0.5)
        assist, announce = start_assist("--desktop", "--window", f"0x{window:x}", "--codecs", "tiles", "--max-width", "0")
        buffer = ctypes.create_unicode_buffer(256)

        def text():
            time.sleep(0.4)
            user32.SendMessageW(edit, 0x000D, 256, buffer)
            return buffer.value

        def edit_fraction():
            left, top, width, height = frame_bounds(window)
            rect = wintypes.RECT()
            user32.GetWindowRect(edit, ctypes.byref(rect))
            return ((rect.left + rect.right) / 2 - left) / width, ((rect.top + rect.bottom) / 2 - top) / height

        try:
            viewer = listen(Viewer(announce["websocket"]))
            viewer.send({"type": "hello", "accept": ["tiles"]})
            cursors = []
            original = viewer.inbox.get

            def tapped(*a, **k):
                message = original(*a, **k)
                if message and message.get("type") == "cursor":
                    cursors.append(message)
                return message

            viewer.inbox.get = tapped
            _, frames = frames_until(viewer, 8, lambda s, f: any(x["codec"] == 6 for x in f))
            frames = [f for f in frames if f["codec"] == 6]
            bounds = frame_bounds(window)
            first_size = [frames[0]["width"], frames[0]["height"]]
            x, y = edit_fraction()
            for action in ("move", "down", "up"):
                viewer.send({"type": "mouse", "action": action, "button": "left", "x": x, "y": y})
                time.sleep(0.08)
            viewer.send({"type": "text", "text": "in place"})
            before_move = text()
            # Cursor messages are read below, with the frames after this.
            click_at = (x, y)
            # Move the window; the same place in the picture still finds the edit box.
            user32.SetWindowPos(window, 0, bounds[0] + 80, bounds[1] + 60, 0, 0, 0x0001 | 0x0004)
            time.sleep(0.8)
            user32.SendMessageW(edit, 0x000C, 0, "")
            for action in ("move", "down", "up"):
                viewer.send({"type": "mouse", "action": action, "button": "left", "x": x, "y": y})
                time.sleep(0.08)
            viewer.send({"type": "text", "text": "after move"})
            after_move = text()
            # Resize: the stream starts again at the new size.
            user32.SetWindowPos(window, 0, 0, 0, bounds[2] + 120, bounds[3] + 40, 0x0002 | 0x0004)
            _, resized = frames_until(viewer, 5, lambda s, f: any(x["codec"] == 6 and x["width"] != first_size[0] for x in f))
            new = [f for f in resized if f["codec"] == 6 and f["width"] != first_size[0]]
            new_bounds = frame_bounds(window)
            # Patched picture against a fresh keyframe of the same window.
            canvas = None
            for frame in [f for f in resized if f["codec"] == 6]:
                canvas = apply(canvas, frame)
            viewer.send({"type": "keyframe"})
            _, later = frames_until(viewer, 6, lambda s, f: any(x["codec"] == 6 and x["keyframe"] for x in f))
            later = [f for f in later if f["codec"] == 6]
            index = next(i for i, f in enumerate(later) if f["keyframe"])
            for frame in later[:index]:
                canvas = apply(canvas, frame)
            reference = apply(None, later[index])
            viewer.close()
        finally:
            assist.terminate()
            assist.wait()
        pointer_at_click = [c for c in cursors if c.get("visible") and abs(c["x"] - click_at[0]) < 0.02
                            and abs(c["y"] - click_at[1]) < 0.02]
        result = {
            "window_bounds": list(bounds), "first_frame_size": first_size,
            "first_frame_is_the_window": first_size == [bounds[2], bounds[3]],
            "click_and_typing_land": before_move == "in place",
            "pointer_reported_relative_to_window": bool(pointer_at_click),
            "input_follows_moved_window": after_move == "after move", "after_move_text": after_move,
            "resize_restarts_at_new_size": bool(new) and new[0]["keyframe"]
            and [new[0]["width"], new[0]["height"]] == [new_bounds[2], new_bounds[3]],
            "worst_tile_difference": round(worst_tile(canvas, reference), 1) if canvas.size == reference.size else None,
        }
        result["passed"] = (result["first_frame_is_the_window"] and result["click_and_typing_land"]
                            and result["pointer_reported_relative_to_window"] and result["input_follows_moved_window"]
                            and result["resize_restarts_at_new_size"]
                            and result["worst_tile_difference"] is not None and result["worst_tile_difference"] < 24)
        return result
    finally:
        probe.terminate()


def main():
    findings = {"listing": listing_check()}
    if "--window" in sys.argv:
        findings["window"] = window_check()
    print(json.dumps(findings, ensure_ascii=False, indent=1))
    return 0 if all(part["passed"] for part in findings.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
