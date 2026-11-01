#!/usr/bin/env python3
"""Check what a person can do through `aad assist`, by its effect on a page.

Headless Chrome, nothing on the real desktop is touched:
- a double click selects a word (the click count reaches the page);
- Ctrl+A then typing replaces a field's text; Ctrl+/ and Ctrl+Space reach the
  page as those keys, named by the physical key;
- a viewer that leaves holding Shift and the mouse button lets go of both,
  and a `release` message lets go of a held Ctrl;
- the built-in viewer page, driven like a person would: an input method
  composing Chinese sends only the finished text, and a button released
  outside the picture is still released on the page (pointer capture).

With --desktop the desktop source's clipboard answer is compared with the
clipboard read independently here (the clipboard is not changed), and keys
are pressed into an isolated probe window: Home, Shift+End, Delete and the
arrows edit its text as they should, arrive with their scan codes and the
extended-key flag, and Shift held by a viewer that leaves is let go. This
moves the real pointer and keyboard, so run it on its own.
"""
import ctypes
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from verify_assist import CHROME, Viewer, start_assist  # noqa: E402
from verify_assist_video import cdp, frames_until, listen, reported  # noqa: E402

PAGE = b"""<!doctype html><title>Assist input fixture</title>
<style>body{margin:0;font:20px sans-serif} #p{position:absolute;left:40px;top:40px}
#t{position:absolute;left:40px;top:120px;width:300px;height:36px;font-size:20px}</style>
<p id="p">alpha bravo charlie</p><input id="t" value="old text">
<script>
window.log=[];
const note=e=>log.push({type:e.type,key:e.key,code:e.code,ctrl:e.ctrlKey,shift:e.shiftKey,detail:e.detail,button:e.button});
for(const t of ['keydown','keyup']) addEventListener(t,note,true);
for(const t of ['mousedown','mouseup','dblclick']) addEventListener(t,note,true);
</script>"""


