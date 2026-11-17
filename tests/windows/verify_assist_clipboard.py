#!/usr/bin/env python3
"""Check clipboard transfer through `aad assist --desktop` by its effect.

A minimal WebSocket client stands in for the viewer and pastes the way the
built-in viewer does (a `paste` message, then each file's bytes in `AADP`
frames). What lands is read back independently:

- Word (through COM, from a PowerShell 5.1 helper): plain text with tabs,
  line breaks and Chinese; HTML keeps its bold; RTF alone keeps its bold; a
  picture arrives as a picture.
- Explorer: files pasted into an open folder are copied there, byte for byte;
  a 100 MB file too, while frames keep coming (a ticking window keeps the
  screen changing) and input still gets through.
- The clipboard holds what it held before every paste, and never the pasted
  content.
- Copy back (`--copy`): text, HTML, a picture and files put on the clipboard
  here are listed in a `clipboard` message, and each is downloaded over HTTP
  with the right bytes; a folder comes as a zip.

This moves the real pointer and keyboard and opens Word and Explorer
windows: run it on its own, on a desktop nobody is using. Needs Word.
"""
import ctypes
import hashlib
import io
import json
import os
import queue
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import zipfile
from ctypes import wintypes

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from verify_assist import Viewer, start_assist  # noqa: E402

user32 = ctypes.windll.user32
kernel32 = ctypes.windll.kernel32
user32.GetClipboardData.restype = ctypes.c_void_p
user32.SetClipboardData.argtypes = [wintypes.UINT, ctypes.c_void_p]
user32.SetClipboardData.restype = ctypes.c_void_p
kernel32.GlobalAlloc.restype = ctypes.c_void_p
kernel32.GlobalAlloc.argtypes = [wintypes.UINT, ctypes.c_size_t]
kernel32.GlobalLock.restype = ctypes.c_void_p
kernel32.GlobalLock.argtypes = [ctypes.c_void_p]
kernel32.GlobalUnlock.argtypes = [ctypes.c_void_p]
kernel32.GlobalSize.restype = ctypes.c_size_t
kernel32.GlobalSize.argtypes = [ctypes.c_void_p]
user32.RegisterClipboardFormatW.restype = wintypes.UINT
CF_UNICODETEXT = 13
# Put in what is pasted, to look for in the clipboard history.
MARKER = f"aad-paste-{os.getpid()}-{int(time.time())}"
CF_HDROP = 15


def open_clipboard():
    for _ in range(100):
        if user32.OpenClipboard(None):
            return
        time.sleep(0.02)
    raise RuntimeError("clipboard busy")


def set_clipboard(formats):
    """formats: {format id: bytes}"""
    open_clipboard()
    try:
        user32.EmptyClipboard()
        for fmt, data in formats.items():
            handle = kernel32.GlobalAlloc(0x0002, len(data))
            pointer = kernel32.GlobalLock(handle)
            ctypes.memmove(pointer, data, len(data))
            kernel32.GlobalUnlock(handle)
            user32.SetClipboardData(fmt, handle)
    finally:
        user32.CloseClipboard()


def utf16z(text):
    return text.encode("utf-16-le") + b"\0\0"


def clipboard_text():
    open_clipboard()
    try:
        handle = user32.GetClipboardData(CF_UNICODETEXT)
        if not handle:
            return None
        pointer = kernel32.GlobalLock(handle)
        try:
            return ctypes.wstring_at(pointer)
        finally:
            kernel32.GlobalUnlock(handle)
    finally:
        user32.CloseClipboard()


def png(width, height, seed=0):
    """A small RGBA PNG, no libraries needed."""
    import zlib
    rows = b"".join(b"\0" + b"".join(bytes(((x * 7 + y * 3 + seed) % 256, (x * 5) % 256, (y * 9) % 256, 255))
                                           for x in range(width)) for y in range(height))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


