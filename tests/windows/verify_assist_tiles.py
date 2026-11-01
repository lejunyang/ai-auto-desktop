#!/usr/bin/env python3
"""Check `aad assist` tiles and adaptive streaming end to end.

Browser (headless Chrome, the rich fixture page of verify_assist_video):
- tiles: the stream starts with one patch covering the frame; a still page sends
  nothing for 6 s; a click sends only small patches near the button; the
  picture rebuilt from every patch matches a whole WebP frame of the same page;
  the clicks and typing really change the page;
- adaptive (when ffmpeg has an H.264 encoder): a large animation switches to
  video, each video frame naming only the animated region as changed, and once
  it stops that region is sent again as tiles; the built-in viewer, in a Chrome
  of its own, then shows the page.

With --desktop the Windows desktop is streamed as tiles: it reports DXGI
capture, the rebuilt picture matches a whole frame, a click and typing land in
an isolated probe window (this moves the real pointer, so run it on its own),
and the picture patched together matches a fresh whole keyframe of the screen,
and cursor messages carry a decodable shape and follow the pointer.
"""
import base64
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from PIL import Image, ImageChops, ImageStat  # noqa: E402
from verify_assist import AAD, CHROME, REPO, Viewer, start_assist  # noqa: E402
from verify_assist_video import (browser_page, cdp, describe, frames_until, listen,  # noqa: E402
                                 reported)


def patches(frame):
    data, at, found = frame["data"], 2, []
    for _ in range(int.from_bytes(data[:2], "big")):
        kind = data[at]
        x, y, w, h = (int.from_bytes(data[at + 2 + 2 * i:at + 4 + 2 * i], "big") for i in range(4))
        at += 10
        if kind == 2:
            found.append({"kind": "copy", "rect": (x, y, w, h),
                          "from": (int.from_bytes(data[at:at + 2], "big"), int.from_bytes(data[at + 2:at + 4], "big"))})
            at += 4
        else:
            length = int.from_bytes(data[at:at + 4], "big")
            found.append({"kind": "picture", "rect": (x, y, w, h), "data": data[at + 4:at + 4 + length]})
            at += 4 + length
    assert at == len(data), "patch payload length"
    return found


def apply(canvas, frame):
    """Draws a tiles frame as the viewer does; returns the canvas."""
    if frame["keyframe"] or canvas is None or canvas.size != (frame["width"], frame["height"]):
        canvas = Image.new("RGB", (frame["width"], frame["height"]))
    for patch in patches(frame):
        x, y, w, h = patch["rect"]
        if patch["kind"] == "copy":
            sx, sy = patch["from"]
            canvas.paste(canvas.crop((sx, sy, sx + w, sy + h)), (x, y))
        else:
            picture = Image.open(io.BytesIO(patch["data"])).convert("RGB")
            assert picture.size == (w, h), (picture.size, (w, h))
            canvas.paste(picture, (x, y))
    return canvas


def difference(a, b):
    """Mean absolute difference per channel, 0..255."""
    return sum(ImageStat.Stat(ImageChops.difference(a, b)).mean) / 3


def worst_tile(a, b, tile=32):
    """The largest mean difference of any tile: a patch put in the wrong place
    or never sent shows as a whole wrong tile, lossy coding only at edges."""
    delta = ImageChops.difference(a, b).convert("L")
    worst = 0.0
    for y in range(0, delta.height, tile):
        for x in range(0, delta.width, tile):
            worst = max(worst, ImageStat.Stat(delta.crop((x, y, x + tile, y + tile))).mean[0])
    return worst


def whole_webp(viewer):
    viewer.send({"type": "hello", "accept": ["webp"]})
    _, frames = frames_until(viewer, 8, lambda s, f: any(x["codec"] == "webp" for x in f))
    frame = next(x for x in frames if x["codec"] == "webp")
    return Image.open(io.BytesIO(frame["data"])).convert("RGB")


def near(rect, area, margin=32):
    x, y, w, h = rect
    left, top, right, bottom = area
    return x < right + margin and x + w > left - margin and y < bottom + margin and y + h > top - margin