def serve_page():
    import http.server
    import threading

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.end_headers()
            self.wfile.write(PAGE)

        def log_message(self, *args):
            pass

        def handle(self):
            try:
                super().handle()
            except ConnectionError:
                pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def chrome(url, size="800,600"):
    profile = tempfile.mkdtemp(prefix="aad-assist-input-profile-")
    process = subprocess.Popen([CHROME, "--headless=new", "--remote-debugging-port=0", f"--user-data-dir={profile}",
                                "--no-first-run", f"--window-size={size}", url],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    active = os.path.join(profile, "DevToolsActivePort")
    port = None
    for _ in range(100):
        try:
            port = open(active).read().split()[0]
            break
        except (OSError, IndexError):
            # Not written yet, or being written.
            time.sleep(0.1)
    time.sleep(1.0)
    targets = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list").read())
    return process, port, next(t["id"] for t in targets if t["type"] == "page")


def evaluate(call, expression):
    return call("Runtime.evaluate", {"expression": expression, "returnByValue": True})["result"].get("value")


def mouse(viewer, action, x, y, count=1):
    viewer.send({"type": "mouse", "action": action, "button": "left", "x": x, "y": y, "count": count})
    time.sleep(0.05)


def key(viewer, action, key_name, code):
    viewer.send({"type": "key", "action": action, "key": key_name, "code": code})
    time.sleep(0.05)


@reported
def protocol_check(port, page_id):
    client, call = cdp(port, page_id)
    assist, announce = start_assist("--browser-port", port, "--page", page_id, "--codecs", "tiles")
    try:
        viewer = listen(Viewer(announce["websocket"]))
        viewer.send({"type": "hello", "accept": ["tiles"]})
        _, frames = frames_until(viewer, 8, lambda s, f: bool(f))
        width, height = frames[0]["width"], frames[0]["height"]
        word = evaluate(call, "(()=>{const r=document.createRange();const t=document.getElementById('p').firstChild;"
                              "r.setStart(t,6);r.setEnd(t,11);const b=r.getBoundingClientRect();return [b.x+b.width/2,b.y+b.height/2]})()")
        x, y = word[0] / width, word[1] / height
        for count in (1, 2):
            mouse(viewer, "down", x, y, count)
            mouse(viewer, "up", x, y, count)
        time.sleep(0.3)
        selected = evaluate(call, "getSelection().toString()")
        # Ctrl+A in the field, then typing, replaces its text.
        field = (190 / width, 138 / height)
        mouse(viewer, "down", *field)
        mouse(viewer, "up", *field)
        key(viewer, "down", "Control", "ControlLeft")
        key(viewer, "down", "a", "KeyA")
        key(viewer, "up", "a", "KeyA")
        key(viewer, "up", "Control", "ControlLeft")
        viewer.send({"type": "text", "text": "new"})
        time.sleep(0.3)
        replaced = evaluate(call, "document.getElementById('t').value")
        evaluate(call, "log.length=0")
        for name, code in (("/", "Slash"), (" ", "Space")):
            key(viewer, "down", "Control", "ControlLeft")
            key(viewer, "down", name, code)
            key(viewer, "up", name, code)
            key(viewer, "up", "Control", "ControlLeft")
        time.sleep(0.3)
        shortcuts = evaluate(call, "log")
        shortcut = lambda code: any(e["type"] == "keydown" and e["code"] == code and e["ctrl"] for e in shortcuts)
        # A release message lets go of a held Ctrl.
        evaluate(call, "log.length=0")
        key(viewer, "down", "Control", "ControlLeft")
        viewer.send({"type": "release"})
        time.sleep(0.3)
        released = any(e["type"] == "keyup" and e["code"] == "ControlLeft" for e in evaluate(call, "log"))
        # Leaving while holding Shift and the button lets go of both.
        evaluate(call, "log.length=0")
        key(viewer, "down", "Shift", "ShiftLeft")
        mouse(viewer, "down", x, y)
        viewer.close()
        time.sleep(1.0)
        log = evaluate(call, "log")
        result = {
            # Windows selects the word with the space after it.
            "double_click_selects_word": selected.strip() == "bravo",
            "selection": selected,
            "ctrl_a_then_typing_replaces": replaced == "new", "field": replaced,
            "ctrl_slash_reaches_page": shortcut("Slash"), "ctrl_space_reaches_page": shortcut("Space"),
            "release_lets_go_of_ctrl": released,
            "leaving_releases_shift": any(e["type"] == "keyup" and e["code"] == "ShiftLeft" for e in log),
            "leaving_releases_button": any(e["type"] == "mouseup" for e in log),
        }
        result["passed"] = all(v for k, v in result.items() if k not in ("selection", "field"))
        return result
    finally:
        client.close()
        assist.terminate()
        assist.wait()


@reported
def viewer_check(port, page_id):
    """The built-in viewer, in a Chrome of its own, driven with an input method
    and a release outside the picture."""
    client, call = cdp(port, page_id)
    call("Page.reload")
    time.sleep(1.0)
    assist, announce = start_assist("--browser-port", port, "--page", page_id, "--codecs", "tiles")
    second = None
    try:
        second, viewer_port, viewer_id = chrome(announce["url"], "1000,800")
        viewer, act = cdp(viewer_port, viewer_id)
        for _ in range(40):
            time.sleep(0.25)
            if evaluate(act, "document.getElementById('screen').width") > 100:
                break
        box = evaluate(act, "(()=>{const r=document.getElementById('screen').getBoundingClientRect();"
                            "return [r.x,r.y,r.width,r.height,document.getElementById('screen').width]})()")
        scale = box[2] / box[4]
        to_viewer = lambda px, py: (box[0] + px * scale, box[1] + py * scale)
        # Click the field through the viewer, then compose Chinese with an input method.
        fx, fy = to_viewer(190, 138)
        for kind in ("mousePressed", "mouseReleased"):
            act("Input.dispatchMouseEvent", {"type": kind, "x": fx, "y": fy, "button": "left", "clickCount": 1})
        time.sleep(0.3)
        evaluate(call, "document.getElementById('t').value=''")
        for partial in ("z", "zh", "zhong"):
            act("Input.imeSetComposition", {"text": partial, "selectionStart": len(partial), "selectionEnd": len(partial)})
            time.sleep(0.05)
        act("Input.insertText", {"text": "中文"})
        time.sleep(0.6)
        composed = evaluate(call, "document.getElementById('t').value")
        # Press inside the picture, release outside it.
        evaluate(call, "log.length=0")
        px, py = to_viewer(100, 300)
        act("Input.dispatchMouseEvent", {"type": "mousePressed", "x": px, "y": py, "button": "left", "clickCount": 1})
        act("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": 5, "y": 5, "button": "left", "buttons": 1})
        act("Input.dispatchMouseEvent", {"type": "mouseReleased", "x": 5, "y": 5, "button": "left", "clickCount": 1})
        time.sleep(0.6)
        log = evaluate(call, "log")
        viewer.close()
        result = {"input_method_text": composed, "composed_text_arrives_once": composed == "中文",
                  "release_outside_arrives": any(e["type"] == "mouseup" for e in log)}
        result["passed"] = result["composed_text_arrives_once"] and result["release_outside_arrives"]
        return result
    finally:
        if second:
            second.terminate()
        client.close()
        assist.terminate()
        assist.wait()


def clipboard_here():
    user32, kernel32 = ctypes.windll.user32, ctypes.windll.kernel32
    kernel32.GlobalLock.restype = ctypes.c_void_p
    user32.GetClipboardData.restype = ctypes.c_void_p
    for _ in range(20):
        if user32.OpenClipboard(None):
            break
        time.sleep(0.05)
    try:
        handle = user32.GetClipboardData(13)  # CF_UNICODETEXT
        if not handle:
            return None
        pointer = kernel32.GlobalLock(ctypes.c_void_p(handle))
        try:
            return ctypes.wstring_at(pointer)
        finally:
            kernel32.GlobalUnlock(ctypes.c_void_p(handle))
    finally:
        user32.CloseClipboard()


