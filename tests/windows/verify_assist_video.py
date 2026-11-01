#!/usr/bin/env python3
"""Check `aad assist` video end to end, with the ffmpeg this machine provides.

For each video codec `aad assist --probe-video` finds an encoder for, a headless
Chrome shows a fixture page and a minimal WebSocket client asks for that codec.
It checks, each in a way that can fail:
- the stream is announced, starts on a keyframe, and every frame received
  decodes with an independent ffmpeg into as many pictures as were sent;
- a still page sends nothing at all, and a small change sends a frame much
  smaller than the keyframe;
- clicks and typing through the video stream really change the page (read back
  with `aad describe`);
- the built-in viewer page, opened in the same Chrome, decodes the stream with
  WebCodecs and draws a picture.

With --desktop the Windows desktop is streamed as H.264 too: it decodes, and a
click and typing land in an isolated probe window (this moves the real pointer,
so run it on its own). AAD_TEST_CHROME overrides the Chrome executable.
"""
import contextlib
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time
import http.server
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from verify_assist import AAD, CHROME, PAGE, REPO, Viewer, start_assist  # noqa: E402

# The assist fixture with a screenful of text and colour below it, so a
# keyframe carries a realistic amount of picture and a small change is small
# beside it.
WORDS = "observe the element twice before acting on it and report what really changed".split()
PROSE = "".join(f"<p>{' '.join(WORDS[(i + j) % len(WORDS)] for j in range(18))}</p>" for i in range(14)).encode()
RICH = PAGE + (b"<div style='position:absolute;left:380px;top:20px;width:400px;height:200px;"
               b"background:linear-gradient(135deg,#c33,#3c3,#33c)'></div>"
               b"<div style='position:absolute;left:20px;top:230px;width:760px;font:13px Georgia,serif'>"
               + PROSE + b"</div>")

FFMPEG = os.environ.get("AAD_FFMPEG") or shutil.which("ffmpeg")
VIDEO = {3: "h264", 4: "vp9", 5: "av1"}


def listen(viewer):
    """Receives on a thread of its own, so waiting never cuts a frame in half."""
    import queue
    viewer.inbox = queue.Queue()

    def pump():
        while True:
            try:
                message = viewer.receive()
            except (EOFError, OSError):
                viewer.inbox.put(None)
                return
            if message:
                viewer.inbox.put(message)

    threading.Thread(target=pump, daemon=True).start()
    return viewer


def frames_until(viewer, seconds, stop=None):
    """Messages for a while: (stream announcements, frames)."""
    import queue
    streams, frames = [], []
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            message = viewer.inbox.get(timeout=0.1)
        except queue.Empty:
            message = {"type": "nothing"}
        if message is None:
            break
        if message["type"] == "frame":
            message["codec"] = VIDEO.get(message["codec"], message["codec"])
            frames.append(message)
        elif message["type"] == "stream":
            streams.append(message)
        elif message["type"] == "error":
            raise AssertionError(message["message"])
        if stop and stop(streams, frames):
            break
    return streams, frames


def decoded_count(codec, frames, width, height):
    """How many pictures an independent ffmpeg decodes from these frames."""
    folder = tempfile.mkdtemp(prefix="aad-assist-video-")
    path = os.path.join(folder, "stream." + ("h264" if codec == "h264" else "ivf"))
    with open(path, "wb") as out:
        if codec != "h264":
            fourcc = b"VP90" if codec == "vp9" else b"AV01"
            out.write(b"DKIF" + struct.pack("<HH4sHHIIII", 0, 32, fourcc, width, height, 1000, 1, len(frames), 0))
        for index, frame in enumerate(frames):
            if codec != "h264":
                out.write(struct.pack("<IQ", len(frame["data"]), index))
            out.write(frame["data"])
    run = subprocess.run([FFMPEG, "-v", "error", "-i", path, "-f", "framecrc", "-"], capture_output=True, text=True)
    shutil.rmtree(folder, ignore_errors=True)
    lines = [line for line in run.stdout.splitlines() if line and not line.startswith("#")]
    return len(lines), run.stderr.strip()[:300]


def describe(port, title):
    env = dict(os.environ, AAD_CDP_PORT=port)
    apps = json.loads(subprocess.run([AAD, "--compact", "apps"], capture_output=True, text=True,
                                     encoding="utf-8", env=env).stdout)["apps"]
    page = next(a for a in apps if a["name"] == title)
    described = subprocess.run([AAD, "--compact", "describe", page["resource_id"]], capture_output=True,
                               text=True, encoding="utf-8", env=env).stdout
    return described


