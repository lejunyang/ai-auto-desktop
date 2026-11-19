#!/usr/bin/env python3
"""Paste into Chrome through `aad assist --desktop` on X11 while other
programs fight over CLIPBOARD, by effect.

A real KDE Plasma desktop reached over xrdp has xrdp-chansrv and Klipper
watching CLIPBOARD; they read every new owner's TARGETS and may take the
selection themselves. This runs a private Xvfb with openbox, a Chrome window
whose page shows what lands in its text box in the window title, and
x11_clipboard_grabber.c in each of its modes (taking CLIPBOARD after every
owner change, once at a given moment as a synced copy from another device
would, back after every loss, or as Klipper does), and pastes text and HTML.

  CHROME  the Chrome (or Chromium / Edge) to run (default ~/cft/chrome-linux64/chrome)
  AAD     the aad binary (default target/debug/aad)

Needs Xvfb, openbox, xdotool and a C compiler with libX11 / libXfixes headers.
Prints a JSON report; exits 1 when a paste did not arrive.
"""
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from verify_assist_clipboard_x11 import Client, run  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
AAD = os.environ.get("AAD", os.path.join(REPO, "target", "debug", "aad"))
CHROME = os.environ.get("CHROME", os.path.expanduser("~/cft/chrome-linux64/chrome"))
DISPLAY = ":79"

PAGE = ("<title>empty</title><textarea id=t autofocus style='width:90vw;height:80vh'></textarea><script>"
        "const t=document.getElementById('t');"
        "t.addEventListener('input',()=>{document.title='V:'+JSON.stringify(t.value)});"
        "t.addEventListener('paste',e=>{document.title='P:'+JSON.stringify(e.clipboardData.types)});"
        "</script>")


def title(env, window):
    return run("xdotool", "getwindowname", str(window), env=env).stdout.strip()


def main():
    env = dict(os.environ, DISPLAY=DISPLAY)
    work = tempfile.mkdtemp(prefix="aad-x11-contend-")
    grabber_bin = os.path.join(work, "grabber")
    built = run("cc", "-o", grabber_bin, os.path.join(HERE, "x11_clipboard_grabber.c"), "-lX11", "-lXfixes")
    if built.returncode:
        raise SystemExit(built.stderr)
    xvfb = subprocess.Popen(["Xvfb", DISPLAY, "-screen", "0", "1280x800x24", "-nolisten", "tcp"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)
    wm = subprocess.Popen(["openbox"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)
    chrome = subprocess.Popen([CHROME, "--no-sandbox", "--no-first-run", "--disable-gpu", "--password-store=basic",
                               f"--user-data-dir={work}/profile", "--window-size=1000,700",
                               "--app=data:text/html," + urllib.parse.quote(PAGE)],
                              env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    report = {"cases": []}
    assist = None
    try:
        window = None
        for _ in range(60):
            found = run("xdotool", "search", "--name", "^empty$", env=env).stdout.split()
            if found:
                window = found[0]
                break
            time.sleep(0.5)
        if not window:
            raise SystemExit("Chrome did not open")
        assist = subprocess.Popen([AAD, "--compact", "assist", "--desktop", "--codecs", "webp"], env=env,
                                  stdout=subprocess.PIPE, stderr=open(os.path.join(work, "assist.log"), "w"),
                                  text=True)
        announce = json.loads(assist.stdout.readline())
        client = Client(announce["websocket"])
        client.wait(lambda m: m["type"] == "ready")
        # (name, grabber arguments, whether the text is typed instead): a
        # program that keeps taking CLIPBOARD within the settling time leaves
        # typing; one that takes it once, or late, does not.
        cases = [("none", [], False), ("klipper", ["klipper"], False),
                 ("chansrv now", ["chansrv", "0"], True), ("chansrv 50 ms", ["chansrv", "50"], True),
                 ("chansrv 300 ms", ["chansrv", "300"], False), ("chansrv once", ["chansrv", "0", "1"], False),
                 # One take at a moment of its own (a copy synced in from another
                 # device, as CrossPaste writes it): while the paste settles, and
                 # after the key.
                 ("once at 100 ms", ["chansrv", "100", "1"], False),
                 ("once at 250 ms", ["chansrv", "250", "1"], False),
                 # A manager that takes it back after every SelectionClear and
                 # ignores the password manager marker.
                 ("retake now", ["retake", "0"], True), ("retake 20 ms", ["retake", "20"], True),
                 ("retake 100 ms", ["retake", "100"], True), ("retake 400 ms", ["retake", "400"], False)]
        for name, grabber_args, typed in cases:
            for kind in ("text", "html"):
                grabber = None
                log = queue.Queue()
                if grabber_args:
                    grabber = subprocess.Popen([grabber_bin, *grabber_args], env=env, stdout=subprocess.PIPE,
                                               text=True)
                    threading.Thread(target=lambda g=grabber: [log.put(line.rstrip()) for line in g.stdout],
                                     daemon=True).start()
                    time.sleep(0.5)
                run("xdotool", "windowactivate", "--sync", window, env=env)
                time.sleep(0.3)
                run("xdotool", "key", "--window", window, "ctrl+a", "BackSpace", env=env)
                time.sleep(0.3)
                sent = f"{kind} 粘贴 {name}"
                formats = {"text/plain": sent}
                if kind == "html":
                    formats["text/html"] = f"<b>{sent}</b>"
                print(f"case {name} / {kind}", file=sys.stderr, flush=True)
                started = time.time()
                try:
                    answer = client.paste(formats)
                except TimeoutError:
                    answer = {"state": "no answer in 20 s"}
                answer["seconds"] = round(time.time() - started, 2)
                time.sleep(0.8)
                seen = title(env, window)
                if grabber:
                    grabber.terminate()
                    grabber.wait()
                lines = []
                while not log.empty():
                    lines.append(log.get())
                arrived = seen == "V:" + json.dumps(sent, ensure_ascii=False)
                arrived = arrived and bool(answer.get("typed")) == typed
                report["cases"].append({"case": name, "kind": kind, "answer": answer, "title": seen,
                                        "passed": arrived, "grabber": lines})
        client.close()
    finally:
        if assist:
            assist.terminate()
        chrome.terminate()
        wm.terminate()
        xvfb.terminate()
        try:
            report["assist_log"] = open(os.path.join(work, "assist.log")).read()[-3000:]
        except OSError:
            pass
        shutil.rmtree(work, ignore_errors=True)
    print(json.dumps(report, ensure_ascii=False, indent=1))
    sys.exit(0 if all(c["passed"] for c in report["cases"]) else 1)


if __name__ == "__main__":
    main()
