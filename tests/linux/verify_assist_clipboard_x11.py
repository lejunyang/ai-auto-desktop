#!/usr/bin/env python3
"""Check clipboard transfer through `aad assist --desktop` on X11, by effect.

Runs a private Xvfb with openbox (the user's desktop is not touched). Needs
Xvfb, openbox, xclip, xdotool and a C compiler with libX11 headers.

- Paste: a small Xlib window (x11_clipboard_target.c), focused, asks the
  CLIPBOARD for every format on Ctrl+V: text, HTML, RTF, a PNG and files
  (x-special/gnome-copied-files, text/uri-list) arrive as sent; afterwards
  the clipboard holds what xclip put there before.
- Copy: text, HTML, a picture and files put on the clipboard with xclip are
  listed, and downloaded over HTTP with the same bytes.

AAD overrides the aad binary (default target/debug/aad).
"""
import base64
import hashlib
import json
import os
import queue
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(REPO, "tests", "windows"))
import verify_assist  # noqa: E402
from verify_assist import Viewer  # noqa: E402

AAD = os.environ.get("AAD", os.path.join(REPO, "target", "debug", "aad"))
DISPLAY = ":78"


def png(width, height):
    rows = b"".join(b"\0" + b"".join(bytes([x * 9 % 256, y * 7 % 256, 99, 255]) for x in range(width))
                    for y in range(height))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


