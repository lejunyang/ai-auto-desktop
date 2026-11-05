"""Linux: real recorder HTTP/WS connect pending in a full loopback queue.
Build cargo build -p aad-cli --features aad-mcp/lifecycle-test-hooks.
Run with AAD_TEST_BROWSER=edge. No existing user browser is used.
"""
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer
from verify_browser_open_atomic import process_exists
from verify_mcp_browser_session import Client, Site, observe, node


def send(client, frame):
    client.process.stdin.write(json.dumps(frame) + "\n")
    client.process.stdin.flush()


def main():
    with tempfile.TemporaryDirectory(prefix="aad-pending-") as root:
        env = dict(os.environ, AAD_RECORDINGS_DIR=root,
                   AAD_CONTROL_LEASE_DIR=root + "/leases")
        browser = os.environ.get("AAD_TEST_BROWSER", "edge")
        class CountingSite(Site):
            navigation_count = 0
            def do_GET(self):
                if self.path == "/forbidden":
                    CountingSite.navigation_count += 1
                super().do_GET()
        site = ThreadingHTTPServer(("127.0.0.1", 0), CountingSite)
        threading.Thread(target=site.serve_forever, daemon=True).start()
        owner = Client(env)
        try:
            opened, _ = owner.tool("browser_open", dict(browser=browser, headless=True,
                url=f"http://127.0.0.1:{site.server_port}/login"))
            for protocol in ("http", "ws"):
                for close in (False, True):
                    with socket.socket() as listener:
                        listener.bind(("127.0.0.1", 0))
                        listener.listen(0)
                        address = listener.getsockname()
                        with socket.create_connection(address) as occupied:
                            try:
                                socket.create_connection(address, timeout=.1).close()
                            except TimeoutError:
                                pass
                            else:
                                raise AssertionError("queue was not saturated")
                            config = Path(root, f"{protocol}-{close}")
                            config.write_text(f"{protocol} {address[1]}")
                            candidate = str(config) + ".candidate"
                            client = Client(dict(env, AAD_TEST_PENDING_TCP=str(config),
                                AAD_TEST_CANDIDATE=candidate), opened["port"])
                            try:
                                old = observe(client, "Fixture login")
                                target = node(old, lambda n: n["name"] == "Sign in")["target"]
                                send(client, {"jsonrpc":"2.0","id":900,"method":"tools/call",
                                    "params":{"name":"browser_open","arguments":{
                                        "browser":browser,"headless":True,"record":{},
                                        "url":f"http://127.0.0.1:{site.server_port}/forbidden"}}})
                                deadline = time.monotonic() + 20
                                while not Path(str(config)+".pending").exists():
                                    assert time.monotonic() < deadline, "TCP never pending"
                                    time.sleep(.01)
                                pid, port = map(int, Path(candidate).read_text().split())
                                assert process_exists(pid)
                                start = time.monotonic()
                                if close:
                                    send(client, {"jsonrpc":"2.0","id":901,"method":"tools/call",
                                        "params":{"name":"control_close","arguments":{}}})
                                else:
                                    send(client, {"jsonrpc":"2.0","method":"notifications/cancelled",
                                        "params":{"requestId":900}})
                                replies = {}
                                while len(replies) < (2 if close else 1):
                                    reply = client.receive(start + 3)
                                    replies[reply["id"]] = reply
                                assert time.monotonic() - start < 3, replies
                                assert replies[900]["result"].get("isError"), replies
                                if close:
                                    assert not replies[901]["result"].get("isError"), replies
                                else:
                                    status, _ = client.tool("browser_status", {})
                                    assert status["port"] == opened["port"], status
                                    assert status["ownership"] == "external", status
                                    client.tool("control_capture", {"target":target})
                                assert not process_exists(pid), "candidate survived"
                                with socket.socket() as probe:
                                    probe.settimeout(.5)
                                    assert probe.connect_ex(("127.0.0.1",port)) != 0
                                assert CountingSite.navigation_count == 0
                                observe(owner, "Fixture login")
                            finally:
                                client.close()
        finally:
            owner.close()
            site.shutdown()
            site.server_close()
    print("PASS: HTTP/WS pending connect cancelled/closed within 3s; candidate reaped; no navigation; old session survives")


if __name__ == "__main__":
    main()