class Client(Viewer):
    """The viewer side of the protocol, with uploads, on a reading thread."""

    def __init__(self, url):
        super().__init__(url)
        self.sock.settimeout(None)
        self.messages = queue.Queue()
        self.frames = []
        self.ready = None
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        try:
            while True:
                message = self.receive()
                if message is None:
                    continue
                if message["type"] == "frame":
                    self.frames.append(time.monotonic())
                else:
                    self.messages.put(message)
        except (EOFError, OSError):
            pass

    def raw(self, opcode, payload):
        mask = os.urandom(4)
        n = len(payload)
        if n < 126:
            head = bytes([0x80 | opcode, 0x80 | n])
        elif n < 65536:
            head = bytes([0x80 | opcode, 0x80 | 126]) + struct.pack(">H", n)
        else:
            head = bytes([0x80 | opcode, 0x80 | 127]) + struct.pack(">Q", n)
        masked = (int.from_bytes(payload, "little") ^ int.from_bytes(mask * (n // 4 + 1), "little")
                  ).to_bytes(n + 4, "little")[:n] if n else b""
        self.sock.sendall(head + mask + masked)

    def send(self, message):
        self.raw(1, json.dumps(message).encode())

    def wait(self, predicate, timeout=20):
        until = time.monotonic() + timeout
        skipped = []
        try:
            while time.monotonic() < until:
                try:
                    message = self.messages.get(timeout=max(0.01, until - time.monotonic()))
                except queue.Empty:
                    break
                if predicate(message):
                    return message
                skipped.append(message)
        finally:
            for message in skipped:
                if message["type"] not in ("cursor", "control", "mode", "stream"):
                    self.messages.put(message)
        raise TimeoutError("no matching message")

    def ready_message(self):
        if self.ready is None:
            self.ready = self.wait(lambda m: m["type"] == "ready")
        return self.ready

    def paste(self, formats=None, files=(), timeout=60):
        """files: [(name, mime, bytes)]. Returns the final paste message."""
        paste_id = os.urandom(8).hex()
        self.send({"type": "paste", "id": paste_id, "formats": formats or {},
                   "files": [{"name": n, "type": t, "size": len(b)} for n, t, b in files]})
        acked, sent, window = 0, 0, 8 * 1024 * 1024
        name = paste_id.encode()
        for index, (_, _, data) in enumerate(files):
            for at in range(0, len(data), 256 * 1024):
                while sent - acked >= window:
                    message = self.wait(lambda m: m["type"] == "paste" and m.get("id") == paste_id)
                    if "ack" in message:
                        acked = message["ack"]
                    elif message.get("state") == "failed":
                        return message
                piece = data[at:at + 256 * 1024]
                self.raw(2, b"AADP" + bytes([1, 0, 0, 0]) + struct.pack(">H", len(name)) + name
                         + struct.pack(">HQ", index, at) + piece)
                sent += len(piece)
        while True:
            message = self.wait(lambda m: m["type"] == "paste" and m.get("id") == paste_id, timeout)
            if message.get("state") in ("pasted", "failed", "cancelled"):
                return message


WORD_HELPER = r"""
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$w = New-Object -ComObject Word.Application
$w.Visible = $true
$d = $w.Documents.Add()
$w.Activate()
[Console]::Out.WriteLine((@{hwnd = $w.ActiveWindow.Hwnd} | ConvertTo-Json -Compress))
while ($null -ne ($line = [Console]::In.ReadLine())) {
  if ($line -eq 'clear') { $d.Content.Delete() | Out-Null; $r = @{ok = $true} }
  elseif ($line -eq 'read') {
    $c = $d.Content
    $types = @(); foreach ($s in $d.InlineShapes) { $types += $s.Type }
    $bold = @(); foreach ($i in 1..[Math]::Min(6, $d.Characters.Count)) { $bold += $d.Characters.Item($i).Bold }
    $r = @{text = $c.Text; shapes = $d.InlineShapes.Count; types = $types; floating = $d.Shapes.Count; bold = $bold}
  }
  elseif ($line -eq 'quit') { $d.Close(0); $w.Quit(); break }
  [Console]::Out.WriteLine(($r | ConvertTo-Json -Compress))
}
"""


class Word:
    def __init__(self):
        path = os.path.join(tempfile.gettempdir(), f"aad-word-helper-{os.getpid()}.ps1")
        with open(path, "w", encoding="utf-8-sig") as f:
            f.write(WORD_HELPER)
        self.process = subprocess.Popen(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", path],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8")
        self.hwnd = json.loads(self.process.stdout.readline())["hwnd"]
        self.dismiss_popups(8)

    def dismiss_popups(self, seconds):
        """Word may greet with an account or licence dialog that takes the focus."""
        process = wintypes.DWORD()
        user32.GetWindowThreadProcessId(self.hwnd, ctypes.byref(process))
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            found = []

            @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
            def each(hwnd, _):
                owner = wintypes.DWORD()
                user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
                name = ctypes.create_unicode_buffer(64)
                user32.GetClassNameW(hwnd, name, 64)
                if owner.value == process.value and name.value == "NUIDialog" and user32.IsWindowVisible(hwnd):
                    found.append(hwnd)
                return True
            user32.EnumWindows(each, 0)
            for hwnd in found:
                user32.PostMessageW(hwnd, 0x0010, 0, 0)
            if found:
                time.sleep(1)
                return
            time.sleep(0.3)

    def ask(self, command):
        self.process.stdin.write(command + "\n")
        self.process.stdin.flush()
        return json.loads(self.process.stdout.readline())

    def quit(self):
        try:
            self.process.stdin.write("quit\n")
            self.process.stdin.flush()
            self.process.wait(20)
        except Exception:
            self.process.kill()


def window_center(hwnd, fraction_y=0.5):
    rect = (ctypes.c_long * 4)()
    user32.GetWindowRect(hwnd, rect)
    left, top = user32.GetSystemMetrics(76), user32.GetSystemMetrics(77)
    width, height = user32.GetSystemMetrics(78), user32.GetSystemMetrics(79)
    x = ((rect[0] + rect[2]) / 2 - left) / width
    y = (rect[1] + (rect[3] - rect[1]) * fraction_y - top) / height
    return x, y


def focus(client, hwnd, fraction_y=0.5):
    """Brings a window forward the way a person would: a click through the stream."""
    user32.ShowWindow(hwnd, 9)
    # Windows lets a process bring a window forward right after a key press.
    user32.keybd_event(0x87, 0, 0, 0)
    user32.keybd_event(0x87, 0, 2, 0)
    user32.SetForegroundWindow(hwnd)
    user32.BringWindowToTop(hwnd)
    time.sleep(0.4)
    x, y = window_center(hwnd, fraction_y)
    for action in ("move", "down", "up"):
        client.send({"type": "mouse", "action": action, "button": "left", "x": x, "y": y})
        time.sleep(0.08)
    time.sleep(0.5)
    return user32.GetForegroundWindow() == hwnd or user32.GetAncestor(user32.GetForegroundWindow(), 3) == hwnd


def find_window(class_name, title, timeout=15):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        found = []

        @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
        def each(hwnd, _):
            name = ctypes.create_unicode_buffer(256)
            user32.GetClassNameW(hwnd, name, 256)
            text = ctypes.create_unicode_buffer(512)
            user32.GetWindowTextW(hwnd, text, 512)
            if name.value == class_name and title in text.value and user32.IsWindowVisible(hwnd):
                found.append(hwnd)
            return True
        user32.EnumWindows(each, 0)
        if found:
            return found[0]
        time.sleep(0.2)
    raise TimeoutError(f"no {class_name} window titled {title}")


TICKER = r"""
import tkinter, itertools
root = tkinter.Tk(); root.title("AAD ticker"); root.geometry("320x80+40+40"); root.attributes("-topmost", True)
label = tkinter.Label(root, font=("Consolas", 28)); label.pack(fill="both", expand=True)
counter = itertools.count()
def tick():
    label.config(text=str(next(counter))); root.after(50, tick)
tick(); root.mainloop()
"""


def paste_checks(client, original, report):
    word = Word()
    try:
        assert focus(client, word.hwnd, 0.45), "Word did not come forward"
        cases = [
            ("text", {"text/plain": f"第一行\tTab\r\nsecond line 😀 {MARKER}"}, (),
             lambda r: "第一行\tTab" in r["text"] and f"second line 😀 {MARKER}" in r["text"]),
            ("html", {"text/plain": "粗体 plain", "text/html": "<b>粗体</b> plain"}, (),
             lambda r: r["bold"][:2] == [-1, -1] and r["bold"][3] == 0),
            ("rtf", {"text/plain": "Bold plain", "text/rtf": r"{\rtf1\ansi {\b Bold} plain}"}, (),
             lambda r: r["bold"][:4] == [-1, -1, -1, -1] and r["bold"][5] == 0),
            ("image", {}, [("shot.png", "image/png", png(64, 48))],
             lambda r: r["shapes"] + r["floating"] >= 1),
        ]
        for name, formats, files, check in cases:
            word.ask("clear")
            focus(client, word.hwnd, 0.45)
            answer = client.paste(formats, files)
            time.sleep(1.0)
            read = word.ask("read")
            report[f"word_{name}"] = {"answer": answer, "read": read, "passed": answer.get("state") == "pasted" and check(read)}
            report[f"word_{name}"]["clipboard_kept"] = clipboard_text() == original
    finally:
        word.quit()

    folder = tempfile.mkdtemp(prefix="aad-paste-into-")
    title = os.path.basename(folder)
    explorer = subprocess.Popen(["explorer.exe", folder])
    hwnd = find_window("CabinetWClass", title)
    try:
        time.sleep(1.5)
        assert focus(client, hwnd, 0.7), "Explorer did not come forward"
        files = [("a.txt", "text/plain", "你好\n".encode()), ("b.bin", "", os.urandom(300_000)),
                 ("shot.png", "image/png", png(20, 10, 3))]
        answer = client.paste({}, files)
        landed = {}
        for _ in range(60):
            landed = {n: open(os.path.join(folder, n), "rb").read() for n in os.listdir(folder)}
            if len(landed) >= len(files):
                break
            time.sleep(0.25)
        report["explorer_files"] = {"answer": answer, "landed": sorted(landed),
                                    "passed": all(landed.get(n) == b for n, _, b in files),
                                    "clipboard_kept": clipboard_text() == original}

        # 100 MB while the screen keeps changing: frames keep coming.
        ticker = subprocess.Popen([sys.executable, "-c", TICKER])
        time.sleep(1.5)
        focus(client, hwnd, 0.7)
        big = os.urandom(100 * 1024 * 1024)
        before = len(client.frames)
        started = time.monotonic()
        stamps_from = len(client.frames)
        answer = client.paste({}, [("big.bin", "", big)], timeout=120)
        elapsed = time.monotonic() - started
        stamps = client.frames[stamps_from:]
        gaps = [b - a for a, b in zip([started] + stamps, stamps)]
        target = os.path.join(folder, "big.bin")
        for _ in range(240):
            if os.path.exists(target) and os.path.getsize(target) == len(big):
                break
            time.sleep(0.25)
        same = os.path.exists(target) and hashlib.sha256(open(target, "rb").read()).digest() == hashlib.sha256(big).digest()
        ticker.terminate()
        report["explorer_100mb"] = {"answer": answer.get("state"), "seconds": round(elapsed, 1),
                                    "frames_during": len(client.frames) - before,
                                    "longest_gap_s": round(max(gaps), 2) if gaps else None,
                                    "same_bytes": same, "clipboard_kept": clipboard_text() == original,
                                    "passed": same and len(stamps) >= elapsed and (max(gaps) if gaps else 99) < 2.0}
    finally:
        user32.PostMessageW(hwnd, 0x0010, 0, 0)

    # Over the limit: refused before anything is sent.
    answer = client.paste({}, [("huge.bin", "", b"")] * 101)
    report["too_many_refused"] = answer.get("state") == "failed"

    # Windows' clipboard history (Win+V) never saw what was pasted. Only the
    # marker is looked for; the history itself is not printed.
    history = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                              os.path.join(os.path.dirname(os.path.abspath(__file__)), "clipboard_history.ps1")],
                             capture_output=True, text=True, encoding="utf-8").stdout
    readable = history.strip().startswith("[")
    report["history_clean"] = {"readable": readable, "passed": (not readable) or MARKER not in history}


