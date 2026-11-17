#!/usr/bin/env python3
"""Check the built-in viewer's clipboard transfer from a real browser.

`aad assist --desktop` serves the viewer; a Chrome (headless by default, or
--visible) opens it twice: at http://127.0.0.1:<port> (a secure context) and
at http://<this machine's LAN address>:<port> through a small forwarder (not
a secure context, as when omem opens it by IP). For each, this machine's real
clipboard is filled and the viewer is made to paste (a trusted Ctrl+V with
the paste command, as a person's Ctrl+V); what the browser's paste event
held is recorded, and the effect is read back where it landed:

- text with HTML into Word keeps its bold;
- a screenshot-like picture (PNG and a bitmap) into Word is a picture;
- files copied as in Explorer (CF_HDROP) into an open Explorer folder.

With --copy, the other way: something is put on this machine's clipboard,
the viewer is asked for the remote clipboard (the remote is this machine),
and what the viewer then wrote to its own clipboard, offered for download
or listed is recorded.

This moves the real pointer and keyboard and opens Word and Explorer: run
it on its own. Results are printed as JSON; nothing here is a pass/fail of
the browser's own limits, those are what it reports.
"""
import ctypes
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from verify_assist import CHROME, start_assist  # noqa: E402
from verify_assist_clipboard import (CF_HDROP, CF_UNICODETEXT, Word, find_window, png, set_clipboard,  # noqa: E402
                                     user32, utf16z, clipboard_text)
from verify_assist_video import cdp  # noqa: E402


def dib(width, height):
    """A 24-bit CF_DIB, bottom-up, as a screenshot leaves next to its PNG."""
    import struct
    stride = (width * 3 + 3) & ~3
    rows = b"".join(b"".join(bytes([(x * 5) % 256, (y * 6) % 256, 200]) for x in range(width))
                    + b"\0" * (stride - width * 3) for y in range(height))
    return struct.pack("<IiiHHIIiiII", 40, width, height, 1, 24, 0, stride * height, 2835, 2835, 0, 0) + rows


def lan_address():
    for address in socket.gethostbyname_ex(socket.gethostname())[2]:
        if address.startswith(("192.168.", "10.")) or (address.startswith("172.") and 16 <= int(address.split(".")[1]) <= 31):
            return address
    raise RuntimeError("no LAN address")


def forward(listen_host, target_port):
    """A TCP forwarder from listen_host to the loopback port, as a proxy would be."""
    server = socket.socket()
    server.bind((listen_host, 0))
    server.listen(16)

    def pipe(a, b):
        try:
            while True:
                data = a.recv(65536)
                if not data:
                    break
                b.sendall(data)
        except OSError:
            pass
        finally:
            for s in (a, b):
                try:
                    s.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def accept():
        while True:
            client, _ = server.accept()
            upstream = socket.create_connection(("127.0.0.1", target_port))
            threading.Thread(target=pipe, args=(client, upstream), daemon=True).start()
            threading.Thread(target=pipe, args=(upstream, client), daemon=True).start()

    threading.Thread(target=accept, daemon=True).start()
    return server.getsockname()[1]


