#!/usr/bin/env python3
"""Check several viewers on one `aad assist`, by their effect on a page.

Headless Chrome, the desktop is not touched:
- two viewers watch at once in different codecs (tiles and WebP);
- only the controlling viewer's clicks reach the page; a watcher's do not;
- a watcher that takes control gets it, and what the last controller held
  (Shift) is let go; when the controller leaves, control passes on;
- a viewer arriving while the page stays still still gets a first picture;
- one more viewer than --max-viewers is refused.
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from verify_assist import Viewer, start_assist  # noqa: E402
from verify_assist_input import chrome, evaluate, key, mouse, serve_page  # noqa: E402
from verify_assist_video import cdp, frames_until, listen, reported  # noqa: E402


def messages(viewer, seconds, kind):
    """Messages of one type that arrive within a time."""
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


def connect(announce, accept):
    viewer = listen(Viewer(announce["websocket"]))
    viewer.send({"type": "hello", "accept": accept})
    return viewer


def first_frame(viewer, codec):
    _, frames = frames_until(viewer, 8, lambda s, f: any(x["codec"] == codec for x in f))
    return next((f for f in frames if f["codec"] == codec), None)


@reported
def viewers_check(port, page_id):
    client, call = cdp(port, page_id)
    assist, announce = start_assist("--browser-port", port, "--page", page_id, "--codecs", "tiles,webp",
                                    "--max-viewers", "3")
    try:
        a = connect(announce, ["tiles"])
        a_frame = first_frame(a, 6)
        b = connect(announce, ["webp"])
        b_frame = first_frame(b, "webp")
        width, height = a_frame["width"], a_frame["height"]
        word = evaluate(call, "(()=>{const b=document.getElementById('p').getBoundingClientRect();return [b.x+20,b.y+b.height/2]})()")
        x, y = word[0] / width, word[1] / height
        evaluate(call, "log.length=0")
        mouse(b, "down", x, y)
        mouse(b, "up", x, y)
        time.sleep(0.4)
        watcher_clicks = [e for e in evaluate(call, "log") if e["type"] == "mousedown"]
        mouse(a, "down", x, y)
        mouse(a, "up", x, y)
        time.sleep(0.4)
        controller_clicks = [e for e in evaluate(call, "log") if e["type"] == "mousedown"]
        # The controller holds Shift; the watcher takes over.
        evaluate(call, "log.length=0")
        key(a, "down", "Shift", "ShiftLeft")
        time.sleep(0.2)
        b.send({"type": "take"})
        b_control = messages(b, 1.0, "control")
        a_control = messages(a, 0.5, "control")
        shift_let_go = any(e["type"] == "keyup" and e["code"] == "ShiftLeft" for e in evaluate(call, "log"))
        evaluate(call, "log.length=0")
        mouse(b, "down", x, y)
        mouse(b, "up", x, y)
        time.sleep(0.4)
        taken_clicks = [e for e in evaluate(call, "log") if e["type"] == "mousedown"]
        # The controller leaves: control passes back.
        b.close()
        handed_back = messages(a, 1.5, "control")
        # A third viewer while the page is still: it gets a picture anyway.
        time.sleep(1.0)
        c = connect(announce, ["tiles"])
        c_frame = first_frame(c, 6)
        d = connect(announce, ["tiles"])
        e = connect(announce, ["tiles"])
        refused = [m for m in messages(e, 2.0, "error") if "max-viewers" in m.get("message", "")]
        for viewer in (a, c, d, e):
            viewer.close()
        # Everyone gone, then a new viewer: still a first picture.
        time.sleep(1.0)
        f = connect(announce, ["tiles"])
        f_frame = first_frame(f, 6)
        f.close()
        result = {
            "both_watch_in_their_codecs": bool(a_frame) and bool(b_frame),
            "watcher_click_ignored": not watcher_clicks,
            "controller_click_lands": len(controller_clicks) == 1,
            "take_gives_control": any(m["you"] for m in b_control) and any(not m["you"] for m in a_control),
            "last_controller_shift_let_go": shift_let_go,
            "new_controller_click_lands": len(taken_clicks) == 1,
            "control_handed_back_when_controller_leaves": any(m["you"] for m in handed_back),
            "late_viewer_gets_picture_of_still_page": bool(c_frame) and c_frame["keyframe"],
            "viewer_over_limit_refused": bool(refused),
            "reconnect_after_all_left_gets_picture": bool(f_frame) and f_frame["keyframe"],
        }
        result["passed"] = all(result.values())
        return result
    finally:
        client.close()
        assist.terminate()
        assist.wait()


def main():
    server = serve_page()
    process, port, page_id = chrome(f"http://127.0.0.1:{server.server_address[1]}/")
    try:
        findings = {"viewers": viewers_check(port, page_id)}
    finally:
        process.terminate()
        server.shutdown()
    print(json.dumps(findings, ensure_ascii=False, indent=1))
    return 0 if all(part["passed"] for part in findings.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
