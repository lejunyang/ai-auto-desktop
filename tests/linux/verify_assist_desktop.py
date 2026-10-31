#!/usr/bin/env python3
"""Check `aad assist --desktop` on X11, inside an Xvfb of its own.

Needs Xvfb, openbox, xev, xdotool, xsetroot and xclip. Starts a private X
server, so the user's desktop is not touched. Checks, each by an effect:
- the stream's pixels are the screen's (the root colour set with xsetroot);
- a still screen sends nothing; moving a window sends patches smaller than
  the screen, and the picture patched together matches a fresh keyframe;
- the pointer goes where the viewer points (read back with xdotool);
- clicks, keys, typed text (a character the keyboard map lacks included) and
  the wheel reach an xev window as X events; Shift held by a viewer that
  leaves is let go;
- the clipboard comes back as xclip holds it; cursor messages carry a shape;
- --list-screens lists the display and the xev window, and --window streams
  just that window.

AAD overrides the aad binary (default target/debug/aad).
"""
import base64
import io
import json
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(REPO, "tests", "windows"))
from PIL import Image  # noqa: E402
import verify_assist  # noqa: E402
from verify_assist import Viewer  # noqa: E402
from verify_assist_tiles import apply, patches, worst_tile  # noqa: E402
from verify_assist_video import frames_until, listen  # noqa: E402

AAD = os.environ.get("AAD", os.path.join(REPO, "target", "debug", "aad"))
verify_assist.AAD = AAD
DISPLAY = ":77"
WIDTH, HEIGHT = 1280, 800


def start_assist(*arguments):
    process = subprocess.Popen([AAD, "--compact", "assist", *arguments], stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, encoding="utf-8")
    line = process.stdout.readline()
    if not line:
        raise RuntimeError(process.stderr.read())
    return process, json.loads(line)


def x(*command, **kwargs):
    return subprocess.run(list(command), capture_output=True, text=True, **kwargs)


def tiles_frame(viewer, seconds=8):
    _, frames = frames_until(viewer, seconds, lambda s, f: any(x["codec"] == 6 for x in f))
    return [f for f in frames if f["codec"] == 6]


def collect(viewer, kind, seconds):
    import queue
    found, deadline = [], time.time() + seconds
    while time.time() < deadline:
        try:
            message = viewer.inbox.get(timeout=0.1)
        except queue.Empty:
            continue
        if message and message.get("type") == kind:
            found.append(message)
    return found