def chrome(url, visible):
    profile = tempfile.mkdtemp(prefix="aad-viewer-profile-")
    flags = [] if visible else ["--headless=new"]
    process = subprocess.Popen([CHROME, *flags, "--remote-debugging-port=0", f"--user-data-dir={profile}",
                                "--no-first-run", "--window-size=1000,700", url],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    active = os.path.join(profile, "DevToolsActivePort")
    port = None
    for _ in range(100):
        try:
            port = open(active).read().split()[0]
            break
        except (OSError, IndexError):
            time.sleep(0.1)
    time.sleep(1.5)
    targets = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list").read())
    return process, port, next(t["id"] for t in targets if t["type"] == "page")


def evaluate(call, expression):
    result = call("Runtime.evaluate", {"expression": expression, "returnByValue": True, "awaitPromise": True})
    return result.get("result", {}).get("value")


RECORDER = """(() => { window.__pastes = [];
document.addEventListener('paste', e => { const d = e.clipboardData;
  window.__pastes.push({types: [...d.types], files: [...d.files].map(f => ({name: f.name, type: f.type, size: f.size})),
    html: d.getData('text/html').slice(0, 200)}); }, true);
window.__status = []; new MutationObserver(() => window.__status.push(document.getElementById('transfer-text').textContent))
  .observe(document.getElementById('transfer-text'), {childList: true, characterData: true, subtree: true});
return true; })()"""


def settled_copy():
    """Here both sides are one machine: a copy here also reaches the Word
    pasted into, whose own clipboard listener is busy with it for a moment;
    pasting right then measured a plain-text paste one time in three. A copy
    on the viewer's machine never reaches the remote Word, so wait it out."""
    time.sleep(2)


def press_paste(call):
    evaluate(call, "document.getElementById('keys').focus()")
    for kind in ("keyDown", "keyUp"):
        params = {"type": kind, "key": "v", "code": "KeyV", "windowsVirtualKeyCode": 86, "modifiers": 2}
        if kind == "keyDown":
            params["commands"] = ["paste"]
        call("Input.dispatchKeyEvent", params)


def click(hwnd, fraction_y=0.5):
    user32.ShowWindow(hwnd, 9)
    user32.keybd_event(0x87, 0, 0, 0)
    user32.keybd_event(0x87, 0, 2, 0)
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.3)
    rect = (ctypes.c_long * 4)()
    user32.GetWindowRect(hwnd, rect)
    x = (rect[0] + rect[2]) // 2
    y = int(rect[1] + (rect[3] - rect[1]) * fraction_y)
    user32.SetCursorPos(x, y)
    user32.mouse_event(2, 0, 0, 0, 0)
    user32.mouse_event(4, 0, 0, 0, 0)
    time.sleep(0.4)


def foreground():
    hwnd = user32.GetForegroundWindow()
    name = ctypes.create_unicode_buffer(64)
    user32.GetClassNameW(hwnd, name, 64)
    return name.value


def wait_status(call, done, timeout=60):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        statuses = evaluate(call, "window.__status") or []
        if any(done(s) for s in statuses):
            return statuses
        time.sleep(0.2)
    return evaluate(call, "window.__status") or []