def copy_checks(client, announce, report):
    base = announce["url"].split("?")[0].rstrip("/")
    token = announce["token"]

    def fetch(item):
        with urllib.request.urlopen(f"{base}/clipboard/{item}?token={token}", timeout=60) as response:
            return response.read(), dict(response.headers)

    # Text and HTML.
    html_format = user32.RegisterClipboardFormatW("HTML Format")
    fragment = "<b>远程</b> copy"
    document = f"<html><body><!--StartFragment-->{fragment}<!--EndFragment--></body></html>"
    head = "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n"
    size = len(head.format(0, 0, 0, 0))
    raw = document.encode()
    start = size + raw.index(b"<!--StartFragment-->") + len("<!--StartFragment-->")
    end = size + raw.index(b"<!--EndFragment-->")
    cf_html = head.format(size, size + len(raw), start, end).encode() + raw + b"\0"
    set_clipboard({CF_UNICODETEXT: utf16z("远程 copy"), html_format: cf_html})
    client.send({"type": "clipboard"})
    listing = client.wait(lambda m: m["type"] == "clipboard")
    report["copy_text_html"] = {"listing": {k: listing.get(k) for k in ("text", "html", "characters")},
                                "passed": listing.get("text") == "远程 copy" and listing.get("html") == fragment}

    # A picture.
    picture = png(30, 20, 9)
    set_clipboard({user32.RegisterClipboardFormatW("PNG"): picture})
    client.send({"type": "clipboard"})
    listing = client.wait(lambda m: m["type"] == "clipboard")
    image = listing.get("image") or {}
    body, headers = fetch(image["id"]) if image.get("id") else (b"", {})
    report["copy_image"] = {"image": image, "type": headers.get("Content-Type"),
                            "passed": body == picture and headers.get("Content-Type") == "image/png"}

    # Files and a folder.
    folder = tempfile.mkdtemp(prefix="aad-copy-from-")
    one = os.path.join(folder, "报告 1.txt")
    with open(one, "wb") as f:
        f.write("内容".encode())
    sub = os.path.join(folder, "dir")
    os.makedirs(os.path.join(sub, "inner"))
    with open(os.path.join(sub, "inner", "x.bin"), "wb") as f:
        f.write(b"x" * 1000)
    paths = [one, sub]
    dropfiles = struct.pack("<IiiII", 20, 0, 0, 0, 1) + b"".join(p.encode("utf-16-le") + b"\0\0" for p in paths) + b"\0\0"
    set_clipboard({CF_HDROP: dropfiles})
    client.send({"type": "clipboard"})
    listing = client.wait(lambda m: m["type"] == "clipboard")
    files = {f["name"]: f for f in listing.get("files", [])}
    body, headers = fetch(files["报告 1.txt"]["id"])
    zipped, zip_headers = fetch(files["dir"]["id"])
    names = zipfile.ZipFile(io.BytesIO(zipped)).namelist()
    everything, _ = fetch(listing["all"]) if listing.get("all") else (b"", {})
    all_names = zipfile.ZipFile(io.BytesIO(everything)).namelist() if everything else []
    report["copy_files"] = {"files": list(files), "disposition": headers.get("Content-Disposition"),
                            "zip": names, "all": all_names,
                            "passed": body == "内容".encode() and "dir/inner/x.bin" in names
                            and "报告 1.txt" in all_names and "dir/inner/x.bin" in all_names
                            and "attachment" in (headers.get("Content-Disposition") or "")}
    # A stale id is refused.
    try:
        fetch("0" * 32)
        report["stale_id_refused"] = False
    except urllib.error.HTTPError as error:
        report["stale_id_refused"] = error.code == 404


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    report = {}
    original = f"ORIGINAL {os.getpid()} {time.time()}"
    set_clipboard({CF_UNICODETEXT: utf16z(original)})
    assist, announce = start_assist("--desktop", "--codecs", "webp", "--fps", "10", "--max-width", "960")
    try:
        client = Client(announce["websocket"])
        ready = client.ready_message()
        report["ready_clipboard"] = ready.get("clipboard")
        if "--copy" not in sys.argv[1:] or "--paste" in sys.argv[1:]:
            paste_checks(client, original, report)
        if "--copy" in sys.argv[1:]:
            copy_checks(client, announce, report)
            set_clipboard({CF_UNICODETEXT: utf16z(original)})
        client.close()
    finally:
        assist.terminate()
        try:
            assist.wait(10)
        except subprocess.TimeoutExpired:
            assist.kill()
    report["clipboard_after"] = clipboard_text() == original
    # Killed, it could not delete its scratch folder: the next run does.
    left = os.path.join(tempfile.gettempdir(), f"aad-assist-{assist.pid}")
    report["scratch_left_by_kill"] = os.path.exists(left)
    again, _ = start_assist("--desktop", "--codecs", "webp")
    time.sleep(1.0)
    again.terminate()
    report["scratch_swept_next_run"] = not os.path.exists(left)
    print(json.dumps(report, ensure_ascii=False, indent=1, default=str))
    checks = [v["passed"] for v in report.values() if isinstance(v, dict) and "passed" in v]
    ok = all(checks) and report.get("clipboard_after") and all(
        v.get("clipboard_kept", True) for v in report.values() if isinstance(v, dict))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