@reported
def clipboard_check():
    assist, announce = start_assist("--desktop", "--codecs", "tiles", "--hide-cursor")
    try:
        viewer = listen(Viewer(announce["websocket"]))
        viewer.send({"type": "clipboard"})
        answer = None
        deadline = time.time() + 5
        original = viewer.inbox.get
        while time.time() < deadline and answer is None:
            message = original(timeout=5)
            if message and message.get("type") in ("clipboard", "error"):
                answer = message
        viewer.close()
        here = clipboard_here()
        same = answer.get("type") == "clipboard" and answer.get("text") == (here[:100_000] if here else None)
        protected = answer.get("type") == "error" and "CLIPBOARD_PROTECTED" in answer.get("message", "")
        return {"answer_type": answer.get("type"), "characters": answer.get("characters"),
                "matches_independent_read": same, "protected": protected, "passed": same or protected}
    finally:
        assist.terminate()
        assist.wait()


@reported
def desktop_keys_check():
    from verify_assist import REPO
    user32 = ctypes.windll.user32
    log_path = os.path.join(tempfile.gettempdir(), f"aad-assist-keys-{os.getpid()}.jsonl")
    probe = subprocess.Popen([sys.executable, os.path.join(REPO, "tests", "windows", "input_channel_probe.py"),
                              "--title", f"AAD Assist Keys Probe {os.getpid()}", "--log", log_path],
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
        assist, announce = start_assist("--desktop", "--codecs", "tiles", "--hide-cursor")
        buffer = ctypes.create_unicode_buffer(256)

        def text():
            time.sleep(0.3)
            user32.SendMessageW(ready["edit_handle"], 0x000D, 256, buffer)
            return buffer.value

        try:
            viewer = listen(Viewer(announce["websocket"]))
            frames_until(viewer, 5, lambda s, f: bool(f))
            mouse(viewer, "down", x, y)
            mouse(viewer, "up", x, y)
            viewer.send({"type": "text", "text": "hello world"})
            typed = text()
            key(viewer, "down", "Home", "Home")
            key(viewer, "up", "Home", "Home")
            viewer.send({"type": "text", "text": ">"})
            home = text()
            key(viewer, "down", "Shift", "ShiftLeft")
            key(viewer, "down", "End", "End")
            key(viewer, "up", "End", "End")
            key(viewer, "up", "Shift", "ShiftLeft")
            key(viewer, "down", "Delete", "Delete")
            key(viewer, "up", "Delete", "Delete")
            selected_and_deleted = text()
            viewer.send({"type": "text", "text": "abc"})
            for _ in range(2):
                key(viewer, "down", "ArrowLeft", "ArrowLeft")
                key(viewer, "up", "ArrowLeft", "ArrowLeft")
            viewer.send({"type": "text", "text": "-"})
            arrows = text()
            # Leave while holding Shift.
            key(viewer, "down", "Shift", "ShiftLeft")
            viewer.close()
            time.sleep(1.0)
            shift_still_down = bool(user32.GetAsyncKeyState(0x10) & 0x8000)
        finally:
            assist.terminate()
            assist.wait()
        events = [json.loads(line) for line in open(log_path, encoding="utf-8") if line.strip()]
        raw = [e for e in events if e.get("channel") == "raw_input_keyboard"]
        arrow = [e for e in raw if e.get("vkey") == 0x25]
        result = {
            "typed": typed, "after_home": home, "after_shift_end_delete": selected_and_deleted, "after_arrows": arrows,
            "home_works": home == ">hello world", "shift_end_delete_works": selected_and_deleted == ">",
            "arrows_work": arrows == ">a-bc",
            "arrow_scan_code": arrow[0].get("make_code") if arrow else None,
            "arrow_extended_flag": bool(arrow) and bool(arrow[0].get("flags", 0) & 2),
            "shift_let_go_after_leaving": not shift_still_down,
        }
        result["passed"] = (result["home_works"] and result["shift_end_delete_works"] and result["arrows_work"]
                            and result["arrow_scan_code"] == 0x4B and result["arrow_extended_flag"]
                            and result["shift_let_go_after_leaving"])
        return result
    finally:
        probe.terminate()


def main():
    server = serve_page()
    process, port, page_id = chrome(f"http://127.0.0.1:{server.server_address[1]}/")
    try:
        findings = {"protocol": protocol_check(port, page_id), "viewer": viewer_check(port, page_id)}
    finally:
        process.terminate()
        server.shutdown()
    if "--desktop" in sys.argv:
        findings["clipboard"] = clipboard_check()
        findings["desktop_keys"] = desktop_keys_check()
    print(json.dumps(findings, ensure_ascii=False, indent=1))
    return 0 if all(part["passed"] for part in findings.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