def cdp(port, target_id):
    """A Viewer speaking CDP to one target, with a call helper."""
    client = Viewer(f"ws://127.0.0.1:{port}/devtools/page/{target_id}")
    counter = [0]

    def call(method, params=None):
        counter[0] += 1
        client.send({"id": counter[0], "method": method, "params": params or {}})
        while True:
            message = client.receive()
            if message and message.get("id") == counter[0]:
                return message.get("result", {})

    return client, call


def reported(check):
    """A check whose failure carries what aad assist wrote to stderr."""
    def run(*args):
        started = []
        original = globals()["start_assist"]

        def tracked(*a):
            process, announce = original(*a)
            started.append(process)
            return process, announce

        globals()["start_assist"] = tracked
        try:
            return check(*args)
        except Exception as error:  # noqa: BLE001 - reported, not hidden
            errors = " | ".join(p.stderr.read()[-1500:] for p in started if p.stderr)
            import traceback
            return {"passed": False, "error": repr(error), "where": traceback.format_exc()[-600:], "stderr": errors}
        finally:
            globals()["start_assist"] = original
    return run


@reported
def codec_check(port, codec, page_id):
    # A fresh page for each codec: nothing focused (a blinking caret is a real
    # change) and the click count back at zero.
    client, call = cdp(port, page_id)
    call("Page.reload")
    client.close()
    time.sleep(1.5)
    assist, announce = start_assist("--browser-port", port, "--page", page_id, "--codecs", f"{codec},webp")
    try:
        viewer = listen(Viewer(announce["websocket"]))
        viewer.send({"type": "hello", "accept": [codec]})
        streams, frames = frames_until(viewer, 8, lambda s, f: any(x["codec"] == codec for x in f))
        frames = [f for f in frames if f["codec"] == codec]
        assert streams and frames, f"no {codec} stream"
        stream = streams[-1]
        # Let the first picture settle, then measure a still page.
        _, settling = frames_until(viewer, 1.5)
        frames += [f for f in settling if f["codec"] == codec]
        # Longer than any read timeout: a viewer that only watches stays connected.
        _, still = frames_until(viewer, 6.0)
        width, height = frames[0]["width"], frames[0]["height"]
        viewer.click(140 / width, 70 / height)
        _, after_click = frames_until(viewer, 1.0)
        viewer.click(140 / width, 70 / height)
        viewer.click(190 / width, 180 / height)
        viewer.send({"type": "text", "text": f"typed as {codec}"})
        _, after_typing = frames_until(viewer, 1.5)
        everything = frames + still + after_click + after_typing
        decoded, error = decoded_count(codec, everything, width, height)
        key_bytes = len(frames[0]["data"])
        deltas = [len(f["data"]) for f in after_click + after_typing if not f["keyframe"]]
        viewer.close()
        text = describe(port, "Assist fixture")
        result = {
            "encoder": stream["encoder"], "webcodecs": stream["webcodecs"], "size": [width, height],
            "starts_on_keyframe": frames[0]["keyframe"],
            "keyframe_bytes": key_bytes,
            "still_page_frames": len(still), "still_page_bytes": sum(len(f["data"]) for f in still),
            "change_frames": len(deltas), "largest_change_bytes": max(deltas, default=0),
            "frames_sent": len(everything), "frames_decoded": decoded, "decode_errors": error,
            "button_pressed_twice": "pressed 2" in text or "pressed 3" in text,
            "text_typed": f"typed as {codec}" in text,
        }
        result["passed"] = (result["starts_on_keyframe"] and result["still_page_frames"] == 0
                            and result["change_frames"] > 0 and result["largest_change_bytes"] * 4 < key_bytes
                            and decoded == len(everything) and not error
                            and result["button_pressed_twice"] and result["text_typed"])
        return result
    finally:
        assist.terminate()
        assist.wait()


@reported
def viewer_page_check(port, page_id, *extra):
    """The built-in viewer, opened in Chrome, draws the video it decodes."""
    assist, announce = start_assist("--browser-port", port, "--page", page_id, *extra)
    try:
        request = urllib.request.Request(f"http://127.0.0.1:{port}/json/new?{announce['url']}", method="PUT")
        target = json.loads(urllib.request.urlopen(request).read())
        client, call = cdp(port, target["id"])
        state = None
        for _ in range(60):
            time.sleep(0.25)
            state = call("Runtime.evaluate", {"returnByValue": True, "expression": """(()=>{
              const c=document.getElementById('screen'), d=c.getContext('2d').getImageData(0,0,c.width,c.height).data;
              let lit=0; for(let i=0;i<d.length;i+=4) if(d[i]|d[i+1]|d[i+2]) lit++;
              return {status:document.getElementById('status').textContent, width:c.width, height:c.height, lit,
                      webcodecs:'VideoDecoder' in window}; })()"""})["result"]["value"]
            if any(name in state["status"] for name in VIDEO.values()) and state["lit"] > 1000:
                break
        client.close()
        urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}/json/close/{target['id']}"))
        state["passed"] = any(f" {name} (" in state["status"] for name in VIDEO.values()) and state["lit"] > 1000
        return state
    finally:
        assist.terminate()
        assist.wait()