def main():
    os.environ["DISPLAY"] = DISPLAY
    server = subprocess.Popen(["Xvfb", DISPLAY, "-screen", "0", f"{WIDTH}x{HEIGHT}x24", "-nolisten", "tcp"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1.0)
    manager = subprocess.Popen(["openbox"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1.0)
    x("xsetroot", "-solid", "#336699")
    events = open("/tmp/aad-assist-xev.log", "w")
    xev = subprocess.Popen(["xev", "-geometry", "400x300+200+150", "-name", "AADAssistProbe"],
                           stdout=events, stderr=subprocess.DEVNULL)
    time.sleep(1.5)
    result = {}
    try:
        listed = json.loads(x(AAD, "--compact", "assist", "--list-screens").stdout)
        probe = next((w for w in listed["windows"] if "AADAssistProbe" in w["title"]), None)
        result["lists_display_and_window"] = bool(listed["monitors"]) and probe is not None
        assist, announce = start_assist("--desktop", "--codecs", "tiles")
        try:
            result["capture"] = announce["source"].get("capture")
            viewer = listen(Viewer(announce["websocket"]))
            # Cursor messages are kept as they pass, the first with its shape.
            cursors = []
            original = viewer.inbox.get

            def tapped(*a, **k):
                message = original(*a, **k)
                if message and message.get("type") == "cursor":
                    cursors.append(message)
                return message

            viewer.inbox.get = tapped
            viewer.send({"type": "hello", "accept": ["tiles"]})
            frames = tiles_frame(viewer)
            canvas = None
            for frame in frames:
                canvas = apply(canvas, frame)
            result["frame_size"] = list(canvas.size)
            # Lossy WebP: within a few steps of the colour.
            pixel = canvas.getpixel((10, 10))
            result["root_pixel"] = list(pixel)
            result["root_colour_seen"] = all(abs(a - b) <= 6 for a, b in zip(pixel, (0x33, 0x66, 0x99)))
            _, still = frames_until(viewer, 3.0)
            result["still_screen_frames"] = len([f for f in still if f["codec"] == 6])
            # Move the window: only where it was and is are sent.
            wid = probe["window"] if probe else None
            x("xdotool", "windowmove", str(int(wid, 16)), "420", "260")
            moved = tiles_frame(viewer, 3)
            area = sum(p["rect"][2] * p["rect"][3] for f in moved for p in patches(f))
            result["move_patch_share"] = round(area / (WIDTH * HEIGHT), 3)
            for frame in moved:
                canvas = apply(canvas, frame)
            viewer.send({"type": "keyframe"})
            later = tiles_frame(viewer, 6)
            index = next(i for i, f in enumerate(later) if f["keyframe"])
            for frame in later[:index]:
                canvas = apply(canvas, frame)
            result["worst_tile_vs_keyframe"] = round(worst_tile(canvas, apply(None, later[index])), 1)
            # The pointer goes where the viewer points.
            viewer.send({"type": "mouse", "action": "move", "x": 0.5, "y": 0.5})
            time.sleep(0.3)
            location = dict(re.findall(r"(\w+):(\d+)", x("xdotool", "getmouselocation").stdout))
            result["pointer_at"] = [int(location.get("x", -1)), int(location.get("y", -1))]
            # Click in the xev window (now at 420,260), type, scroll.
            fx, fy = (420 + 200) / WIDTH, (260 + 150) / HEIGHT
            for action in ("move", "down", "up"):
                viewer.send({"type": "mouse", "action": action, "button": "left", "x": fx, "y": fy})
                time.sleep(0.1)
            viewer.send({"type": "text", "text": "aZ;中"})
            viewer.send({"type": "key", "action": "down", "key": "Control", "code": "ControlLeft"})
            viewer.send({"type": "key", "action": "up", "key": "Control", "code": "ControlLeft"})
            viewer.send({"type": "mouse", "action": "wheel", "x": fx, "y": fy, "dx": 0, "dy": 100})
            # Hold Shift and leave.
            viewer.send({"type": "key", "action": "down", "key": "Shift", "code": "ShiftLeft"})
            collect(viewer, "cursor", 1.0)
            viewer.close()
            time.sleep(1.0)
            # The clipboard as xclip holds it.
            subprocess.run(["xclip", "-selection", "clipboard"], input="from the X clipboard", text=True)
            second = listen(Viewer(announce["websocket"]))
            second.send({"type": "clipboard"})
            answers, errors = [], []
            deadline = time.time() + 6
            while time.time() < deadline and not answers and not errors:
                try:
                    message = second.inbox.get(timeout=6)
                except Exception:
                    errors.append(f"nothing from the server; it exited: {assist.poll()}")
                    break
                if message and message.get("type") == "clipboard":
                    answers.append(message)
                elif message and message.get("type") == "error":
                    errors.append(message["message"])
            result["clipboard_errors"] = errors
            second.close()
        finally:
            assist.terminate()
            assist.wait()
            result["assist_stderr"] = assist.stderr.read()[-800:]
        events.flush()
        log = open("/tmp/aad-assist-xev.log").read()
        # What each key press typed, as xev decodes it.
        presses = re.findall(r'KeyPress event[^\n]*\n[^\n]*\n[^\n]*\n\s+XLookupString gives \d+ bytes: (?:\([0-9a-f ]+\) )?"(.*)"', log)
        result["typed_strings"] = [t for t in presses if t]
        result["text_reaches_window"] = "aZ;中" in "".join(presses)
        result["click_reaches_window"] = "ButtonPress event" in log and "button 1," in log
        result["ctrl_reaches_window"] = "Control_L" in log
        result["wheel_reaches_window"] = "button 5," in log
        result["shift_let_go"] = bool(re.search(r"KeyRelease event.*\n.*\n.*Shift_L", log))
        shaped = [c for c in cursors if c.get("shape")]
        result["cursor_shape"] = bool(shaped) and Image.open(io.BytesIO(base64.b64decode(shaped[0]["shape"]["png"]))).size == (
            shaped[0]["shape"]["width"], shaped[0]["shape"]["height"])
        result["clipboard"] = answers[0].get("text") if answers else None
        # Just the window.
        if probe:
            assist, announce = start_assist("--desktop", "--window", probe["window"], "--codecs", "tiles", "--view-only")
            try:
                viewer = listen(Viewer(announce["websocket"]))
                viewer.send({"type": "hello", "accept": ["tiles"]})
                window_frames = tiles_frame(viewer)
                viewer.close()
                result["window_frame_size"] = [window_frames[0]["width"], window_frames[0]["height"]]
            finally:
                assist.terminate()
                assist.wait()
    finally:
        xev.terminate()
        manager.terminate()
        server.terminate()
    result["passed"] = (result.get("lists_display_and_window") and result.get("capture") == "x11-damage"
                        and result.get("frame_size") == [WIDTH, HEIGHT] and result.get("root_colour_seen")
                        and result.get("still_screen_frames") == 0 and 0 < result.get("move_patch_share", 1) < 0.5
                        and result.get("worst_tile_vs_keyframe", 99) < 24
                        and abs(result["pointer_at"][0] - WIDTH // 2) <= 1 and abs(result["pointer_at"][1] - HEIGHT // 2) <= 1
                        and result["text_reaches_window"] and result["click_reaches_window"]
                        and result["ctrl_reaches_window"] and result["wheel_reaches_window"] and result["shift_let_go"]
                        and result["cursor_shape"] and result["clipboard"] == "from the X clipboard"
                        and result.get("window_frame_size") == [400, 300])
    print(json.dumps(result, ensure_ascii=False, indent=1))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
