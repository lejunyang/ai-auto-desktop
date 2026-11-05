"""Deterministic real CDP final-body stop regression; lifecycle-test-hooks required."""
import http.server
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import verify_mcp_browser_session as fixture


def wait(path):
    deadline = time.monotonic() + 10
    while not path.exists():
        assert time.monotonic() < deadline, str(path)
        time.sleep(.005)


def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), fixture.Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        for release, close in ((True, False), (False, False), (False, True), (False, "cancel")):
            with tempfile.TemporaryDirectory(prefix="aad-stop-body-") as root:
                gate = Path(root) / "body"
                client = fixture.Client(dict(os.environ, AAD_RECORDINGS_DIR=root,
                    AAD_TEST_BODY_GATE=str(gate)))
                try:
                    opened, _ = client.tool("browser_open", {"browser": "edge", "headless": True,
                        "url": f"http://127.0.0.1:{server.server_port}/login", "record": {"body_limit": 4096}})
                    page = fixture.observe(client, "Fixture login")
                    button = fixture.node(page, lambda n: n["name"] == "Sign in")
                    client.tool("control_act", {"target": button["target"], "action": {"type": "click"}})
                    wait(gate)
                    def unblock():
                        wait(Path(str(gate) + ".stop"))
                        Path(str(gate) + ".release").touch()
                    helper = threading.Thread(target=unblock) if release else None
                    if helper:
                        helper.start()
                    started = time.monotonic()
                    if close == "cancel":
                        client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":900,"method":"tools/call","params":{"name":"browser_record_stop","arguments":{}}})+"\n")
                        client.process.stdin.flush()
                        wait(Path(str(gate)+".stop"))
                        client.process.stdin.write(json.dumps({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":900}})+"\n")
                        client.process.stdin.flush()
                        answer=client.receive(time.monotonic()+1)
                        stopped=json.loads(answer["result"]["content"][0]["text"])
                        assert time.monotonic()-started < 1
                    else:
                        stopped, _ = client.tool("browser_close" if close else "browser_record_stop", {})
                    if close is True:
                        stopped = stopped["recording"]
                    elapsed = time.monotonic() - started
                    if helper:
                        helper.join()
                    out = Path(stopped["out"])
                    manifest = json.loads((out / "manifest.json").read_text())
                    records = [json.loads(line) for line in (out / "evidence/network.jsonl").read_text().splitlines()]
                    api = [r for r in records if "/api/orders" in json.dumps(r)]
                    assert api, records
                    if release:
                        assert elapsed < 2, elapsed
                        assert any("A-1" in r.get("body", "") for r in api), api
                        assert manifest["status"] == "complete" and manifest["error"] is None, manifest
                        assert stopped["summary"]["recorded"] is True, stopped
                    else:
                        assert elapsed < 4, elapsed
                        if not close:
                            assert elapsed >= 1.8, elapsed
                        assert manifest["status"] == "incomplete", manifest
                        reason = "stop request" if close == "cancel" else "shutdown" if close else "deadline"
                        assert reason in manifest["error"], manifest
                        assert stopped["summary"]["status"] == "incomplete", stopped
                        assert reason in stopped["summary"]["error"], stopped
                        assert any(r.get("body_status") == "unavailable" for r in api), api
                    print(json.dumps({"release": release, "elapsed": elapsed, "status": manifest["status"]}))
                finally:
                    client.close()
    finally:
        server.shutdown()


if __name__ == "__main__":
    main()