class Site(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(RICH)

    def log_message(self, *args):
        pass

    def handle(self):
        # Chrome drops idle keep-alive connections; that is not a failure.
        try:
            super().handle()
        except ConnectionError:
            pass


@contextlib.contextmanager
def browser_page():
    """A headless Chrome showing the rich fixture: (CDP port, page id)."""
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    profile = tempfile.mkdtemp(prefix="aad-assist-video-profile-")
    chrome = subprocess.Popen([CHROME, "--headless=new", "--remote-debugging-port=0", f"--user-data-dir={profile}",
                               "--no-first-run", "--window-size=800,600", f"http://127.0.0.1:{server.server_address[1]}/"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        active = os.path.join(profile, "DevToolsActivePort")
        for _ in range(100):
            if os.path.exists(active) and open(active).read().strip():
                break
            time.sleep(0.1)
        port = open(active).read().split()[0]
        time.sleep(1.0)
        targets = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list").read())
        yield port, next(t["id"] for t in targets if t["type"] == "page")
    finally:
        chrome.terminate()
        server.shutdown()
        time.sleep(1)
        shutil.rmtree(profile, ignore_errors=True)


def browser_checks(codecs):
    with browser_page() as (port, page_id):
        results = {codec: codec_check(port, codec, page_id) for codec in codecs}
        # Video only: the viewer would otherwise start on tiles.
        results["viewer_page"] = viewer_page_check(port, page_id, "--codecs", "h264,vp9,av1,webp")
        return results


@reported
def desktop_check():
    import ctypes
    user32 = ctypes.windll.user32
    probe = subprocess.Popen([sys.executable, os.path.join(REPO, "tests", "windows", "input_channel_probe.py"),
                              "--title", f"AAD Assist Video Probe {os.getpid()}",
                              "--log", os.path.join(tempfile.gettempdir(), f"aad-assist-video-{os.getpid()}.jsonl")],
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
        assist, announce = start_assist("--desktop", "--codecs", "h264")
        try:
            viewer = listen(Viewer(announce["websocket"]))
            streams, frames = frames_until(viewer, 8, lambda s, f: len(f) > 0)
            viewer.click(x, y)
            viewer.send({"type": "text", "text": "desktop video"})
            buffer = ctypes.create_unicode_buffer(256)
            for _ in range(50):
                time.sleep(0.1)
                user32.SendMessageW(ready["edit_handle"], 0x000D, 256, buffer)
                if "desktop video" in buffer.value:
                    break
            _, more = frames_until(viewer, 1.0)
            frames += more
            decoded, error = decoded_count("h264", frames, frames[0]["width"], frames[0]["height"])
            result = {"encoder": streams[0]["encoder"] if streams else None,
                      "size": [frames[0]["width"], frames[0]["height"]],
                      "starts_on_keyframe": frames[0]["keyframe"], "frames_sent": len(frames),
                      "frames_decoded": decoded, "decode_errors": error, "edit_text": buffer.value}
        finally:
            assist.terminate()
    finally:
        probe.terminate()
    result["passed"] = (result["starts_on_keyframe"] and decoded == len(frames) and not error
                        and "desktop video" in result["edit_text"])
    return result


def main():
    if not FFMPEG:
        print(json.dumps({"skipped": "no ffmpeg: set AAD_FFMPEG or put ffmpeg on PATH"}))
        return 0
    probe = json.loads(subprocess.run([AAD, "--compact", "assist", "--probe-video"], capture_output=True,
                                      text=True, encoding="utf-8").stdout)
    codecs = [c for c in ("h264", "vp9", "av1") if c in probe["encoders"]]
    findings = {"probe": probe, "browser": browser_checks(codecs)}
    if "--desktop" in sys.argv:
        findings["desktop"] = desktop_check()
    print(json.dumps(findings, ensure_ascii=False, indent=1))
    parts = list(findings["browser"].values()) + ([findings["desktop"]] if "desktop" in findings else [])
    return 0 if codecs and all(part["passed"] for part in parts) else 1


if __name__ == "__main__":
    raise SystemExit(main())
