#!/usr/bin/env python3
"""Check `aad assist` end to end: frames reach a viewer, the viewer's input lands.

Browser: a headless Chrome with a temporary profile shows a fixture page. A
minimal WebSocket client (standing in for omem's page) receives frames, clicks
a button twice, clicks a text box and types. The page is then read back
independently with `aad describe` over the same CDP port.

Desktop (Windows, with --desktop): an isolated Win32 probe window is created; the
client clicks its edit box through the desktop stream and types, and the text is
read back with WM_GETTEXT. This moves the real pointer, so run it on its own.

AAD_TEST_CHROME overrides the Chrome executable.
"""
import base64
import ctypes
import hashlib
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import http.server
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
AAD = os.path.join(REPO, "target", "debug", "aad.exe" if os.name == "nt" else "aad")
CHROME = os.environ.get("AAD_TEST_CHROME", r"C:\Program Files\Google\Chrome\Application\chrome.exe")

PAGE = b"""<!doctype html><title>Assist fixture</title>
<style>body{margin:0} #b{position:absolute;left:40px;top:40px;width:200px;height:60px}
#t{position:absolute;left:40px;top:160px;width:300px;height:40px}</style>
<button id="b" onclick="this.textContent='pressed '+(++window.n)">press me</button>
<input id="t" aria-label="note"><script>window.n=0</script>"""


class Viewer:
    """Just enough of a WebSocket client for this protocol."""

    def __init__(self, url):
        host, rest = url[len("ws://"):].split("/", 1)
        address, port = host.split(":")
        self.sock = socket.create_connection((address, int(port)), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(f"GET /{rest} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                          f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += self.sock.recv(1)
        expected = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        assert b"101" in head.split(b"\r\n")[0] and expected.encode() in head, head

    def exact(self, n):
        data = b""
        while len(data) < n:
            chunk = self.sock.recv(n - len(data))
            if not chunk:
                raise EOFError
            data += chunk
        return data

    def receive(self):
        first, second = self.exact(2)
        length = second & 0x7F
        if length == 126:
            length = struct.unpack(">H", self.exact(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", self.exact(8))[0]
        payload = self.exact(length)
        return json.loads(payload) if first & 0x0F == 1 else None

    def send(self, message):
        payload = json.dumps(message).encode()
        mask = os.urandom(4)
        head = bytes([0x81]) + (bytes([0x80 | len(payload)]) if len(payload) < 126
                                else bytes([0x80 | 126]) + struct.pack(">H", len(payload)))
        self.sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def frame(self):
        while True:
            message = self.receive()
            if message and message["type"] == "frame":
                return message

    def click(self, x, y):
        for action in ("move", "down", "up"):
            self.send({"type": "mouse", "action": action, "button": "left", "x": x, "y": y})
            time.sleep(0.08)


def start_assist(*arguments):
    process = subprocess.Popen([AAD, "--compact", "assist", *arguments], stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, encoding="utf-8")
    announce = json.loads(process.stdout.readline())
    return process, announce


def forbidden_without_token(announce):
    url = announce["websocket"].replace(announce["token"], "wrong")
    try:
        Viewer(url)
        return False
    except AssertionError:
        return True


def browser_check():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), type("Site", (http.server.BaseHTTPRequestHandler,), {
        "do_GET": lambda self: (self.send_response(200), self.send_header("Content-Type", "text/html"),
                                self.end_headers(), self.wfile.write(PAGE)),
        "log_message": lambda self, *a: None}))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    profile = tempfile.mkdtemp(prefix="aad-assist-profile-")
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
        assist, announce = start_assist("--browser-port", port)
        try:
            viewer = Viewer(announce["websocket"])
            frame = viewer.frame()
            width, height = frame["width"], frame["height"]
            jpeg = base64.b64decode(frame["data"])[:3] == b"\xff\xd8\xff"
            viewer.click(140 / width, 70 / height)
            viewer.click(140 / width, 70 / height)
            viewer.click(190 / width, 180 / height)
            viewer.send({"type": "text", "text": "typed through assist"})
            time.sleep(0.5)
            env = dict(os.environ, AAD_CDP_PORT=port)
            apps = json.loads(subprocess.run([AAD, "--compact", "apps"], capture_output=True, text=True,
                                             encoding="utf-8", env=env).stdout)["apps"]
            page = next(a for a in apps if a["name"] == "Assist fixture")
            described = json.loads(subprocess.run([AAD, "--compact", "describe", page["resource_id"]],
                                                  capture_output=True, text=True, encoding="utf-8", env=env).stdout)
            text = json.dumps(described, ensure_ascii=False)
            result = {"frame_is_jpeg": jpeg, "frame_size": [width, height],
                      "button_pressed_twice": "pressed 2" in text,
                      "text_typed": "typed through assist" in text,
                      "wrong_token_refused": forbidden_without_token(announce)}
        finally:
            assist.terminate()
    finally:
        chrome.terminate()
        server.shutdown()
        time.sleep(1)
        shutil.rmtree(profile, ignore_errors=True)
    result["passed"] = all(v for k, v in result.items() if k != "frame_size")
    return result


def desktop_check():
    user32 = ctypes.windll.user32
    probe = subprocess.Popen([sys.executable, os.path.join(REPO, "tests", "windows", "input_channel_probe.py"),
                              "--title", f"AAD Assist Probe {os.getpid()}",
                              "--log", os.path.join(tempfile.gettempdir(), f"aad-assist-{os.getpid()}.jsonl")],
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
        assist, announce = start_assist("--desktop", "--fps", "4")
        try:
            viewer = Viewer(announce["websocket"])
            frame = viewer.frame()
            viewer.click(x, y)
            viewer.send({"type": "text", "text": "desktop assist"})
            time.sleep(0.5)
            buffer = ctypes.create_unicode_buffer(256)
            user32.SendMessageW(ready["edit_handle"], 0x000D, 256, buffer)
            result = {"frame_is_jpeg": base64.b64decode(frame["data"])[:3] == b"\xff\xd8\xff",
                      "frame_size": [frame["width"], frame["height"]],
                      "edit_text": buffer.value}
        finally:
            assist.terminate()
    finally:
        probe.terminate()
    result["passed"] = result["frame_is_jpeg"] and "desktop assist" in result["edit_text"]
    return result


def main():
    findings = {"browser": browser_check()}
    if "--desktop" in sys.argv:
        findings["desktop"] = desktop_check()
    print(json.dumps(findings, ensure_ascii=False, indent=1))
    return 0 if all(part["passed"] for part in findings.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