@reported
def tiles_check(port, page_id):
    client, call = cdp(port, page_id)
    call("Page.reload")
    client.close()
    time.sleep(1.5)
    assist, announce = start_assist("--browser-port", port, "--page", page_id, "--codecs", "tiles,webp")
    try:
        viewer = listen(Viewer(announce["websocket"]))
        viewer.send({"type": "hello", "accept": ["tiles"]})
        _, frames = frames_until(viewer, 8, lambda s, f: any(x["codec"] == 6 for x in f))
        frames = [f for f in frames if f["codec"] == 6]
        _, settling = frames_until(viewer, 1.5)
        frames += settling
        key = frames[0]
        _, still = frames_until(viewer, 6.0)
        width, height = key["width"], key["height"]
        viewer.click(140 / width, 70 / height)
        _, clicked = frames_until(viewer, 1.0)
        viewer.click(140 / width, 70 / height)
        viewer.click(190 / width, 180 / height)
        viewer.send({"type": "text", "text": "typed as tiles"})
        _, typed = frames_until(viewer, 1.5)
        canvas = None
        for frame in frames + still + clicked + typed:
            canvas = apply(canvas, frame)
        reference = whole_webp(viewer)
        viewer.close()
        text = describe(port, "Assist fixture")
        button = (40, 40, 240, 100)
        click_patches = [p for f in clicked for p in patches(f)]
        result = {
            "size": [width, height], "starts_whole": key["keyframe"] and len(patches(key)) == 1
            and patches(key)[0]["rect"] == (0, 0, width, height),
            "keyframe_bytes": len(key["data"]), "still_page_frames": len(still),
            "click_frames": len(clicked), "click_bytes": sum(len(f["data"]) for f in clicked),
            "click_patches_near_button": bool(click_patches) and all(near(p["rect"], button) for p in click_patches),
            "typing_bytes": sum(len(f["data"]) for f in typed),
            "rebuilt_size": list(canvas.size), "whole_size": list(reference.size),
            "rebuilt_vs_whole_difference": round(difference(canvas, reference), 2),
            "worst_tile_difference": round(worst_tile(canvas, reference), 1),
            "button_pressed_twice": "pressed 2" in text, "text_typed": "typed as tiles" in text,
        }
        result["passed"] = (result["starts_whole"] and result["still_page_frames"] == 0
                            and canvas.size == reference.size
                            and result["click_patches_near_button"]
                            and result["click_bytes"] * 10 < result["keyframe_bytes"]
                            and result["rebuilt_vs_whole_difference"] < 4 and result["worst_tile_difference"] < 24
                            and result["button_pressed_twice"] and result["text_typed"])
        return result
    finally:
        assist.terminate()
        assist.wait()


ANIMATE = """(()=>{const d=document.createElement('div');d.id='flash';
d.style.cssText='position:fixed;left:0;top:0;width:100%;height:70%;z-index:9';document.body.appendChild(d);
let n=0;const t=setInterval(()=>{d.style.background=`hsl(${(n++*37)%360},70%,50%)`;if(n>30){clearInterval(t);}},50);})()"""


@reported
def adaptive_check(port, page_id):
    client, call = cdp(port, page_id)
    call("Page.reload")
    time.sleep(1.5)
    assist, announce = start_assist("--browser-port", port, "--page", page_id, "--codecs", "tiles,h264,webp")
    try:
        viewer = listen(Viewer(announce["websocket"]))
        viewer.send({"type": "hello", "accept": ["tiles", "h264"]})
        _, start = frames_until(viewer, 8, lambda s, f: any(x["codec"] == 6 for x in f))
        frames_until(viewer, 1.0)
        call("Runtime.evaluate", {"expression": ANIMATE})
        streams, during = frames_until(viewer, 4.0)
        client.close()
        kinds = [f["codec"] for f in during]
        last_video = max((i for i, k in enumerate(kinds) if k == "h264"), default=-1)
        after = [f for f in during[last_video + 1:] if f["codec"] == 6]
        refined = [p["rect"] for f in after for p in patches(f)]
        covered = sum(w * h for _, _, w, h in refined)
        width, height = start[0]["width"], start[0]["height"]
        viewer.close()
        videos = [f for f in during if f["codec"] == "h264"]
        # Each video frame says where it changed: inside the animated top 70%.
        regions_inside = bool(videos) and all(
            f["regions"] and all(y + h <= height * 0.7 + 32 for _, y, _, h in f["regions"]) for f in videos)
        result = {"starts_with_tiles": start[0]["codec"] == 6, "video_stream": streams[0] if streams else None,
                  "video_regions_inside_animation": regions_inside,
                  "video_frames": kinds.count("h264"), "tiles_frames": kinds.count(6),
                  "tiles_after_video": len(after),
                  "refined_share_of_animated_area": round(covered / (width * height * 0.7), 2)}
        result["passed"] = (result["starts_with_tiles"] and result["video_frames"] > 0 and bool(after)
                            and regions_inside
                            and result["refined_share_of_animated_area"] >= 0.9)
        return result
    finally:
        assist.terminate()
        assist.wait()


