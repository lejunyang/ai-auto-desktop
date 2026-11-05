"""Real read-only CLI diagnostics using an isolated headless Edge fixture."""
import json
import socket
import select
import urllib.request
from http.server import BaseHTTPRequestHandler
import os
import subprocess
import tempfile
import time
import verify_mcp_browser_session as fixture
import threading
from http.server import ThreadingHTTPServer
from verify_mcp_browser_session import Client, Site, AAD

def main():
    fixture.LOGIN += "<script>setInterval(()=>history.replaceState({}, '', '/login?ticket=secret-ticket'),100)</script>"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory() as root:
        owner = Client(dict(os.environ, AAD_RECORDINGS_DIR=root))
        process = None
        try:
            opened, _ = owner.tool("browser_open", {"browser":"edge","headless":True,
                "url":f"http://127.0.0.1:{server.server_port}/login"})
            class Proxy(BaseHTTPRequestHandler):
                protocol_version = "HTTP/1.1"
                pairs = []
                def do_GET(self):
                    if self.headers.get("Upgrade", "").lower() != "websocket":
                        with urllib.request.urlopen(f"http://127.0.0.1:{opened['port']}{self.path}") as response:
                            body = response.read().replace(
                                f"127.0.0.1:{opened['port']}".encode(),
                                f"127.0.0.1:{proxy.server_port}".encode())
                        self.send_response(200)
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                        return
                    upstream = socket.create_connection(("127.0.0.1",opened["port"]))
                    try:
                        upstream.sendall((f"GET {self.path} HTTP/1.1\r\n" +
                            "".join(f"{k}: {v}\r\n" for k,v in self.headers.items())+"\r\n").encode())
                        self.pairs.append((self.connection,upstream))
                        while True:
                            ready, _, _ = select.select([self.connection,upstream],[],[],2)
                            for source in ready:
                                data = source.recv(65536)
                                if not data:
                                    return
                                (upstream if source is self.connection else self.connection).sendall(data)
                    except OSError:
                        pass
                    finally:
                        upstream.close()
                def log_message(self, *args):
                    pass
            proxy = ThreadingHTTPServer(("127.0.0.1",0),Proxy)
            threading.Thread(target=proxy.serve_forever,daemon=True).start()
            process = subprocess.Popen([AAD,"control","diagnostics","--browser-port",str(proxy.server_port)],
                stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
            session = None
            count = 0
            def request(operation):
                nonlocal session, count
                count += 1
                process.stdin.write(json.dumps({"request_id":str(count),"session_id":session,
                    "timeout_ms":3000,"operation":operation})+"\n")
                process.stdin.flush()
                reply = json.loads(process.stdout.readline())
                session = reply["session_id"]
                return reply
            capabilities = request({"method":"capabilities"})
            assert not capabilities["data"]["allow_input"]
            targets = request({"method":"targets"})["data"]
            assert any(r["provider"] == "browser.cdp" for r in targets["resources"]), targets["unavailable"]
            resource = next(r for r in targets["resources"] if r["provider"] == "browser.cdp")
            args = {"method":"browser_network","resource_id":resource["resource_id"],"limit":1}
            data = request(args)["data"]
            assert len(data["requests"]) <= 1
            assert "summary" in data and "since" in data
            assert all(k in data for k in ("evicted_requests","dropped_events","omitted_by_limit"))
            time.sleep(.4)
            data = request(args)["data"]
            timeline = data["timeline"]
            navigation = [e for e in timeline["events"] if e["kind"] == "navigation"]
            assert navigation, timeline
            assert "secret-ticket" not in json.dumps(timeline)
            assert timeline["generation"] == 1
            assert timeline["events"][0]["details"]["pre_attach_history_available"] is False
            page = fixture.observe(owner, "Fixture login")
            button = next(n["target"] for n in page["nodes"] if n["name"] == "Sign in")
            owner.tool("control_act", {"target":button,"action":{"type":"click"}})
            time.sleep(.3)
            data = request(args)["data"]
            assert any(e["kind"] == "navigation" and e["details"]["method"] == "Page.frameNavigated"
                for e in data["timeline"]["events"]), data
            for pair in list(Proxy.pairs):
                for connection in pair:
                    try:
                        connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
            # Pure diagnostics polling must recover without an observe command.
            time.sleep(.1)
            for _ in range(40):
                data = request(args)["data"]
            assert data["timeline"]["generation"] == 2, data
            kinds = [e["kind"] for e in data["timeline"]["events"]]
            assert kinds.count("connection_failure") == 1, kinds
            assert kinds.count("reconnect_gap") == 1, kinds
            assert "navigation" in kinds
            for limit in (0,501):
                reply = request(dict(args,limit=limit))
                assert reply["error"]["code"] == "CONTROL.INVALID_REQUEST"
            request({"method":"close"})
            process.stdin.close()
            process.wait(timeout=5)
            proxy.shutdown()
            proxy.server_close()
        finally:
            if process and process.poll() is None:
                process.kill()
                process.wait()
            owner.close()
            server.shutdown()
            server.server_close()
    print("PASS: real CLI persistent diagnostics, bounded limit, read-only policy")

if __name__ == "__main__":
    main()