def run_origin(name, url, word, explorer_folder, explorer_hwnd, visible, report):
    process, port, page = chrome(url, visible)
    client, call = cdp(port, page)
    result = {}
    try:
        for _ in range(50):
            if evaluate(call, "typeof clip !== 'undefined' && clip !== null"):
                break
            time.sleep(0.2)
        evaluate(call, RECORDER)
        result["secure_context"] = evaluate(call, "window.isSecureContext")
        result["navigator_clipboard"] = evaluate(call, "typeof navigator.clipboard")
        result["clip"] = evaluate(call, "clip")
        finished = ("Pasted", "Not pasted", "已粘贴", "没有粘贴")

        # Text and HTML into Word.
        word.ask("clear")
        word.dismiss_popups(0.5)
        click(word.hwnd, 0.45)
        result.setdefault("foreground", []).append(foreground())
        html_format = user32.RegisterClipboardFormatW("HTML Format")
        doc = "<html><body>\r\n<!--StartFragment--><b>粗体</b> plain<!--EndFragment-->\r\n</body></html>"
        head = "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n"
        size, raw = len(head.format(0, 0, 0, 0)), doc.encode()
        start, end = size + raw.index(b"<!--StartFragment-->") + 20, size + raw.index(b"<!--EndFragment-->")
        set_clipboard({CF_UNICODETEXT: utf16z("粗体 plain"),
                       html_format: head.format(size, size + len(raw), start, end).encode() + raw + b"\0"})
        settled_copy()
        press_paste(call)
        time.sleep(3)
        read = word.ask("read")
        result["html_into_word"] = {"seen": (evaluate(call, "window.__pastes") or [None])[-1],
                                    "bold": read["bold"][:3], "text": read["text"],
                                    "passed": read["bold"][:2] == [-1, -1]}

        # A screenshot: PNG and a bitmap, as the snipping tool leaves them.
        word.ask("clear")
        word.dismiss_popups(0.5)
        click(word.hwnd, 0.45)
        result.setdefault("foreground", []).append(foreground())
        picture = png(50, 40, 5)
        set_clipboard({user32.RegisterClipboardFormatW("PNG"): picture, 8: dib(50, 40)})
        settled_copy()
        press_paste(call)
        statuses = wait_status(call, lambda s: s.startswith(finished) or s.startswith(("Not pasted", "没有粘贴")), 20)
        time.sleep(1.5)
        read = word.ask("read")
        result["picture_into_word"] = {"seen": (evaluate(call, "window.__pastes") or [None])[-1],
                                       "statuses": statuses[-3:], "shapes": read["shapes"],
                                       "passed": read["shapes"] + read["floating"] >= 1}

        # Files copied in Explorer.
        source = tempfile.mkdtemp(prefix="aad-viewer-files-")
        names = {"one.txt": "一".encode() * 1000, "two.bin": os.urandom(2_000_000)}
        paths = []
        for n, data in names.items():
            paths.append(os.path.join(source, n))
            with open(paths[-1], "wb") as f:
                f.write(data)
        import struct
        dropfiles = struct.pack("<IiiII", 20, 0, 0, 0, 1) + b"".join(p.encode("utf-16-le") + b"\0\0" for p in paths) + b"\0\0"
        set_clipboard({CF_HDROP: dropfiles, user32.RegisterClipboardFormatW("Preferred DropEffect"): struct.pack("<I", 1)})
        target = os.path.join(explorer_folder, name)
        os.makedirs(target, exist_ok=True)
        click(explorer_hwnd, 0.7)
        settled_copy()
        press_paste(call)
        statuses = wait_status(call, lambda s: s.startswith(finished), 60)
        landed = []
        for _ in range(40):
            landed = sorted(n for n in os.listdir(explorer_folder) if n in names)
            if len(landed) == len(names):
                break
            time.sleep(0.25)
        same = all(os.path.exists(os.path.join(explorer_folder, n)) and
                   open(os.path.join(explorer_folder, n), "rb").read() == data for n, data in names.items())
        for n in names:
            try:
                os.remove(os.path.join(explorer_folder, n))
            except OSError:
                pass
        result["explorer_files"] = {"seen": (evaluate(call, "window.__pastes") or [None])[-1],
                                    "statuses": statuses[-3:], "landed": landed, "passed": same}
    finally:
        client.close()
        process.terminate()
    report[name] = result


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    visible = "--visible" in sys.argv[1:]
    report = {}
    original = f"ORIGINAL viewer {os.getpid()}"
    assist, announce = start_assist("--desktop", "--codecs", "webp", "--fps", "5", "--max-width", "800")
    try:
        port = int(announce["url"].split(":")[2].split("/")[0])
        lan = lan_address()
        lan_port = forward(lan, port)
        word = Word()
        folder = tempfile.mkdtemp(prefix="aad-viewer-into-")
        explorer = subprocess.Popen(["explorer.exe", folder])
        explorer_hwnd = find_window("CabinetWClass", os.path.basename(folder))
        try:
            for name, host in (("loopback", f"127.0.0.1:{port}"), ("lan", f"{lan}:{lan_port}")):
                url = announce["url"].replace(f"127.0.0.1:{port}", host)
                set_clipboard({CF_UNICODETEXT: utf16z(original)})
                run_origin(name, url, word, folder, explorer_hwnd, visible, report)
        finally:
            word.quit()
            user32.PostMessageW(explorer_hwnd, 0x0010, 0, 0)
    finally:
        assist.terminate()
    set_clipboard({CF_UNICODETEXT: utf16z(original)})
    print(json.dumps(report, ensure_ascii=False, indent=1, default=str))


if __name__ == "__main__":
    main()
