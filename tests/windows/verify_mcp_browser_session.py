#!/usr/bin/env python3
"""Drive a browser through `aad mcp`: open it, record, screenshot, act, read the network.

A local fixture site stands in for a site with a QR login: the code on the
login page changes every second, and signing in loads data from a JSON API.
An MCP client (this script, standing in for an agent):

1. opens a browser with a temporary profile and recording on, at the login page;
2. screenshots the QR code twice and checks that the digest changed;
3. presses "sign in", observes the signed-in page and reads its data;
4. stops the recording and checks the API request and its response body are in it.

Needs Chrome, Edge or Brave. AAD_TEST_BROWSER picks one (default chrome).
"""
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
AAD = os.path.join(REPO, "target", "debug", "aad.exe" if os.name == "nt" else "aad")

LOGIN = """<!doctype html><title>Fixture login</title><body>
<h1>Scan to sign in</h1>
<img id="qr" alt="login code" width="120" height="120">
<button id="go" onclick="location.href='/home'">Sign in</button>
<script>
let n = 0;
function draw() {
  n += 1;
  const cells = [];
  for (let i = 0; i < 36; i++) if ((i * 7 + n) % 3 === 0) cells.push(`<rect x="${(i % 6) * 20}" y="${Math.floor(i / 6) * 20}" width="20" height="20"/>`);
  document.getElementById('qr').src = 'data:image/svg+xml,' + encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" width="120" height="120">${cells.join('')}</svg>`);
}
draw(); setInterval(draw, 1000);
</script></body>"""

HOME = """<!doctype html><title>Fixture orders</title><body>
<h1>Orders</h1><ul id="orders"></ul>
<script>
fetch('/api/orders?page=1').then(r => r.json()).then(data => {
  for (const order of data.orders) {
    const li = document.createElement('li'); li.textContent = order.id + ' ' + order.total;
    document.getElementById('orders').appendChild(li);
  }
});
</script></body>"""


class Site(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/api/orders"):
            body, kind = json.dumps({"orders": [{"id": "A-1", "total": 42}, {"id": "A-2", "total": 7}]}), "application/json"
        elif self.path.startswith("/home"):
            body, kind = HOME, "text/html; charset=utf-8"
        else:
            body, kind = LOGIN, "text/html; charset=utf-8"
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


class Client:
    def __init__(self, env):
        self.process = subprocess.Popen([AAD, "mcp", "--allow-input"], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, text=True, encoding="utf-8", env=env)
        self.next = 0
        self.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                    "clientInfo": {"name": "probe", "version": "0"}})

    def request(self, method, params):
        self.next += 1
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}) + "\n")
        self.process.stdin.flush()
        while True:
            answer = json.loads(self.process.stdout.readline())
            if answer.get("id") == self.next:
                return answer

    def tool(self, name, arguments):
        result = self.request("tools/call", {"name": name, "arguments": arguments})["result"]
        texts = [c["text"] for c in result["content"] if c["type"] == "text"]
        images = [c for c in result["content"] if c["type"] == "image"]
        payload = json.loads(texts[0]) if texts else {}
        if result.get("isError"):
            raise RuntimeError(f"{name}: {json.dumps(payload, ensure_ascii=False)}")
        return payload, images

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=60)


def observe(client, title):
    for _ in range(40):
        targets, _ = client.tool("control_targets", {})
        page = [r for r in targets["data"]["resources"]
                if r["provider"] == "browser.cdp" and r["name"] == title and r["metadata"].get("main_frame")]
        if page:
            observed, _ = client.tool("control_observe", {"resource_id": page[0]["resource_id"]})
            return observed["data"]
        time.sleep(0.25)
    raise RuntimeError(f"page {title!r} never appeared")


def node(observation, predicate):
    return next(n for n in observation["nodes"] if predicate(n))


def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    site = f"http://127.0.0.1:{server.server_address[1]}"
    env = dict(os.environ, AAD_RECORDINGS_DIR=tempfile.mkdtemp(prefix="aad-browser-store-"))
    client = Client(env)
    findings = {}
    try:
        opened, _ = client.tool("browser_open", {
            "browser": os.environ.get("AAD_TEST_BROWSER", "chrome"), "url": site + "/login",
            "headless": os.environ.get("AAD_TEST_HEADED") is None, "record": {"body_limit": 4096}})
        findings["opened"] = {"port": opened["port"], "recording": "recording" in opened}

        login = observe(client, "Fixture login")
        qr = node(login, lambda n: n["name"] == "login code")
        first, images = client.tool("control_capture", {"target": qr["target"]})
        time.sleep(1.5)
        login = observe(client, "Fixture login")
        qr = node(login, lambda n: n["name"] == "login code")
        second, _ = client.tool("control_capture", {"target": qr["target"]})
        findings["qr"] = {"image_returned": bool(images and images[0]["mimeType"] == "image/png"),
                          "size": [first["data"]["width"], first["data"]["height"]],
                          "digest_changed": first["data"]["digest"] != second["data"]["digest"]}

        button = node(login, lambda n: n["name"] == "Sign in")
        client.tool("control_act", {"target": button["target"], "action": {"type": "click"}})
        orders = None
        for _ in range(20):
            home = observe(client, "Fixture orders")
            orders = [n["name"] for n in home["nodes"] if n["role"] == "listitem"]
            if orders:
                break
            time.sleep(0.25)
        findings["page_data"] = orders

        stopped, _ = client.tool("browser_record_stop", {})
        network = os.path.join(stopped["out"], "evidence", "network.jsonl")
        records = [json.loads(line) for line in open(network, encoding="utf-8")] if os.path.exists(network) else []
        api = [r for r in records if "/api/orders" in json.dumps(r)]
        findings["recording"] = {"files": stopped["files"], "network_records": len(records),
                                 "api_recorded": bool(api),
                                 "api_body_kept": any("A-1" in json.dumps(r) for r in api)}
        client.tool("browser_close", {})
    finally:
        client.close()
        server.shutdown()
    findings["passed"] = (findings.get("qr", {}).get("image_returned") and findings["qr"]["digest_changed"]
                          and findings.get("page_data") and findings["recording"]["api_body_kept"])
    print(json.dumps(findings, ensure_ascii=False, indent=1))
    return 0 if findings["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
