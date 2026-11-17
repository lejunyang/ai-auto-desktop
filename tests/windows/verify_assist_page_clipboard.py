#!/usr/bin/env python3
"""Check clipboard transfer through `aad assist --browser-port` by its effect.

Headless Chrome shows a fixture page; nothing on the real desktop is touched.
A minimal viewer pastes the way the built-in viewer does, and the page is
read back over its own CDP connection:

- text with HTML into a contenteditable: a real paste (the page's paste
  event holds text/plain and text/html) and the bold arrives;
- a picture: the paste event holds it as an image/png file;
- a PDF: a synthetic paste event carries it as a File;
- a focused <input type=file> gets the files;
- more than the page limit is refused;
- copy back: a selection with bold comes back as text and HTML.
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import verify_assist_input as fixture  # noqa: E402
from verify_assist import start_assist  # noqa: E402
from verify_assist_clipboard import Client, png  # noqa: E402
from verify_assist_video import cdp  # noqa: E402

fixture.PAGE = b"""<!doctype html><title>Assist clipboard fixture</title>
<div id=e contenteditable style='width:400px;height:120px;border:1px solid'></div>
<p id=s>copy <b>this bold</b> text</p>
<input id=f type=file multiple>
<script>window.log=[];
document.addEventListener('paste',e=>{const d=e.clipboardData;
  log.push({types:[...d.types],files:[...d.files].map(f=>({name:f.name,type:f.type,size:f.size})),trusted:e.isTrusted});},true);
</script>"""


def evaluate(call, expression):
    return call("Runtime.evaluate", {"expression": expression, "returnByValue": True,
                                     "awaitPromise": True}).get("result", {}).get("value")


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    server = fixture.serve_page()
    url = f"http://127.0.0.1:{server.server_address[1]}/"
    process, port, page = fixture.chrome(url)
    report = {}
    try:
        _, call = cdp(port, page)
        assist, announce = start_assist("--browser-port", str(port), "--page", page, "--codecs", "webp")
        try:
            client = Client(announce["websocket"])
            report["ready_clipboard"] = client.ready_message().get("clipboard")

            evaluate(call, "document.getElementById('e').focus(); log.length = 0")
            answer = client.paste({"text/plain": "粗体 plain", "text/html": "<b>粗体</b> plain"})
            time.sleep(0.5)
            report["html"] = {"answer": answer, "inner": evaluate(call, "document.getElementById('e').innerHTML"),
                              "event": evaluate(call, "log[0]")}
            report["html"]["passed"] = "<b>粗体</b>" in (report["html"]["inner"] or "") and \
                report["html"]["event"]["trusted"]

            evaluate(call, "document.getElementById('e').innerHTML = ''; log.length = 0")
            answer = client.paste({}, [("shot.png", "image/png", png(20, 10))])
            time.sleep(0.5)
            event = evaluate(call, "log[0]")
            report["picture"] = {"answer": answer, "event": event,
                                 "passed": bool(event) and any(f["type"] == "image/png" for f in event["files"])}

            evaluate(call, "log.length = 0")
            pdf = b"%PDF-1.4 fixture"
            answer = client.paste({"text/plain": "with a file"}, [("doc.pdf", "application/pdf", pdf)])
            time.sleep(0.5)
            event = evaluate(call, "log[0]")
            report["pdf"] = {"answer": answer, "event": event,
                             "passed": bool(event) and event["files"] == [{"name": "doc.pdf", "type": "application/pdf",
                                                                           "size": len(pdf)}]}

            evaluate(call, "document.getElementById('f').focus()")
            answer = client.paste({}, [("a.txt", "text/plain", b"a"), ("b.txt", "text/plain", b"bb")])
            time.sleep(0.5)
            names = evaluate(call, "[...document.getElementById('f').files].map(f => f.name + ':' + f.size)")
            report["file_input"] = {"answer": answer, "files": names, "passed": names == ["a.txt:1", "b.txt:2"]}

            evaluate(call, "document.getElementById('e').focus()")
            answer = client.paste({}, [("big.bin", "", os.urandom(21 * 1024 * 1024))])
            report["over_page_limit"] = {"answer": answer.get("message"), "passed": answer.get("state") == "failed"}

            evaluate(call, "(() => { const r = document.createRange(); r.selectNodeContents(document.getElementById('s'));"
                           " const s = getSelection(); s.removeAllRanges(); s.addRange(r); document.getElementById('s').focus(); })()")
            client.send({"type": "clipboard"})
            listing = client.wait(lambda m: m["type"] == "clipboard")
            report["copy"] = {"text": listing.get("text"), "html": listing.get("html"),
                              "passed": listing.get("text") == "copy this bold text" and "this bold</b>" in (listing.get("html") or "")}
            client.close()
        finally:
            assist.terminate()
    finally:
        process.terminate()
    print(json.dumps(report, ensure_ascii=False, indent=1))
    sys.exit(0 if all(v["passed"] for v in report.values() if isinstance(v, dict) and "passed" in v) else 1)


if __name__ == "__main__":
    main()
