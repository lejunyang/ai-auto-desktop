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
Install tests/windows/requirements.txt for identity-aware descendant cleanup.
"""
import http.server
import json
import queue
import signal
import os
import subprocess
import sys
import tempfile
import threading
import time
import psutil

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
    def __init__(self, env, port=None):
        self.candidate_directory = tempfile.TemporaryDirectory(prefix="aad-candidate-tracking-")
        self.candidate_log = os.path.join(self.candidate_directory.name, "pids")
        env = dict(env, AAD_TEST_CANDIDATE_LOG=self.candidate_log)
        self.process = subprocess.Popen([AAD, "mcp", "--allow-input"] + (["--browser-port", str(port)] if port else []), stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, text=True, encoding="utf-8", env=env,
                                        start_new_session=os.name != "nt")
        self.next = 0
        self.tracked = {}
        self.candidates_seen = set()
        self.tracking_stop = threading.Event()
        def track():
            while not self.tracking_stop.wait(0.01):
                self.track_candidates()
                self.track_descendants()
                try:
                    for child in psutil.Process(self.process.pid).children(recursive=True):
                        self.tracked[(child.pid, child.create_time())] = child
                except psutil.Error:
                    pass
        self.tracker = threading.Thread(target=track, daemon=True)
        self.tracker.start()
        self.responses = queue.Queue()
        def read_responses():
            try:
                for line in self.process.stdout:
                    self.responses.put(line)
            finally:
                self.responses.put(None)
        self.reader = threading.Thread(target=read_responses, daemon=True)
        self.reader.start()
        try:
            self.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                        "clientInfo": {"name": "probe", "version": "0"}})
        except BaseException:
            self.close()
            raise

    def track_descendants(self):
        # Keep discovering from known identities after MCP exits and the
        # independently grouped browser candidates are reparented.
        for root in list(self.tracked.values()):
            try:
                if root.is_running():
                    for child in root.children(recursive=True):
                        self.tracked[(child.pid, child.create_time())] = child
            except psutil.NoSuchProcess:
                pass

    def freeze_descendants(self):
        # Suspend each parent BEFORE enumerating its children. Children may
        # still fork while we walk, so apply the same rule recursively. Once
        # this finishes no discovered process can create an untracked orphan
        # between enumeration and termination. Process methods check identity.
        pending = list(self.tracked.values())
        frozen = set()
        while pending:
            root = pending.pop()
            identity = (root.pid, root.create_time())
            if identity in frozen:
                continue
            frozen.add(identity)
            try:
                root.suspend()
                for child in root.children():
                    self.tracked[(child.pid, child.create_time())] = child
                    pending.append(child)
            except psutil.NoSuchProcess:
                pass

    def track_candidates(self):
        path = getattr(self, "candidate_log", None)
        if not path or not os.path.exists(path):
            return
        with open(path) as source:
            for line in source:
                try:
                    pid = int(line)
                    if pid in self.candidates_seen:
                        continue
                    self.candidates_seen.add(pid)
                    child = psutil.Process(pid)
                    self.tracked[(child.pid, child.create_time())] = child
                except (psutil.Error, ValueError):
                    pass

    def request(self, method, params):
        self.next += 1
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}) + "\n")
        self.process.stdin.flush()
        deadline = time.monotonic() + 60
        while True:
            answer = self.receive(deadline)
            if answer.get("id") == self.next:
                return answer

    def receive(self, deadline):
        try:
            line = self.responses.get(timeout=max(0, deadline - time.monotonic()))
        except queue.Empty as exc:
            raise TimeoutError("MCP response deadline exceeded") from exc
        if line is None:
            raise EOFError("MCP stdout closed before response")
        return json.loads(line)

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
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            if os.name == "nt":
                subprocess.run(["taskkill", "/PID", str(self.process.pid), "/T", "/F"],
                               capture_output=True, timeout=5, check=True)
            else:
                # This process group belongs exclusively to this fixture.
                os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=5)
        self.tracking_stop.set()
        self.tracker.join(timeout=1)
        self.track_candidates()
        self.freeze_descendants()
        # Browser candidates create their own process groups. Process objects
        # retain creation identity and guard against killing a reused PID.
        tracked = list(self.tracked.values())
        for child in reversed(tracked):
            try:
                child.kill()
            except psutil.NoSuchProcess:
                pass
        _, alive = psutil.wait_procs(tracked, timeout=3)
        self.reader.join(timeout=3)
        if self.reader.is_alive():
            # Never close a TextIOWrapper while its reader holds the file lock.
            raise TimeoutError("fixture stdout reader did not terminate after process cleanup")
        self.process.stdout.close()
        if hasattr(self, "candidate_directory"):
            self.candidate_directory.cleanup()
        if any(p.status() != psutil.STATUS_ZOMBIE for p in alive if p.is_running()):
            raise TimeoutError("fixture descendants survived cleanup")


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

def assert_detached_references(client, observation):
    target = next(n["target"] for n in observation["nodes"] if n.get("target"))
    for name, arguments, code in [
        ("control_find", {"observation_id": target["observation_id"], "locator": {"name": "Sign in"}}, "CONTROL.STALE_TARGET"),
        ("control_capture", {"target": target}, "CONTROL.STALE_TARGET"),
        ("control_act", {"target": target, "action": {"type": "click"}}, "CONTROL.STALE_TARGET"),
        ("control_observe", {"resource_id": target["resource_id"]}, "CONTROL.TARGET_UNKNOWN"),
    ]:
        result = client.request("tools/call", {"name": name, "arguments": arguments})["result"]
        assert result.get("isError"), result
        payload = json.loads(next(c["text"] for c in result["content"] if c["type"] == "text"))
        assert code in json.dumps(payload), payload
        assert payload["effect"] == "not_applied", payload


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
        status, _ = client.tool("browser_status", {})
        assert status["ownership"] == "tool", status
        assert status["profile_mode"] == "temporary", status
        assert status["process_id"] > 0, status
        assert status["port"] == opened["port"], status

        # A second server owns only its attachment, never this browser process.
        attached = Client(env, opened["port"])
        try:
            assert attached.tool("browser_status", {})[0]["ownership"] == "external"
            attached_observation = observe(attached, "Fixture login")
            closed, _ = attached.tool("browser_close", {})
            assert_detached_references(attached, attached_observation)
            assert closed["closed"] is False, closed
            detached, _ = attached.tool("browser_status", {})
            assert detached["ownership"] == "none" and detached["port"] is None, detached
            targets, _ = attached.tool("control_targets", {})
            assert not any(r["provider"] == "browser.cdp" for r in targets["data"]["resources"])
            observe(client, "Fixture login")
        finally:
            attached.close()

        # A replacement must not close the original browser or its recorder.
        for replacement, code in [
            ({"url": "file:///invalid"}, "MCP.INVALID_ARGUMENT"),
            ({"url": site + "/home"}, "BROWSER.ALREADY_OPEN"),
            ({"profile_dir": "/nonexistent/aad-profile"}, "BROWSER.ALREADY_OPEN"),
        ]:
            try:
                client.tool("browser_open", dict(
                    browser=os.environ.get("AAD_TEST_BROWSER", "chrome"),
                    headless=True, **replacement))
            except RuntimeError as error:
                assert code in str(error), str(error)
            else:
                raise AssertionError("replacement unexpectedly succeeded")
            observe(client, "Fixture login")
        findings["replacement_preserved"] = True

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
        with open(os.path.join(stopped["out"], "manifest.json"), encoding="utf-8") as stream:
            manifest = json.load(stream)
        assert manifest["status"] == "complete", manifest
        assert manifest["error"] is None, manifest
        network = os.path.join(stopped["out"], "evidence", "network.jsonl")
        records = [json.loads(line) for line in open(network, encoding="utf-8")] if os.path.exists(network) else []
        api = [r for r in records if "/api/orders" in json.dumps(r)]
        findings["recording"] = {"files": stopped["files"], "network_records": len(records),
                                 "api_recorded": bool(api),
                                 "api_body_kept": any("A-1" in json.dumps(r) for r in api)}
        before_close = observe(client, "Fixture orders")
        client.tool("browser_close", {})
        assert_detached_references(client, before_close)
        status, _ = client.tool("browser_status", {})
        assert status["connection"] == "not_configured", status
        assert status["ownership"] == "none" and status["port"] is None, status
        assert status["profile_mode"] is None and status["process_id"] is None, status
        targets, _ = client.tool("control_targets", {})
        assert not any(r["provider"] == "browser.cdp" for r in targets["data"]["resources"])
    finally:
        client.close()
        server.shutdown()
    findings["passed"] = (findings.get("qr", {}).get("image_returned") and findings["qr"]["digest_changed"]
                          and findings.get("page_data") and findings["recording"]["api_body_kept"])
    print(json.dumps(findings, ensure_ascii=False, indent=1))
    return 0 if findings["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