class Client(Viewer):
    def __init__(self, url):
        super().__init__(url)
        self.sock.settimeout(None)
        self.messages = queue.Queue()
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        try:
            while True:
                m = self.receive()
                if m and m["type"] != "frame":
                    self.messages.put(m)
        except (EOFError, OSError):
            pass

    def raw(self, opcode, payload):
        mask = os.urandom(4)
        n = len(payload)
        head = bytes([0x80 | opcode]) + (bytes([0x80 | n]) if n < 126 else
                                         bytes([0x80 | 126]) + struct.pack(">H", n) if n < 65536 else
                                         bytes([0x80 | 127]) + struct.pack(">Q", n))
        body = (int.from_bytes(payload, "little") ^ int.from_bytes(mask * (n // 4 + 1), "little")
                ).to_bytes(n + 4, "little")[:n] if n else b""
        self.sock.sendall(head + mask + body)

    def send(self, message):
        self.raw(1, json.dumps(message).encode())

    def wait(self, predicate, timeout=20):
        until = time.time() + timeout
        while time.time() < until:
            try:
                m = self.messages.get(timeout=0.2)
            except queue.Empty:
                continue
            if predicate(m):
                return m
        raise TimeoutError

    def paste(self, formats, files=()):
        pid = os.urandom(8).hex()
        self.send({"type": "paste", "id": pid, "formats": formats,
                   "files": [{"name": n, "type": t, "size": len(b)} for n, t, b in files]})
        name = pid.encode()
        for index, (_, _, data) in enumerate(files):
            for at in range(0, len(data), 256 * 1024):
                self.raw(2, b"AADP\x01\0\0\0" + struct.pack(">H", len(name)) + name
                         + struct.pack(">HQ", index, at) + data[at:at + 256 * 1024])
        while True:
            m = self.wait(lambda m: m["type"] == "paste" and m.get("id") == pid)
            if m.get("state") in ("pasted", "failed", "cancelled"):
                return m


def run(*command, **kwargs):
    return subprocess.run(list(command), capture_output=True, text=True, **kwargs)


def main():
    env = dict(os.environ, DISPLAY=DISPLAY)
    os.environ["DISPLAY"] = DISPLAY
    work = tempfile.mkdtemp(prefix="aad-x11-clip-")
    target_bin = os.path.join(work, "target")
    built = run("cc", "-o", target_bin, os.path.join(HERE, "x11_clipboard_target.c"), "-lX11")
    if built.returncode:
        raise SystemExit(built.stderr)
    xvfb = subprocess.Popen(["Xvfb", DISPLAY, "-screen", "0", "1024x768x24", "-nolisten", "tcp"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)
    wm = subprocess.Popen(["openbox"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)
    report = {}
    keeper = None
    try:
        # What the person copied before: xclip keeps owning it.
        keeper = subprocess.Popen(["xclip", "-selection", "clipboard", "-i", "-loops", "0"], stdin=subprocess.PIPE,
                                  env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        keeper.stdin.write(b"ORIGINAL x11")
        keeper.stdin.close()
        time.sleep(0.5)
        target = subprocess.Popen([target_bin, "AAD target"], env=env, stdout=subprocess.PIPE, text=True)
        window = json.loads(target.stdout.readline())["window"]
        time.sleep(0.5)
        run("xdotool", "windowactivate", "--sync", str(window), env=env)
        run("xdotool", "windowfocus", "--sync", str(window), env=env)
        time.sleep(0.3)
        assist = subprocess.Popen([AAD, "--compact", "assist", "--desktop", "--codecs", "webp"], env=env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        announce = json.loads(assist.stdout.readline())
        lines = queue.Queue()
        threading.Thread(target=lambda: [lines.put(json.loads(l)) for l in target.stdout], daemon=True).start()
        try:
            client = Client(announce["websocket"])
            ready = client.wait(lambda m: m["type"] == "ready")
            report["ready_clipboard"] = ready.get("clipboard")
            picture = png(16, 8)
            answer = client.paste({"text/plain": "第一行\tTab\nline 2", "text/html": "<b>粗体</b>",
                                   "text/rtf": r"{\rtf1 {\b B}}"}, [("shot.png", "image/png", picture)])
            seen = lines.get(timeout=10)
            report["paste"] = {"answer": answer.get("state"), "targets": seen["targets"],
                               "text": seen["text"], "html": seen["html"], "rtf": seen["rtf"],
                               "png": seen["png"], "files": seen["files"]}
            report["paste"]["passed"] = (seen["text"] == "第一行\tTab\nline 2" and seen["html"] == "<b>粗体</b>"
                                         and seen["rtf"] == r"{\rtf1 {\b B}}" and seen["png"] == len(picture)
                                         and (seen["files"] or "").startswith("copy\nfile:///"))
            time.sleep(1.0)
            restored = run("xclip", "-selection", "clipboard", "-o", env=env, timeout=5).stdout
            report["restored"] = restored == "ORIGINAL x11"

            # Copy: what xclip offers comes back.
            def offer(kind, data):
                p = subprocess.Popen(["xclip", "-selection", "clipboard", "-t", kind, "-i"], stdin=subprocess.PIPE,
                                     env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                p.stdin.write(data)
                p.stdin.close()
                time.sleep(0.5)

            offer("text/html", "<i>远程</i>".encode())
            client.send({"type": "clipboard"})
            listing = client.wait(lambda m: m["type"] == "clipboard")
            report["copy_html"] = {"html": listing.get("html"), "passed": listing.get("html") == "<i>远程</i>"}
            offer("image/png", picture)
            client.send({"type": "clipboard"})
            listing = client.wait(lambda m: m["type"] == "clipboard")
            base = announce["url"].split("?")[0].rstrip("/")
            image = listing.get("image") or {}
            body = urllib.request.urlopen(f"{base}/clipboard/{image.get('id')}?token={announce['token']}").read() \
                if image.get("id") else b""
            report["copy_image"] = {"image": image, "passed": body == picture}
            path = os.path.join(work, "报告.txt")
            with open(path, "wb") as f:
                f.write(b"x11 file")
            offer("x-special/gnome-copied-files", ("copy\nfile://" + urllib.request.pathname2url(path)).encode())
            client.send({"type": "clipboard"})
            listing = client.wait(lambda m: m["type"] == "clipboard")
            files = listing.get("files") or []
            body = urllib.request.urlopen(f"{base}/clipboard/{files[0]['id']}?token={announce['token']}").read() \
                if files and files[0].get("id") else b""
            report["copy_files"] = {"files": [f["name"] for f in files], "passed": body == b"x11 file"}
            client.close()
        finally:
            assist.terminate()
            target.terminate()
    finally:
        if keeper:
            keeper.terminate()
        run("pkill", "-f", "xclip -selection clipboard")
        wm.terminate()
        xvfb.terminate()
    print(json.dumps(report, ensure_ascii=False, indent=1))
    sys.exit(0 if all(v.get("passed", True) for v in report.values() if isinstance(v, dict))
             and report.get("restored") else 1)


if __name__ == "__main__":
    main()