@reported
def viewer_tiles_check(port, page_id):
    """The built-in viewer draws tiles, then video during an animation, then
    tiles again, and the picture it ends with is the page."""
    client, call = cdp(port, page_id)
    call("Page.reload")
    time.sleep(1.5)
    assist, announce = start_assist("--browser-port", port, "--page", page_id)
    # The viewer runs in a Chrome of its own: as a second tab of the streamed
    # one it would hide the page, which then paints once a second.
    profile = tempfile.mkdtemp(prefix="aad-assist-viewer-profile-")
    second = subprocess.Popen([CHROME, "--headless=new", "--remote-debugging-port=0", f"--user-data-dir={profile}",
                               "--no-first-run", "--window-size=1000,800", announce["url"]],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        return viewer_session(profile, call)
    finally:
        second.terminate()
        assist.terminate()
        assist.wait()


def viewer_session(profile, call):
    """Waits for the viewer in its own Chrome to show tiles, animates the page,
    and compares what the viewer then draws with the page itself."""
    active = os.path.join(profile, "DevToolsActivePort")
    for _ in range(100):
        if os.path.exists(active) and open(active).read().strip():
            break
        time.sleep(0.1)
    viewer_port = open(active).read().split()[0]
    time.sleep(1.0)
    targets = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{viewer_port}/json/list").read())
    viewer, evaluate = cdp(viewer_port, next(t["id"] for t in targets if t["type"] == "page"))
    state = lambda: evaluate("Runtime.evaluate", {"returnByValue": True, "expression": """(()=>{
      const c=document.getElementById('screen'), d=c.getContext('2d').getImageData(0,0,c.width,c.height).data;
      let lit=0; for(let i=0;i<d.length;i+=4) if(d[i]|d[i+1]|d[i+2]) lit++;
      return {status:document.getElementById('status').textContent, width:c.width, height:c.height, lit}; })()"""
    })["result"]["value"]
    before = None
    for _ in range(40):
        time.sleep(0.25)
        before = state()
        if "tiles" in before["status"] and before["lit"] > 1000:
            break
    call("Runtime.evaluate", {"expression": ANIMATE})
    time.sleep(4)
    after = state()
    picture = evaluate("Runtime.evaluate", {"returnByValue": True, "expression":
                       "document.getElementById('screen').toDataURL('image/png')"})["result"]["value"]
    viewer.close()
    drawn = Image.open(io.BytesIO(base64.b64decode(picture.split(",", 1)[1]))).convert("RGB")
    # The page as it is now, straight from Chrome.
    shot = call("Page.captureScreenshot", {"format": "png"})["data"]
    # Adaptive frames are cut to an even size, not scaled.
    page = Image.open(io.BytesIO(base64.b64decode(shot))).convert("RGB").crop((0, 0) + drawn.size)
    result = {"before": before, "after": after, "drawn_vs_page_difference": round(difference(drawn, page), 2)}
    result["passed"] = ("tiles" in before["status"] and "h264 (" in after["status"]
                        and result["drawn_vs_page_difference"] < 4)
    return result


@reported
def desktop_check():
    import ctypes
    user32 = ctypes.windll.user32
    probe = subprocess.Popen([sys.executable, os.path.join(REPO, "tests", "windows", "input_channel_probe.py"),
                              "--title", f"AAD Assist Tiles Probe {os.getpid()}",
                              "--log", os.path.join(tempfile.gettempdir(), f"aad-assist-tiles-{os.getpid()}.jsonl")],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        ready = json.loads(probe.stdout.readline())
        user32.SetForegroundWindow(ready["window_handle"])
        time.sleep(0.5)
        rect = (ctypes.c_long * 4)()
        user32.GetWindowRect(ready["edit_handle"], rect)
        left, top = user32.GetSystemMetrics(76), user32.GetSystemMetrics(77)
        width, height = user32.GetSystemMetrics(78), user32.GetSystemMetrics(79)
        x = ((rect[0] + rect[2]) / 2 - left) / width
        y = ((rect[1] + rect[3]) / 2 - top) / height
        assist, announce = start_assist("--desktop", "--codecs", "tiles,webp")
        try:
            viewer = listen(Viewer(announce["websocket"]))
            viewer.send({"type": "hello", "accept": ["tiles"]})
            cursors = []
            viewer_messages = []

            def collect(seconds, stop=None):
                streams, frames = frames_until(viewer, seconds, stop)
                return frames

            # frames_until drops other messages; read cursor messages through a tap.
            original = viewer.inbox.get

            def tapped(*a, **k):
                message = original(*a, **k)
                if message and message.get("type") == "cursor":
                    cursors.append(message)
                viewer_messages.append(message and message.get("type"))
                return message

            viewer.inbox.get = tapped
            frames = collect(8, lambda s, f: any(x["codec"] == 6 for x in f))
            frames = [f for f in frames if f["codec"] == 6]
            frames += collect(1.0)
            key = frames[0]
            idle = collect(3.0)
            # Something else may have come to the front meanwhile.
            user32.SetForegroundWindow(ready["window_handle"])
            time.sleep(0.3)
            in_front = user32.GetForegroundWindow() == ready["window_handle"]
            viewer.click(x, y)
            viewer.send({"type": "text", "text": "desktop tiles"})
            typed = collect(1.5)
            buffer = ctypes.create_unicode_buffer(256)
            for _ in range(30):
                user32.SendMessageW(ready["edit_handle"], 0x000D, 256, buffer)
                if "desktop tiles" in buffer.value:
                    break
                time.sleep(0.1)
            canvas = None
            for frame in frames + idle + typed:
                canvas = apply(canvas, frame)
            # Other windows keep changing, so compare with a whole keyframe of
            # what the server holds, asked for now, after the patches before it.
            viewer.send({"type": "keyframe"})
            # A debug build takes a while to scale a whole 4K screen.
            later = [f for f in collect(6, lambda s, f: any(x["codec"] == 6 and x["keyframe"] for x in f))
                     if f["codec"] == 6]
            index = next(i for i, f in enumerate(later) if f["keyframe"])
            for frame in later[:index]:
                canvas = apply(canvas, frame)
            reference = apply(None, later[index])
            viewer.close()
            # Other windows may animate meanwhile; count what lies near the probe.
            scale_x, scale_y = key["width"] / width, key["height"] / height
            window = ((rect[0] - left) * scale_x, (rect[1] - top) * scale_y,
                      (rect[2] - left) * scale_x, (rect[3] - top) * scale_y)
            typed_patches = [p for f in typed for p in patches(f)]
            near_edit = sum(len(p.get("data", b"")) for p in typed_patches if near(p["rect"], window))
            shaped = [c for c in cursors if c.get("shape")]
            shape_ok = False
            if shaped:
                picture = Image.open(io.BytesIO(base64.b64decode(shaped[-1]["shape"]["png"])))
                shape_ok = picture.size == (shaped[-1]["shape"]["width"], shaped[-1]["shape"]["height"])
            at_target = [c for c in cursors if abs(c["x"] - x) < 0.01 and abs(c["y"] - y) < 0.01]
            result = {
                "capture": announce["source"].get("capture"), "fallback": announce["source"].get("fallback"),
                "size": [key["width"], key["height"]], "keyframe_bytes": len(key["data"]),
                "idle_frames": len(idle), "idle_bytes": sum(len(f["data"]) for f in idle),
                "typing_frames": len(typed), "typing_bytes": sum(len(f["data"]) for f in typed),
                "typing_bytes_near_edit": near_edit,
                "patched_vs_keyframe_difference": round(difference(canvas, reference), 3),
                "worst_tile_difference": round(worst_tile(canvas, reference), 1),
                "cursor_messages": len(cursors), "cursor_shape_decodes": shape_ok,
                "cursor_reached_click": bool(at_target), "probe_in_front": in_front, "edit_text": buffer.value,
            }
        finally:
            assist.terminate()
    finally:
        probe.terminate()
    result["passed"] = (result["capture"] == "dxgi" and 0 < result["typing_bytes_near_edit"] * 4 < result["keyframe_bytes"]
                        and result["worst_tile_difference"] < 24 and result["cursor_shape_decodes"]
                        and result["cursor_reached_click"] and "desktop tiles" in result["edit_text"])
    return result


def main():
    probe = json.loads(subprocess.run([AAD, "--compact", "assist", "--probe-video"], capture_output=True,
                                      text=True, encoding="utf-8").stdout)
    with browser_page() as (port, page_id):
        findings = {"tiles": tiles_check(port, page_id)}
        if "h264" in probe.get("encoders", {}):
            findings["adaptive"] = adaptive_check(port, page_id)
            findings["viewer_page"] = viewer_tiles_check(port, page_id)
    if "--desktop" in sys.argv:
        findings["desktop"] = desktop_check()
    print(json.dumps(findings, ensure_ascii=False, indent=1))
    return 0 if all(part["passed"] for part in findings.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
